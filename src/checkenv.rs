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

/// Which layer supplied a setting's value.
///
/// A report that gave only the value would be true and useless: the question
/// anyone actually has is *why* it is that, and the answer is almost always
/// "something you did not know was overriding it".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// This machine's configuration, or the default it fell back to.
    Machine,
    /// A `.sccache.toml` in the tree being built, which outranks the machine.
    Project,
    /// An environment variable, which outranks both.
    Environment,
    /// No layer chose it: it follows from another setting, named in `note`.
    Implied,
}

/// One resolved setting, and where its value came from.
#[derive(Debug, Clone, Serialize)]
pub struct Setting {
    pub name: String,
    /// What a compilation in the reported working directory would actually
    /// use -- not what the machine's configuration says in isolation.
    pub value: bool,
    pub origin: Origin,
    /// What the machine's configuration said, when something outranked it.
    ///
    /// `None` when nothing did, so that nobody has to compare two equal values
    /// to work out whether an override was in play.
    pub machine_value: Option<bool>,
    /// Why the value is what it is, when the layers alone do not explain it.
    pub note: Option<String>,
}

impl Setting {
    /// The ordinary case: the most specific layer that said anything wins.
    fn resolved(name: &str, machine: bool, project: Option<bool>, env: Option<bool>) -> Setting {
        let (value, origin) = match (env, project) {
            (Some(v), _) => (v, Origin::Environment),
            (None, Some(v)) => (v, Origin::Project),
            (None, None) => (machine, Origin::Machine),
        };
        Setting {
            name: name.to_owned(),
            value,
            origin,
            machine_value: (origin != Origin::Machine).then_some(machine),
            note: None,
        }
    }

    /// The value, and what it took to get there.
    ///
    /// Silent when the machine's own configuration decided it: saying so on
    /// every line would bury the two or three that are actually interesting.
    pub fn describe(&self) -> String {
        match (self.origin, self.machine_value, &self.note) {
            (Origin::Machine, _, _) => self.value.to_string(),
            (_, _, Some(note)) => format!("{} ({note})", self.value),
            (origin, Some(was), None) => format!(
                "{} (from the {}, over the machine's {was})",
                self.value,
                match origin {
                    Origin::Project => "project",
                    Origin::Environment => "environment",
                    _ => "machine",
                }
            ),
            _ => self.value.to_string(),
        }
    }

