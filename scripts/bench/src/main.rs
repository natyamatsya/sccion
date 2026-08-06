// SPDX-FileCopyrightText: Copyright natyamatsya and sccache contributors
// SPDX-License-Identifier: Apache-2.0

//! Benchmark sccache against a CMake + Ninja MSVC build.
//!
//! Three scenarios -- cold, fully cached, and one-hot-header-touched -- reported
//! as wall clock plus sccache's own counters, written to JSON so runs can be
//! diffed across commits.
//!
//! See `README.md` for what the scenarios mean and how to read the numbers.

mod env;
mod proc;
mod project;
mod stats;
mod telemetry;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::Parser;
use serde::Serialize;

use env::{Findings, Level};
use proc::{Capture, Completion, Supervision, run_supervised};
use project::ProjectSpec;
use stats::ScenarioStats;
use tracing::info;

#[derive(Parser, Debug)]
#[command(
    name = "sccache-bench",
    about = "Benchmark sccache against a CMake + Ninja MSVC build."
)]
struct Args {
    /// sccache binary under test. Defaults to target/release/sccache.exe.
    #[arg(long)]
    sccache: Option<PathBuf>,

    /// Use an existing CMake project instead of generating one.
    #[arg(long)]
    project: Option<PathBuf>,

    /// File to modify for the touch scenario when --project is given.
    #[arg(long)]
    touch_file: Option<PathBuf>,

    /// Cache directory. Defaults to $SCCACHE_DIR.
    #[arg(long)]
    cache_dir: Option<PathBuf>,

    /// Compiler driver to benchmark. `clang-cl` goes through the same MSVC
    /// argument parsing but a different preprocess path, so it is worth
    /// measuring separately rather than assuming cl results carry over.
    #[arg(long, default_value = "cl")]
    compiler: String,

    /// Ninja parallelism. Defaults to the processor count.
    #[arg(long, default_value_t = 0)]
    jobs: usize,

    /// Sets SCCACHE_CACHE_ZSTD_LEVEL, to A/B compression against hit latency.
    #[arg(long)]
    zstd_level: Option<u32>,

    /// Where to write the JSON result.
    #[arg(long)]
    out: Option<PathBuf>,

    /// Hard timeout per build, in seconds. The build is killed, tree and all.
    #[arg(long, default_value_t = 900)]
    timeout: u64,

    /// Seconds between progress lines while a build runs.
    #[arg(long, default_value_t = 10)]
    heartbeat: u64,

    /// Report a build as stalled after this many seconds with no output.
    #[arg(long, default_value_t = 30)]
    stall: u64,

    /// Write a detailed log here. Rendering and I/O happen on a dedicated
    /// thread, so this does not perturb the timings. Defaults to a file
    /// alongside the JSON result; pass "none" to disable.
    #[arg(long)]
    log_file: Option<String>,

    /// Record environment findings but run anyway.
    #[arg(long)]
    skip_checks: bool,

    #[arg(long, default_value_t = 40)]
    translation_units: usize,
    #[arg(long, default_value_t = 20)]
    headers: usize,
    #[arg(long, default_value_t = 8)]
    includes_per_unit: usize,
    #[arg(long, default_value_t = 60)]
    weight: usize,
}

#[derive(Debug, Serialize)]
struct ScenarioResult {
    name: String,
    wall_seconds: f64,
    timed_out: bool,
    longest_silence_seconds: f64,
    #[serde(flatten)]
    stats: ScenarioStats,
    notes: Vec<String>,
}

#[derive(Debug, Serialize)]
struct BenchResult {
    unix_time: u64,
    sccache: SccacheInfo,
    environment: EnvInfo,
    findings: Vec<env::Finding>,
    scenarios: Vec<ScenarioResult>,
}

#[derive(Debug, Serialize)]
struct SccacheInfo {
    path: String,
    version: String,
    commit: String,
}

#[derive(Debug, Serialize)]
struct EnvInfo {
    cache_dir: String,
    cache_volume: env::VolumeInfo,
    project_dir: String,
    project_volume: env::VolumeInfo,
    jobs: usize,
    zstd_level: Option<u32>,
    embedded_debug_info: Option<bool>,
    generated: Option<project::GeneratedProject>,
    compiler: String,
    compiler_path: String,
}

