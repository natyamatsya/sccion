// SPDX-FileCopyrightText: Copyright natyamatsya and sccache contributors
// SPDX-License-Identifier: Apache-2.0

//! `sccache --check-env`: report the environment sccache is about to run in.
//!
//! The failures this exists to catch are not crashes. They are configurations
//! that work perfectly while quietly costing most of what sccache could have
//! saved -- a cache on a different volume than the build tree, `/Zi` instead of
//! `/Z7` so nothing is cacheable at all, preprocessor cache mode reporting itself
//! enabled while structurally unable to hit. Each of those produces a build that
//! succeeds and numbers that mean nothing.
//!
//! Three rules shape everything here:
//!
//! 1. **Out of band.** None of this runs on the compile path. The checks cost
//!    more than they could ever save there.
//! 2. **Report, never adapt.** Nothing sccache does may vary with what is found.
//!    Behaviour that depends on the detected environment gives you a cache hit on
//!    one machine and a miss on another, which is the least debuggable kind of
//!    bug.
//! 3. **Never change anything.** No writes to Defender preferences, no
//!    `fsutil devdrv` mutation, no filter detachment. Besides needing elevation, a
//!    build tool that reconfigures antivirus is indistinguishable from malware and
//!    will eventually be treated as such.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// Something that invalidates results or defeats caching outright.
    Error,
    /// Works, but leaves a known amount of performance unclaimed.
    Warning,
    /// Worth knowing; no action implied.
    Info,
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub level: Level,
    pub message: String,
    /// What to do about it, when there is something to do.
    pub remedy: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VolumeReport {
    pub path: String,
    pub root: Option<String>,
    pub filesystem: String,
    /// `trusted`, `no`, or `unknown (needs elevation)` -- never guessed.
    pub dev_drive: String,
    pub supports_block_cloning: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnvReport {
    pub sccache_version: String,
    pub sccache_path: Option<String>,
    pub cache: VolumeReport,
    pub working_directory: VolumeReport,
    pub same_volume: bool,
    pub cache_location: String,
    pub max_cache_size: u64,
    pub compression: String,
    pub preprocessor_cache_mode: bool,
    pub preprocessor_cache_settings: Vec<(String, bool)>,
    pub defender: DefenderReport,
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DefenderReport {
    /// `Get-MpComputerStatus` is not always available or permitted; `None` means
    /// we could not tell, which is reported as such rather than assumed benign.
    pub real_time_protection: Option<bool>,
    pub performance_mode: Option<String>,
    pub process_exclusions: Option<Vec<String>>,
    /// Tamper Protection makes Defender discard exclusion writes without
    /// failing, so this decides whether recommending `Add-MpPreference` is
    /// useful advice or a wild goose chase.
    pub tamper_protected: Option<bool>,
}

struct Findings(Vec<Finding>);

impl Findings {
    fn push(&mut self, level: Level, message: impl Into<String>, remedy: Option<&str>) {
        self.0.push(Finding {
            level,
            message: message.into(),
            remedy: remedy.map(str::to_owned),
        });
    }
}

/// What to actually tell someone whose compiler is not excluded from Defender.
///
/// Only `Some(true)` diverts to the policy route. An unknown tamper state must
/// keep the ordinary advice: guessing that it is on would talk someone out of a
/// command that would have worked.
fn exclusion_remedy(tamper_protected: Option<bool>) -> &'static str {
    if tamper_protected == Some(true) {
        "Tamper Protection must be turned off in the Windows Security app, or the \
         exclusions set through Defender policy; Add-MpPreference will not take. \
         A Dev Drive gets scanning deferred without needing exclusions at all."
    } else {
        "Add-MpPreference -ExclusionProcess cl.exe, sccache.exe, ninja.exe"
    }
}

fn volume_root(path: &Path) -> Option<String> {
    let s = path.to_string_lossy().replace('/', "\\");
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s).to_string();
    let b = s.as_bytes();
    (b.len() >= 2 && b[1] == b':' && (b[0] as char).is_ascii_alphabetic())
        .then(|| format!("{}:\\", (b[0] as char).to_ascii_uppercase()))
}

