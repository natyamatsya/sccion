// SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
// SPDX-License-Identifier: Apache-2.0

//! Toolchain discovery and environment checks.
//!
//! The failure this guards against is not a crash but a silently invalid
//! measurement: the cache quietly on the wrong volume, `/Z7` never reaching the
//! compiler, a debug binary being timed. Each of those produces numbers that look
//! fine and mean nothing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::proc::run_capture;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub level: Level,
    pub message: String,
}

#[derive(Debug, Default)]
pub struct Findings(pub Vec<Finding>);

impl Findings {
    pub fn push(&mut self, level: Level, message: impl Into<String>) {
        self.0.push(Finding {
            level,
            message: message.into(),
        });
    }

    pub fn has_errors(&self) -> bool {
        self.0.iter().any(|f| f.level == Level::Error)
    }

    pub fn report(&self) {
        for f in &self.0 {
            let tag = match f.level {
                Level::Error => "error",
                Level::Warning => "warning",
                Level::Info => "info",
            };
            eprintln!("[{tag}] {}", f.message);
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct VolumeInfo {
    pub root: String,
    pub filesystem: String,
    /// `trusted`, `no`, or `unknown (needs elevation)` -- never guessed.
    pub dev_drive: String,
}

/// Extract the drive letter, tolerating the `\\?\` extended-length prefix that
/// `fs::canonicalize` returns on Windows. Missing this silently reports every
/// volume as "unknown", which quietly disables the same-volume and ReFS checks
/// rather than failing them -- the worst possible outcome for a check.
fn drive_letter(path: &Path) -> Option<String> {
    let s = path.to_string_lossy().replace('/', "\\");
    let s = s
        .strip_prefix(r"\\?\UNC\")
        .map(|_| String::new())
        .unwrap_or_else(|| s.strip_prefix(r"\\?\").unwrap_or(&s).to_string());
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && (bytes[0] as char).is_ascii_alphabetic() {
        return Some((bytes[0] as char).to_ascii_uppercase().to_string());
    }
    None
}

/// Canonicalise, then strip the `\\?\` verbatim prefix that Windows adds.
///
/// The prefix is correct for the Win32 API but not portable through the tools we
/// drive: MSVC rejects a verbatim path outright (`C1083: Cannot open source
/// file`), so a canonicalised project directory handed to CMake breaks the build.
/// Left in place only for UNC paths, which have no plain form.
pub fn canonicalize_usable(path: &Path) -> Result<PathBuf> {
    let canonical =
        std::fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?;
    let text = canonical.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        // A drive-letter path is safe to simplify; `\\?\UNC\...` is not.
        Some(rest) if !rest.starts_with("UNC\\") => Ok(PathBuf::from(rest)),
        _ => Ok(canonical),
    }
}

pub fn volume_info(path: &Path) -> VolumeInfo {
    let letter = drive_letter(path).unwrap_or_default();
    let root = if letter.is_empty() {
        String::new()
    } else {
        format!("{letter}:\\")
    };

    let filesystem = if letter.is_empty() {
        "unknown".into()
    } else {
        query_filesystem(&letter).unwrap_or_else(|| "unknown".into())
    };

    let dev_drive = if letter.is_empty() {
        "unknown".into()
    } else {
        query_dev_drive(&letter)
    };

    VolumeInfo {
        root,
        filesystem,
        dev_drive,
    }
}

/// Filesystem name for a drive, via `GetVolumeInformationW`.
///
/// Deliberately not `fsutil fsinfo volumeinfo`, which requires elevation and
/// fails with "Access is denied" for an ordinary user. That would report every
/// volume as "unknown" and silently disable the ReFS check rather than failing
/// it -- the worst outcome for something whose whole job is catching a bad setup.
#[cfg(windows)]
fn query_filesystem(letter: &str) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;

    let root: Vec<u16> = format!("{letter}:\\")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut fs_name = [0u16; 64];
    // SAFETY: `root` is a NUL-terminated UTF-16 string and the output buffer
    // length is passed as its true element count. Every other output is optional
    // and passed as null.
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs_name.as_mut_ptr(),
            fs_name.len() as u32,
        )
    };
    if ok == 0 {
        return None;
    }
    let end = fs_name
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(fs_name.len());
    Some(String::from_utf16_lossy(&fs_name[..end]))
}

#[cfg(not(windows))]
fn query_filesystem(_letter: &str) -> Option<String> {
    None
}

#[cfg(windows)]
fn query_dev_drive(letter: &str) -> String {
    let out = std::process::Command::new("fsutil")
        .args(["devdrv", "query", &format!("{letter}:")])
        .output();
    let Ok(out) = out else {
        return "unknown".into();
    };
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let lower = text.to_ascii_lowercase();
    // Report what we actually know. Claiming "no" when the query was merely
    // denied would be worse than admitting the gap.
    if lower.contains("trusted developer volume") {
        "trusted".into()
    } else if lower.contains("not a developer volume") {
        "no".into()
    } else if lower.contains("access is denied") {
        "unknown (needs elevation)".into()
    } else {
        text.trim().replace('\n', " ").replace('\r', "")
    }
}

#[cfg(not(windows))]
fn query_dev_drive(_letter: &str) -> String {
    "n/a".into()
}

/// The MSVC developer environment, imported the only way it can be: by running
/// vcvarsall and reading back what it set.
#[cfg(windows)]
pub fn msvc_environment() -> Result<HashMap<String, String>> {
    // Already inside a developer prompt: inherit as-is.
    if std::env::var_os("VSCMD_ARG_TGT_ARCH").is_some() {
        return Ok(std::env::vars().collect());
    }

    let pf86 = std::env::var("ProgramFiles(x86)")
        .context("ProgramFiles(x86) is not set; is this really Windows?")?;
    let vswhere = PathBuf::from(pf86).join("Microsoft Visual Studio/Installer/vswhere.exe");
    if !vswhere.exists() {
        bail!("vswhere.exe not found at {}", vswhere.display());
    }

    let install = run_capture(
        &vswhere,
        &[
            "-latest".into(),
            "-products".into(),
            "*".into(),
            "-requires".into(),
            "Microsoft.VisualStudio.Component.VC.Tools.x86.x64".into(),
            "-property".into(),
            "installationPath".into(),
        ],
        None,
        &HashMap::new(),
        Duration::from_secs(60),
    )?;
    let install = install.trim();
    if install.is_empty() {
        bail!("vswhere found no Visual Studio install with the C++ toolset");
    }

    let vcvars = PathBuf::from(install).join("VC/Auxiliary/Build/vcvarsall.bat");
    if !vcvars.exists() {
        bail!("vcvarsall.bat not found at {}", vcvars.display());
    }

    // vcvarsall only exports into its own cmd process, so run it and read the
    // resulting environment back out.
    //
    // Via a temp batch file rather than `cmd /c "..."`: the VS install path
    // contains spaces, and cmd's rules for quotes inside /c strings mangle it.
    // A batch file has no such ambiguity.
    let batch =
        std::env::temp_dir().join(format!("sccache-bench-vcvars-{}.bat", std::process::id()));
    let script = format!(
        "@echo off\r\n\
         call \"{}\" x64 >nul\r\n\
         if errorlevel 1 exit /b 1\r\n\
         set\r\n",
        vcvars.display()
    );
    std::fs::write(&batch, script).with_context(|| format!("writing {}", batch.display()))?;

    let out = std::process::Command::new("cmd")
        .arg("/c")
        .arg(&batch)
        .output()
        .context("failed to run vcvarsall");
    let _ = std::fs::remove_file(&batch);
    let out = out?;

    if !out.status.success() {
        bail!(
            "vcvarsall failed (exit {:?}).\nstdout: {}\nstderr: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let mut env: HashMap<String, String> = HashMap::new();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            env.insert(k.to_string(), v.to_string());
        }
    }
    if !env.contains_key("VSCMD_ARG_TGT_ARCH") {
        bail!("vcvarsall ran but did not set VSCMD_ARG_TGT_ARCH");
    }
    Ok(env)
}

#[cfg(not(windows))]
pub fn msvc_environment() -> Result<HashMap<String, String>> {
    bail!("this harness targets MSVC on Windows")
}

/// Resolve a tool against the PATH of the given environment.
pub fn which_in_env(tool: &str, env: &HashMap<String, String>) -> Option<PathBuf> {
    let path = env
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("PATH"))
        .map(|(_, v)| v.clone())?;
    let exts: Vec<String> = if cfg!(windows) {
        env.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("PATHEXT"))
            .map(|(_, v)| v.split(';').map(|s| s.to_ascii_lowercase()).collect())
            .unwrap_or_else(|| vec![".exe".into()])
    } else {
        vec![String::new()]
    };

    for dir in path.split(if cfg!(windows) { ';' } else { ':' }) {
        if dir.is_empty() {
            continue;
        }
        for ext in &exts {
            let candidate = Path::new(dir).join(format!("{tool}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::drive_letter;
    use std::path::Path;

    #[test]
    fn drive_letter_handles_extended_length_paths() {
        assert_eq!(drive_letter(Path::new(r"D:\cache")).as_deref(), Some("D"));
        assert_eq!(
            drive_letter(Path::new(r"\\?\D:\cache")).as_deref(),
            Some("D")
        );
        assert_eq!(drive_letter(Path::new(r"d:/cache")).as_deref(), Some("D"));
        // UNC paths have no drive letter; reporting one would be a lie.
        assert_eq!(drive_letter(Path::new(r"\\?\UNC\server\share")), None);
        assert_eq!(drive_letter(Path::new(r"\\server\share")), None);
        assert_eq!(drive_letter(Path::new("relative")), None);
    }
}