fn repo_root() -> Result<PathBuf> {
    // scripts/bench/ -> repo root
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    Ok(manifest
        .parent()
        .and_then(|p| p.parent())
        .context("locating repo root")?
        .to_path_buf())
}

fn main() -> Result<()> {
    let args = Args::parse();
    let root = repo_root()?;

    // Install logging before anything else, so even a failure during toolchain
    // discovery leaves a record behind.
    let log_path = match args.log_file.as_deref() {
        Some("none") => None,
        Some(p) => Some(PathBuf::from(p)),
        None => Some(
            root.join("scripts/bench/results")
                .join(format!("bench-{}.log", now_unix())),
        ),
    };
    let _log_guard = match &log_path {
        Some(p) => {
            let g = telemetry::init(p)?;
            eprintln!("log     : {}", p.display());
            Some(g)
        }
        None => None,
    };

    let jobs = if args.jobs == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(8)
    } else {
        args.jobs
    };

    // --- sccache binary --------------------------------------------------
    let sccache = match &args.sccache {
        Some(p) => p.clone(),
        None => {
            let candidates = [
                root.join("target/release/sccache.exe"),
                root.join("target/release/sccache"),
                root.join("target/debug/sccache.exe"),
                root.join("target/debug/sccache"),
            ];
            candidates
                .into_iter()
                .find(|p| p.exists())
                .context("no sccache binary found; build it or pass --sccache")?
        }
    };
    let sccache = env::canonicalize_usable(&sccache)?;

    // --- toolchain -------------------------------------------------------
    eprintln!("importing MSVC environment...");
    let mut environment = env::msvc_environment()?;

    let cache_dir = args
        .cache_dir
        .clone()
        .or_else(|| std::env::var_os("SCCACHE_DIR").map(PathBuf::from))
        .context("no cache dir: pass --cache-dir or set SCCACHE_DIR")?;
    fs::create_dir_all(&cache_dir)?;
    let cache_dir = env::canonicalize_usable(&cache_dir)?;
    environment.insert(
        "SCCACHE_DIR".into(),
        cache_dir.to_string_lossy().into_owned(),
    );
    if let Some(level) = args.zstd_level {
        environment.insert("SCCACHE_CACHE_ZSTD_LEVEL".into(), level.to_string());
    }

    let cmake = env::which_in_env("cmake", &environment).context("cmake not found on PATH")?;
    let ninja = env::which_in_env("ninja", &environment).context("ninja not found on PATH")?;
    // Accept a full path as well as a bare name. Which clang-cl you get matters:
    // the MSVC STL static_asserts on compiler version (STL1000), so an older
    // clang-cl from a side-by-side VS install cannot compile against a newer
    // toolset's headers even though both are on the PATH.
    let compiler = {
        let as_path = Path::new(&args.compiler);
        if as_path.is_file() {
            env::canonicalize_usable(as_path)?
        } else {
            env::which_in_env(&args.compiler, &environment).with_context(|| {
                format!(
                    "compiler {:?} not found on PATH; pass a full path if it is not on the                      PATH that vcvarsall sets up",
                    args.compiler
                )
            })?
        }
    };
    let compiler_name = compiler
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| args.compiler.clone());

    // --- project ---------------------------------------------------------
    let (project_dir, touch_file, generated) = match &args.project {
        Some(p) => {
            let p = env::canonicalize_usable(p)?;
            (p, args.touch_file.clone(), None)
        }
        None => {
            let dir = root.join("target/bench-project");
            fs::create_dir_all(&dir)?;
            let spec = ProjectSpec {
                translation_units: args.translation_units,
                headers: args.headers,
                includes_per_unit: args.includes_per_unit,
                weight: args.weight,
            };
            let generated = project::generate(&dir, &spec)?;
            let touch = generated.hot_header.clone();
            (
                env::canonicalize_usable(&dir)?,
                Some(touch),
                Some(generated),
            )
        }
    };
    let build_dir = project_dir.join("build");

    // --- findings --------------------------------------------------------
    let cache_vol = env::volume_info(&cache_dir);
    let project_vol = env::volume_info(&project_dir);
    let mut findings = Findings::default();

    // A debug build makes every wall-clock number here meaningless: sccache's hot
    // path is blake3 hashing and zstd compression, and unoptimised Rust runs those
    // far slower, swamping whatever is being measured. Hit/miss counters stay valid,
    // since those are logic rather than speed.
    // sccache decides what it is from argv[0]: if the file stem is not exactly
    // "sccache" it treats itself as a compiler shim and rewrites the command line
    // to `sccache <exe-name> <args...>` (src/cmdline.rs). A renamed binary then
    // interprets `--stop-server` as "compile this", starts a daemon and blocks
    // forever, with no output to say so. Compare two builds by putting each in
    // its own directory, not by renaming them.
    let stem = sccache
        .file_stem()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if stem != "sccache" {
        findings.push(
            Level::Error,
            format!(
                "sccache binary is named '{stem}', not 'sccache'. A renamed binary acts as a \
                 compiler shim and will hang on control commands like --stop-server. Put the \
                 binary in its own directory under the name sccache instead."
            ),
        );
    }

    let looks_debug = sccache
        .to_string_lossy()
        .replace('\\', "/")
        .contains("/target/debug/");
    if looks_debug {
        findings.push(
            Level::Error,
            format!(
                "Benchmarking a debug build ({}). Hashing and compression dominate sccache and are \
                 far slower unoptimised, so timings will not reflect release behaviour. Build with \
                 --release, or pass --skip-checks if you only want the hit/miss counters.",
                sccache.display()
            ),
        );
    }
    if cache_vol.root != project_vol.root {
        findings.push(
            Level::Error,
            format!(
                "Cache ({}) and build tree ({}) are on different volumes. Block cloning requires \
                 one volume, and cross-volume numbers are not comparable to same-volume ones.",
                cache_vol.root, project_vol.root
            ),
        );
    }
    if !cache_vol.filesystem.eq_ignore_ascii_case("ReFS") {
        findings.push(
            Level::Warning,
            format!(
                "Cache volume is {}, not ReFS. Block cloning is unavailable here.",
                cache_vol.filesystem
            ),
        );
    }
    if cache_vol.dev_drive != "trusted" {
        findings.push(
            Level::Info,
            format!(
                "Cache volume Dev Drive status: {}. Defender async scanning may not apply.",
                cache_vol.dev_drive
            ),
        );
    }

    eprintln!("sccache : {}", sccache.display());
    eprintln!("project : {}", project_dir.display());
    eprintln!(
        "cache   : {} ({}, dev drive: {})",
        cache_dir.display(),
        cache_vol.filesystem,
        cache_vol.dev_drive
    );
    eprintln!("jobs    : {jobs}\n");
    findings.report();
    for f in &findings.0 {
        info!(level = ?f.level, "{}", f.message);
    }

    if findings.has_errors() && !args.skip_checks {
        bail!("environment checks failed; fix the above or pass --skip-checks");
    }

    // --- scenarios -------------------------------------------------------
    let sv = |label: &str| Supervision {
        heartbeat: Duration::from_secs(args.heartbeat),
        timeout: Duration::from_secs(args.timeout),
        stall: Duration::from_secs(args.stall),
        label: label.to_string(),
    };

    let mut scenarios: Vec<ScenarioResult> = Vec::new();

    // cold: nothing cached, nothing built
    eprintln!("\n[cold] empty cache, empty build tree");
    restart_server(&sccache, &environment, Some(&cache_dir))?;
    configure(
        &cmake,
        &sccache,
        &compiler.display().to_string(),
        &project_dir,
        &build_dir,
        &environment,
        &sv("cmake"),
    )?;
    let embedded_debug_info = has_embedded_debug_info(&build_dir)?;
    if embedded_debug_info == Some(false) {
        findings.push(
            Level::Error,
            "No /Z7 in compile_commands.json. MSVC writes a shared PDB and sccache refuses to \
             cache at all, so this run would measure nothing.",
        );
        findings.report();
        if !args.skip_checks {
            bail!("configured build is not using embedded debug info");
        }
    }
    scenarios.push(run_scenario(
        "cold",
        &sccache,
        &ninja,
        &build_dir,
        jobs,
        &environment,
        &sv("cold"),
    )?);

    // cached: same sources, warm cache, fresh build tree
    eprintln!("\n[cached] warm cache, empty build tree");
    configure(
        &cmake,
        &sccache,
        &compiler.display().to_string(),
        &project_dir,
        &build_dir,
        &environment,
        &sv("cmake"),
    )?;
    let mut cached = run_scenario(
        "cached",
        &sccache,
        &ninja,
        &build_dir,
        jobs,
        &environment,
        &sv("cached"),
    )?;

    // The regression test for the MSVC direct-mode bug: the mode reports as enabled
    // but, before the -E and #line fixes, could never hit. Zero hits on a fully
    // cached rebuild means every TU still ran the preprocessor.
    if cached.stats.use_preprocessor_cache_mode
        && cached.stats.preprocessor_cache_hits == Some(0)
        && cached.stats.preprocessor_cache_misses.unwrap_or(0) > 0
    {
        let msg = "Direct mode reports enabled but recorded 0 hits on a fully cached rebuild: \
                   the preprocessor ran for every translation unit."
            .to_string();
        findings.push(Level::Error, msg.clone());
        cached.notes.push(msg);
    }
    scenarios.push(cached);

    // touch: invalidate one widely-included header
    match &touch_file {
        Some(f) if f.exists() => {
            eprintln!("\n[touch] warm cache, one header edited");
            let original = fs::read(f)?;
            let mut modified = original.clone();
            modified.extend_from_slice(format!("\n// bench touch {}\n", now_unix()).as_bytes());
            fs::write(f, &modified)?;
            let outcome = (|| -> Result<ScenarioResult> {
                configure(
                    &cmake,
                    &sccache,
                    &compiler.display().to_string(),
                    &project_dir,
                    &build_dir,
                    &environment,
                    &sv("cmake"),
                )?;
                run_scenario(
                    "touch",
                    &sccache,
                    &ninja,
                    &build_dir,
                    jobs,
                    &environment,
                    &sv("touch"),
                )
            })();
            // Restore whatever happened, so a failure here can't leave the tree edited.
            fs::write(f, &original)?;
            scenarios.push(outcome?);
        }
        _ => eprintln!("\n[touch] skipped (no --touch-file)"),
    }

    // --- output ----------------------------------------------------------
    let version = proc::run_capture(
        &sccache,
        &["--version".into()],
        None,
        &environment,
        Duration::from_secs(30),
    )
    .unwrap_or_default()
    .trim()
    .to_string();
    let commit = git_commit(&root).unwrap_or_default();

    let result = BenchResult {
        unix_time: now_unix(),
        sccache: SccacheInfo {
            path: sccache.display().to_string(),
            version,
            commit,
        },
        environment: EnvInfo {
            cache_dir: cache_dir.display().to_string(),
            cache_volume: cache_vol,
            project_dir: project_dir.display().to_string(),
            project_volume: project_vol,
            jobs,
            zstd_level: args.zstd_level,
            embedded_debug_info,
            generated,
            compiler: compiler_name.clone(),
            compiler_path: compiler.display().to_string(),
        },
        findings: findings.0.clone(),
        scenarios,
    };

    let out_path = match &args.out {
        Some(p) => p.clone(),
        None => {
            let dir = root.join("scripts/bench/results");
            fs::create_dir_all(&dir)?;
            dir.join(format!("bench-{}.json", result.unix_time))
        }
    };
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&out_path, serde_json::to_string_pretty(&result)?)?;

    print_summary(&result);
    eprintln!("written: {}", out_path.display());

    if findings.has_errors() {
        std::process::exit(1);
    }
    Ok(())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn git_commit(root: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args([
            "-C",
            &root.display().to_string(),
            "rev-parse",
            "--short",
            "HEAD",
        ])
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn restart_server(
    sccache: &Path,
    environment: &HashMap<String, String>,
    wipe: Option<&Path>,
) -> Result<()> {
    // Ignore failures: "not running" is a perfectly good outcome here.
    let _ = proc::run_capture(
        sccache,
        &["--stop-server".into()],
        None,
        environment,
        Duration::from_secs(60),
    );
    if let Some(dir) = wipe {
        if dir.exists() {
            fs::remove_dir_all(dir)
                .with_context(|| format!("wiping cache dir {}", dir.display()))?;
        }
        fs::create_dir_all(dir)?;
    }
    proc::run_capture(
        sccache,
        &["--start-server".into()],
        None,
        environment,
        Duration::from_secs(120),
    )?;
    Ok(())
}

fn configure(
    cmake: &Path,
    sccache: &Path,
    compiler: &str,
    project_dir: &Path,
    build_dir: &Path,
    environment: &HashMap<String, String>,
    sv: &Supervision,
) -> Result<()> {
    if build_dir.exists() {
        fs::remove_dir_all(build_dir)
            .with_context(|| format!("clearing build dir {}", build_dir.display()))?;
    }
    fs::create_dir_all(build_dir)?;

    let launcher = sccache.display().to_string();
    let args: Vec<String> = vec![
        "-S".into(),
        project_dir.display().to_string(),
        "-B".into(),
        build_dir.display().to_string(),
        "-G".into(),
        "Ninja".into(),
        "-DCMAKE_BUILD_TYPE=Debug".into(),
        format!("-DCMAKE_C_COMPILER={compiler}"),
        format!("-DCMAKE_CXX_COMPILER={compiler}"),
        format!("-DCMAKE_C_COMPILER_LAUNCHER={launcher}"),
        format!("-DCMAKE_CXX_COMPILER_LAUNCHER={launcher}"),
        "-DCMAKE_MSVC_DEBUG_INFORMATION_FORMAT=Embedded".into(),
        "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON".into(),
    ];

    let out = run_supervised(cmake, &args, None, environment, sv, Capture::ViaFile)?;
    if !out.success() {
        bail!(
            "cmake configure failed ({:?}):\n{}",
            out.completion,
            out.tail(40)
        );
    }
    Ok(())
}

/// `/Z7` is what makes MSVC output cacheable; `/Zi` writes a PDB shared across
/// translation units and sccache declines to cache it.
fn has_embedded_debug_info(build_dir: &Path) -> Result<Option<bool>> {
    let path = build_dir.join("compile_commands.json");
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)?;
    let found = text
        .split(|c: char| c.is_whitespace() || c == '"' || c == ',')
        .any(|tok| tok.eq_ignore_ascii_case("/Z7") || tok.eq_ignore_ascii_case("-Z7"));
    Ok(Some(found))
}