#[cfg(windows)]
fn filesystem_name(root: &str) -> String {
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;
    let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
    let mut name = [0u16; 64];
    // SAFETY: `wide` is NUL-terminated; the output buffer length is its true
    // element count and every other out-parameter is null.
    let ok = unsafe {
        GetVolumeInformationW(
            wide.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            name.as_mut_ptr(),
            name.len() as u32,
        )
    };
    if ok == 0 {
        return "unknown".into();
    }
    let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    String::from_utf16_lossy(&name[..end])
}

#[cfg(not(windows))]
fn filesystem_name(_root: &str) -> String {
    "n/a".into()
}

/// Dev Drive trust state.
///
/// Distinguishes "no" from "could not tell": the query needs elevation, and
/// reporting an unelevated failure as "not a Dev Drive" would be a lie that
/// silently disables a check.
#[cfg(windows)]
fn dev_drive_state(root: &str) -> String {
    let letter = root.trim_end_matches(":\\").trim_end_matches('\\');
    let Ok(out) = std::process::Command::new("fsutil")
        .args(["devdrv", "query", &format!("{letter}:")])
        .output()
    else {
        return "unknown".into();
    };
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
    .to_ascii_lowercase();
    if text.contains("trusted developer volume") {
        "trusted".into()
    } else if text.contains("not a developer volume") {
        "no".into()
    } else if text.contains("access is denied") {
        "unknown (needs elevation)".into()
    } else {
        "unknown".into()
    }
}

#[cfg(not(windows))]
fn dev_drive_state(_root: &str) -> String {
    "n/a".into()
}

fn describe_volume(path: &Path) -> VolumeReport {
    let root = volume_root(path);
    let (filesystem, dev_drive) = match &root {
        Some(r) => (filesystem_name(r), dev_drive_state(r)),
        None => ("unknown".into(), "unknown".into()),
    };
    VolumeReport {
        path: path.display().to_string(),
        supports_block_cloning: crate::blockclone::volume_supports_cloning(path),
        root,
        filesystem,
        dev_drive,
    }
}

/// Defender state, best effort.
///
/// Shelling out to PowerShell rather than binding WMI: this runs once, by hand,
/// and a COM dependency for a diagnostic is not a good trade. Every field is
/// optional, because "we could not tell" is a real answer here.
#[cfg(windows)]
fn defender_report() -> DefenderReport {
    let script = "$ErrorActionPreference='SilentlyContinue';\
        $s = Get-MpComputerStatus; $p = Get-MpPreference;\
        [pscustomobject]@{ rtp = $s.RealTimeProtectionEnabled;\
        perf = $s.PerformanceModeStatus;\
        tamper = $s.IsTamperProtected;\
        excl = @($p.ExclusionProcess) } | ConvertTo-Json -Compress";
    let Ok(out) = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
    else {
        return DefenderReport::default();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text.trim()) else {
        return DefenderReport::default();
    };
    DefenderReport {
        real_time_protection: v.get("rtp").and_then(|x| x.as_bool()),
        // A JSON null means the property is absent on this Defender build, which
        // is "could not tell" rather than a value worth printing.
        performance_mode: v.get("perf").and_then(|x| match x {
            serde_json::Value::Null => None,
            serde_json::Value::Number(n) => Some(n.to_string()),
            serde_json::Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        }),
        process_exclusions: v
            .get("excl")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|e| e.as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
            })
            // Unelevated, Get-MpPreference does not fail: it succeeds and puts
            // the refusal *in the value*, as
            // "N/A: Must be an administrator to view exclusions". Taken at face
            // value that is a list containing no compilers, which reads as
            // "nothing is excluded" -- the exact opposite of the truth, and it
            // would tell someone who had just added exclusions that they had
            // not. Treat it as the "could not tell" this type already models.
            .filter(|e: &Vec<String>| !e.iter().any(|p| p.contains("Must be an administrator"))),
        // Readable without elevation, unlike the exclusion list itself, so this
        // is worth reporting either way.
        tamper_protected: v.get("tamper").and_then(|x| x.as_bool()),
    }
}