    /// A value the layers did not get to choose.
    fn implied(name: &str, machine: bool, why: &str) -> Setting {
        Setting {
            name: name.to_owned(),
            value: false,
            origin: Origin::Implied,
            machine_value: Some(machine),
            note: Some(why.to_owned()),
        }
    }
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
    /// The project file whose settings were applied, if the working directory
    /// is under one. Named so that an override can be traced to a file rather
    /// than left as an assertion.
    pub project_config: Option<String>,
    pub preprocessor_cache_mode: Setting,
    pub preprocessor_cache_settings: Vec<Setting>,
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

/// An environment variable read exactly the way the compile path reads it.
///
/// The same rule as `generate_hash_key`: anything present that is not a
/// recognised negative counts as on. A report that parsed these more strictly
/// than the code does would disagree with the build it describes.
///
/// This is *this process's* environment. For `sccache --check-env` that is the
/// shell someone would build from; for the MCP adapter it is wherever the
/// adapter was launched, which need not be where the build runs. That is why
/// the origin is reported as `environment` rather than folded silently into
/// the value.
fn bool_from_env(name: &str) -> Option<bool> {
    std::env::var(name).ok().map(|v| {
        !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "false" | "off" | "0"
        )
    })
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
        Ok(_) => format!(
            "zstd level {}",
            std::env::var("SCCACHE_CACHE_ZSTD_LEVEL").unwrap_or_else(|_| "3".into())
        ),
        // Nothing in the environment: the config file decides, and reporting
        // the default here would contradict what the server is actually doing.
        Err(_) => match config.object_encoding {
            Some(crate::cache::ObjectEncoding::Uncompressed) => "none".to_string(),
            Some(crate::cache::ObjectEncoding::Zstd(level)) => format!("zstd level {level}"),
            None => format!(
                "zstd level {}",
                std::env::var("SCCACHE_CACHE_ZSTD_LEVEL").unwrap_or_else(|_| "3".into())
            ),
        },
    };

    // The settings a build in `cwd` would run under, not the machine's in
    // isolation. Reporting the machine's was a quiet lie for any tree carrying
    // a `.sccache.toml`: this report said `ignore_time_macros: false` while
    // every compilation in that tree ran with it on, which is the kind of
    // wrong answer that costs an afternoon.
    let project = crate::projectconf::discover(cwd);
    let p = config.fallback_cache.preprocessor_cache_mode;

    // Mode first: the two settings below are meaningless without it.
    //
    // A layer above the machine may only *decline* it, because whether the
    // mode is available at all depends on the cache backend -- a checkout
    // asking for it does not make it so. `SCCACHE_DIRECT` is folded into the
    // machine value already, by `Config::load`, so it is not asked again here.
    let preprocessor_cache_mode = match (
        p.use_preprocessor_cache_mode,
        project.use_preprocessor_cache_mode,
    ) {
        (true, Some(false)) => Setting {
            name: "use_preprocessor_cache_mode".to_owned(),
            value: false,
            origin: Origin::Project,
            machine_value: Some(true),
            note: None,
        },
        (machine, _) => Setting {
            name: "use_preprocessor_cache_mode".to_owned(),
            value: machine,
            origin: Origin::Machine,
            machine_value: None,
            note: None,
        },
    };

    let preprocessor_cache_settings = vec![
        Setting::resolved("file_stat_matches", p.file_stat_matches, None, None),
        Setting::resolved("use_ctime_for_stat", p.use_ctime_for_stat, None, None),
        Setting::resolved(
            "ignore_time_macros",
            p.ignore_time_macros,
            project.ignore_time_macros,
            None,
        ),
        Setting::resolved("skip_system_headers", p.skip_system_headers, None, None),
        // The one with the largest measured effect: on a 561-unit clang build,
        // 206s cold against 155s.
        if preprocessor_cache_mode.value {
            Setting::resolved(
                "use_dependency_keying",
                p.use_dependency_keying,
                project.use_dependency_keying,
                bool_from_env("SCCACHE_DEPEND"),
            )
        } else {
            // Dependency keying replaces only the way a compilation's inputs
            // are learned, so it is inert without the manifest that consumes
            // them. Reporting the configured value here would promise a
            // behaviour that no compilation in this tree can reach.
            Setting::implied(
                "use_dependency_keying",
                p.use_dependency_keying,
                "preprocessor cache mode is off, so nothing consumes the dependency report",
            )
        },
    ];

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

    if !preprocessor_cache_mode.value {
        findings.push(
            Level::Warning,
            "Preprocessor cache mode is off, so every compile runs the preprocessor even on \
             a cache hit."
                .to_string(),
            // Which advice is useful depends on who turned it off. Sending
            // someone to check their storage backend when their own project
            // file declined the mode is a wild goose chase.
            Some(match preprocessor_cache_mode.origin {
                Origin::Project => {
                    "This tree's project file declines it; see use_preprocessor_cache_mode in \
                     the project config named above."
                }
                _ => "It requires local disk storage; SCCACHE_DIRECT=false also disables it.",
            }),
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
        project_config: project.source.map(|p| p.display().to_string()),
        preprocessor_cache_mode,
        preprocessor_cache_settings,
        // The report gathered above, not a second `defender_report()`: each
        // call shells out to PowerShell, and two calls can disagree with the
        // findings that were derived from the first.
        defender,
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
        if let Some(p) = &self.project_config {
            println!("  project config          {p}");
        }
        println!(
            "  direct/preprocessor     {}",
            self.preprocessor_cache_mode.describe()
        );
        for s in &self.preprocessor_cache_settings {
            println!("    {:<22}{}", s.name, s.describe());
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

    /// The whole point of the origin: a value alone cannot tell you that
    /// something you did not write is deciding your build.
    #[test]
    fn a_setting_says_which_layer_supplied_it() {
        let machine_only = Setting::resolved("ignore_time_macros", false, None, None);
        assert_eq!(machine_only.origin, Origin::Machine);
        assert!(!machine_only.value);
        assert_eq!(
            machine_only.machine_value, None,
            "nothing overrode it, so there is no earlier value to report"
        );

        // The case this exists for: a checkout's own file turning something on
        // that the machine leaves off. The report used to say `false` here,
        // while every compilation in that tree ran with it `true`.
        let overridden = Setting::resolved("ignore_time_macros", false, Some(true), None);
        assert!(overridden.value);
        assert_eq!(overridden.origin, Origin::Project);
        assert_eq!(overridden.machine_value, Some(false));
        assert!(overridden.describe().contains("project"));

        // The environment outranks the project, matching `generate_hash_key`.
        // A report that resolved these in a different order would describe a
        // build nobody is running.
        let from_env = Setting::resolved("use_dependency_keying", true, Some(true), Some(false));
        assert!(!from_env.value);
        assert_eq!(from_env.origin, Origin::Environment);
    }

    /// An override that changes nothing must not be reported as if it did.
    #[test]
    fn a_setting_nothing_can_reach_says_so_instead_of_a_value() {
        let inert = Setting::implied(
            "use_dependency_keying",
            true,
            "preprocessor cache mode is off",
        );
        assert!(
            !inert.value,
            "the configured `true` is unreachable, and reporting it would promise \
             a behaviour no compilation in this tree can get"
        );
        assert_eq!(inert.origin, Origin::Implied);
        // The configured value is still there to be seen -- it is what would
        // take effect once the mode is back on.
        assert_eq!(inert.machine_value, Some(true));
        assert!(inert.describe().contains("preprocessor cache mode is off"));
    }

    /// `SCCACHE_DEPEND` is read on the compile path with "anything that is not
    /// a recognised negative means on". Parsing it more strictly here would
    /// make the report disagree with the build it describes.
    #[test]
    fn env_overrides_are_read_the_way_the_compile_path_reads_them() {
        // Sharing one process-wide environment, so the values are set and
        // cleared within a single test rather than raced across several.
        for off in ["false", "FALSE", "off", "0", " 0 "] {
            unsafe { std::env::set_var("SCCACHE_TEST_DEPEND", off) };
            assert_eq!(bool_from_env("SCCACHE_TEST_DEPEND"), Some(false), "{off:?}");
        }
        for on in ["true", "1", "yes", ""] {
            unsafe { std::env::set_var("SCCACHE_TEST_DEPEND", on) };
            assert_eq!(bool_from_env("SCCACHE_TEST_DEPEND"), Some(true), "{on:?}");
        }
        unsafe { std::env::remove_var("SCCACHE_TEST_DEPEND") };
        assert_eq!(
            bool_from_env("SCCACHE_TEST_DEPEND"),
            None,
            "unset is not `false`: it means the layer said nothing, and the \
             project or the machine still gets to decide"
        );
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

    /// The regression the layered shape exists for. A tree that sets
    /// `ignore_time_macros` in its own `.sccache.toml` builds with it on, and
    /// this report said `false` -- the machine's answer, describing a build
    /// nobody was running.
    #[test]
    fn a_project_file_is_reported_as_the_source_of_what_it_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // A checkout, so the walk stops here rather than wandering up into
        // whatever the temporary directory happens to live under.
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(
            root.join(crate::projectconf::PROJECT_CONF_NAME),
            "ignore_time_macros = true\n",
        )
        .unwrap();

        let report = check(&Config::default(), root);
        let setting = report
            .preprocessor_cache_settings
            .iter()
            .find(|s| s.name == "ignore_time_macros")
            .expect("the report lists it");

        assert!(setting.value, "this is what a build in that tree would use");
        assert_eq!(setting.origin, Origin::Project);
        assert_eq!(
            setting.machine_value,
            Some(false),
            "the machine's answer is still worth seeing -- it is what makes \
             this an override rather than an agreement"
        );
        assert!(
            report.project_config.is_some(),
            "an override that cannot be traced to a file is an assertion, not \
             an answer: the file may be several directories up"
        );

        // A setting no project file mentioned still comes from the machine, or
        // a project would be opting out of settings it never named.
        let untouched = report
            .preprocessor_cache_settings
            .iter()
            .find(|s| s.name == "file_stat_matches")
            .expect("the report lists it");
        assert_eq!(untouched.origin, Origin::Machine);
        assert_eq!(untouched.machine_value, None);
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
            project_config: None,
            preprocessor_cache_mode: Setting::resolved(
                "use_preprocessor_cache_mode",
                true,
                None,
                None,
            ),
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