#[allow(clippy::too_many_arguments)]
fn run_scenario(
    name: &str,
    sccache: &Path,
    ninja: &Path,
    build_dir: &Path,
    jobs: usize,
    environment: &HashMap<String, String>,
    sv: &Supervision,
) -> Result<ScenarioResult> {
    stats::zero(sccache, environment)?;

    let args: Vec<String> = vec![
        "-C".into(),
        build_dir.display().to_string(),
        "-j".into(),
        jobs.to_string(),
    ];
    let out = run_supervised(ninja, &args, None, environment, sv, Capture::Streamed)?;

    let mut notes = Vec::new();
    let timed_out = out.completion == Completion::TimedOut;
    if timed_out {
        notes.push(format!(
            "build killed after the {}s hard timeout; last output: {}",
            sv.timeout.as_secs(),
            out.last_line.clone().unwrap_or_else(|| "(none)".into())
        ));
    } else if !out.success() {
        bail!(
            "ninja build failed ({:?}):\n{}",
            out.completion,
            out.tail(40)
        );
    }
    if out.longest_silence >= sv.stall {
        notes.push(format!(
            "longest gap with no output was {:.0}s",
            out.longest_silence.as_secs_f64()
        ));
    }

    let info = stats::show(sccache, environment)?;
    eprintln!("  {:.2}s", out.elapsed.as_secs_f64());

    Ok(ScenarioResult {
        name: name.to_string(),
        wall_seconds: (out.elapsed.as_secs_f64() * 1000.0).round() / 1000.0,
        timed_out,
        longest_silence_seconds: (out.longest_silence.as_secs_f64() * 100.0).round() / 100.0,
        stats: ScenarioStats::from(&info),
        notes,
    })
}

fn print_summary(result: &BenchResult) {
    let fmt = |v: Option<u64>| match v {
        Some(n) => n.to_string(),
        None => "n/a".into(),
    };
    eprintln!("\n--- summary ---");
    eprintln!(
        "{:<8} {:>9} {:>7} {:>8} {:>10} {:>10}",
        "scenario", "seconds", "hits", "misses", "pp-hits", "pp-misses"
    );
    for s in &result.scenarios {
        eprintln!(
            "{:<8} {:>9.2} {:>7} {:>8} {:>10} {:>10}{}",
            s.name,
            s.wall_seconds,
            s.stats.cache_hits,
            s.stats.cache_misses,
            fmt(s.stats.preprocessor_cache_hits),
            fmt(s.stats.preprocessor_cache_misses),
            if s.timed_out { "  TIMED OUT" } else { "" }
        );
    }
}