#[cfg(not(windows))]
fn defender_report() -> DefenderReport {
    DefenderReport::default()
}

/// Gather everything, without changing anything.
pub fn check(config: &Config, cwd: &Path) -> EnvReport {
    let mut findings = Findings(Vec::new());

    // The configured directory, not the default: SCCACHE_DIR and the config
    // file both move it, and reporting the default would defeat the point.
    let cache_dir: PathBuf = config.fallback_cache.dir.clone();
    let cache = describe_volume(&cache_dir);
    let working_directory = describe_volume(cwd);
    let same_volume = cache.root.is_some() && cache.root == working_directory.root;

    // Storage encoding, as this process sees it. The server reads its own
    // environment at startup, so a running server may disagree -- said plainly
    // rather than implied.
    let compression = match std::env::var("SCCACHE_CACHE_COMPRESSION") {
        Ok(v)
            if matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "none" | "off" | "false" | "0"
            ) =>
        {
            "none".to_string()
        }
        _ => format!(
            "zstd level {}",
            std::env::var("SCCACHE_CACHE_ZSTD_LEVEL").unwrap_or_else(|_| "3".into())
        ),
    };

    let pp = config.fallback_cache.preprocessor_cache_mode;
    let preprocessor_cache_mode = pp.use_preprocessor_cache_mode;
    let preprocessor_cache_settings = {
        let p = pp;
        vec![
            ("file_stat_matches".to_string(), p.file_stat_matches),
            ("use_ctime_for_stat".to_string(), p.use_ctime_for_stat),
            ("ignore_time_macros".to_string(), p.ignore_time_macros),
            ("skip_system_headers".to_string(), p.skip_system_headers),
        ]
    };

    // --- findings ---------------------------------------------------------

    if !same_volume {
        findings.push(
            Level::Warning,
            format!(
                "Cache ({}) and working directory ({}) are on different volumes.",
                cache.root.clone().unwrap_or_else(|| "?".into()),
                working_directory.root.clone().unwrap_or_else(|| "?".into())
            ),
            Some("Set SCCACHE_DIR to a directory on the same volume as the build tree."),
        );
    }

    if cache.dev_drive == "no" {
        findings.push(
            Level::Info,
            format!(
                "Cache volume {} is not a Dev Drive, so Defender scans it synchronously.",
                cache.root.clone().unwrap_or_default()
            ),
            Some("A Dev Drive defers scanning until after the file open completes."),
        );
    } else if cache.dev_drive.starts_with("unknown") {
        findings.push(
            Level::Info,
            format!(
                "Dev Drive status of the cache volume is {}.",
                cache.dev_drive
            ),
            Some("Run this from an elevated prompt for a definitive answer."),
        );
    }

    if !preprocessor_cache_mode {
        findings.push(
            Level::Warning,
            "Preprocessor cache mode is off, so every compile runs the preprocessor even on \
             a cache hit."
                .to_string(),
            Some("It requires local disk storage; SCCACHE_DIRECT=false also disables it."),
        );
    }

    let defender = defender_report();
    if defender.real_time_protection == Some(true) {
        // Tamper Protection makes Defender accept an exclusion write and then
        // discard it -- Add-MpPreference reports no error and the list stays
        // empty. Recommending it here would send someone chasing a command that
        // cannot work, so the remedy has to change with it.
        let tampered = defender.tamper_protected == Some(true);
        let remedy = exclusion_remedy(defender.tamper_protected);
        match defender.process_exclusions.as_ref() {
            Some(e) if !e.iter().any(|p| p.to_ascii_lowercase().contains("cl.exe")) => {
                findings.push(
                    Level::Info,
                    format!(
                        "Defender real-time protection is on and cl.exe is not excluded{}.",
                        if tampered {
                            ", and Tamper Protection is on, so exclusions cannot be added locally"
                        } else {
                            ""
                        }
                    ),
                    Some(remedy),
                );
            }
            // Reading the exclusion list needs elevation. Say so, rather than
            // let silence imply the exclusions are in place -- or, worse,
            // claim they are missing when they are not.
            None => {
                findings.push(
                    Level::Info,
                    format!(
                        "Defender real-time protection is on; its process exclusions could not \
                         be read (needs elevation), so this run cannot say whether cl.exe is \
                         excluded.{}",
                        if tampered {
                            " Tamper Protection is on, so any exclusions that are missing cannot \
                             be added locally anyway."
                        } else {
                            ""
                        }
                    ),
                    Some("Run this from an elevated prompt for a definitive answer."),
                );
            }
            Some(_) => {}
        }
    }

    let sccache_path = std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string());
    if let (Some(exe), Some(cache_root)) = (&sccache_path, &cache.root)
        && volume_root(Path::new(exe)).as_ref() != Some(cache_root)
    {
        findings.push(
            Level::Info,
            "sccache.exe is not on the cache volume.".to_string(),
            Some(
                "Executables can live on a Dev Drive; only the Dev Drive itself cannot be the \
                 system volume. Moving them there gets their image scans deferred too.",
            ),
        );
    }

    findings.0.sort_by_key(|f| f.level);

    EnvReport {
        sccache_version: env!("CARGO_PKG_VERSION").to_string(),
        sccache_path,
        cache_location: cache_dir.display().to_string(),
        max_cache_size: config.fallback_cache.size,
        cache,
        working_directory,
        same_volume,
        compression,
        preprocessor_cache_mode,
        preprocessor_cache_settings,
        defender: defender_report(),
        findings: findings.0,
    }
}

impl EnvReport {
    /// True if anything found would invalidate a measurement, so CI can gate.
    pub fn has_errors(&self) -> bool {
        self.findings.iter().any(|f| f.level == Level::Error)
    }

    pub fn print(&self) {
        println!("sccache {}", self.sccache_version);
        if let Some(p) = &self.sccache_path {
            println!("  binary                  {p}");
        }
        println!("  cache                   {}", self.cache_location);
        println!(
            "  cache volume            {} ({}, dev drive: {}, block cloning: {})",
            self.cache.root.clone().unwrap_or_else(|| "?".into()),
            self.cache.filesystem,
            self.cache.dev_drive,
            if self.cache.supports_block_cloning {
                "yes"
            } else {
                "no"
            }
        );
        println!(
            "  working dir volume      {} ({})",
            self.working_directory
                .root
                .clone()
                .unwrap_or_else(|| "?".into()),
            self.working_directory.filesystem
        );
        println!("  same volume             {}", self.same_volume);
        println!("  compression             {}", self.compression);
        println!("  direct/preprocessor     {}", self.preprocessor_cache_mode);
        for (k, v) in &self.preprocessor_cache_settings {
            println!("    {k:<22}{v}");
        }
        match self.defender.real_time_protection {
            Some(rtp) => println!("  defender rtp            {rtp}"),
            None => println!("  defender rtp            unknown"),
        }
        if let Some(mode) = &self.defender.performance_mode {
            println!("  defender perf mode      {mode}");
        }
        if let Some(t) = self.defender.tamper_protected {
            println!("  defender tamper prot    {t}");
        }

        if self.findings.is_empty() {
            println!("\nNo findings.");
            return;
        }
        println!("\nFindings:");
        for f in &self.findings {
            let tag = match f.level {
                Level::Error => "error",
                Level::Warning => "warning",
                Level::Info => "info",
            };
            println!("  [{tag}] {}", f.message);
            if let Some(r) = &f.remedy {
                println!("          -> {r}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Advice that cannot be carried out is worse than no advice: it sends
    /// someone to run a command that reports success and changes nothing.
    #[test]
    fn tamper_protection_changes_the_exclusion_remedy() {
        // Tamper Protection on: Add-MpPreference is accepted and discarded, so
        // it must not be the advice.
        let blocked = exclusion_remedy(Some(true));
        assert!(blocked.contains("Tamper Protection"));
        assert!(
            !blocked.contains("Add-MpPreference -ExclusionProcess"),
            "must not hand out a command that silently does nothing"
        );

        // Off, or unreadable: the ordinary advice stands. Guessing "on" would
        // talk someone out of a command that would have worked.
        for state in [Some(false), None] {
            assert!(
                exclusion_remedy(state).starts_with("Add-MpPreference"),
                "tamper_protected={state:?} should keep the normal remedy"
            );
        }
    }

    #[test]
    fn volume_root_handles_the_shapes_paths_actually_take() {
        assert_eq!(volume_root(Path::new(r"D:\cache")).as_deref(), Some("D:\\"));
        // fs::canonicalize hands back verbatim paths; missing this reported every
        // volume as unknown and silently disabled the same-volume check.
        assert_eq!(
            volume_root(Path::new(r"\\?\D:\cache")).as_deref(),
            Some("D:\\")
        );
        assert_eq!(volume_root(Path::new("d:/cache")).as_deref(), Some("D:\\"));
        // A UNC path has no drive letter; claiming one would be a lie.
        assert_eq!(volume_root(Path::new(r"\\server\share")), None);
        assert_eq!(volume_root(Path::new("relative/path")), None);
    }

    #[test]
    fn a_report_with_no_errors_does_not_gate_ci() {
        let report = EnvReport {
            sccache_version: "0".into(),
            sccache_path: None,
            cache: VolumeReport {
                path: "d:/c".into(),
                root: Some("D:\\".into()),
                filesystem: "ReFS".into(),
                dev_drive: "trusted".into(),
                supports_block_cloning: true,
            },
            working_directory: VolumeReport {
                path: "d:/w".into(),
                root: Some("D:\\".into()),
                filesystem: "ReFS".into(),
                dev_drive: "trusted".into(),
                supports_block_cloning: true,
            },
            same_volume: true,
            cache_location: "d:/c".into(),
            max_cache_size: 0,
            compression: "none".into(),
            preprocessor_cache_mode: true,
            preprocessor_cache_settings: vec![],
            defender: DefenderReport::default(),
            findings: vec![
                Finding {
                    level: Level::Info,
                    message: "fine".into(),
                    remedy: None,
                },
                Finding {
                    level: Level::Warning,
                    message: "suboptimal".into(),
                    remedy: None,
                },
            ],
        };
        // Warnings describe unclaimed performance; only errors mean the numbers
        // would be meaningless, and only those should fail a CI gate.
        assert!(!report.has_errors());
    }
}

#[cfg(test)]
mod cost {
    use super::*;

    /// Not an assertion: how long does loading the configuration take?
    ///
    /// It sits on the client's hot path, and the per-invocation budget it
    /// competes for is single-digit milliseconds, so "it reads a small file" is
    /// not good enough an answer.
    #[test]
    fn report_config_load_cost() {
        let runs = 500;
        // Warm the filesystem cache and any lazily-initialised globals first.
        let _ = Config::load();
        let start = std::time::Instant::now();
        for _ in 0..runs {
            let _ = Config::load();
        }
        let ms = start.elapsed().as_secs_f64() * 1000.0 / runs as f64;
        eprintln!("CONFIG LOAD: {ms:.3} ms per call ({runs} runs)");
    }
}
