// SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
// SPDX-License-Identifier: Apache-2.0

//! Running child processes with visibility.
//!
//! A benchmark that hangs is worse than one that fails: it burns wall clock and
//! tells you nothing about where it stopped. Everything here exists so that a
//! stuck build reports *what* it was doing when it stuck, and dies on a deadline
//! rather than waiting for someone to notice.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};

/// How the child finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    Exited(i32),
    /// Killed after exceeding the hard timeout.
    TimedOut,
}

#[derive(Debug)]
pub struct RunOutcome {
    pub completion: Completion,
    pub elapsed: Duration,
    /// Combined stdout+stderr, in arrival order. Kept for error reporting.
    pub output: Vec<String>,
    /// Last line the child emitted, whatever stream it came from.
    pub last_line: Option<String>,
    /// Longest observed gap between output lines. A large value on an otherwise
    /// successful run is the signature of a stall that resolved itself.
    pub longest_silence: Duration,
}

impl RunOutcome {
    pub fn success(&self) -> bool {
        self.completion == Completion::Exited(0)
    }

    pub fn tail(&self, n: usize) -> String {
        let start = self.output.len().saturating_sub(n);
        self.output[start..].join("\n")
    }
}

/// Knobs for how loudly and how long we are willing to wait.
#[derive(Debug, Clone)]
pub struct Supervision {
    /// Print a progress line this often while the child runs.
    pub heartbeat: Duration,
    /// Kill the child (and its descendants) after this long.
    pub timeout: Duration,
    /// Flag the child as stalled after this long with no output. Does not kill
    /// it -- some legitimate steps are quiet -- but makes the silence visible.
    pub stall: Duration,
    /// Label used in progress lines.
    pub label: String,
}

/// How a child's output should be collected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capture {
    /// Stream through pipes, so output is available live for the heartbeat.
    ///
    /// Only safe for children that do not outlive us. A process that daemonises
    /// inherits these pipe handles; if nothing drains them it blocks forever once
    /// the 64 KiB pipe buffer fills, taking the daemon down with it.
    Streamed,
    /// Redirect to a temp file and read it once the child exits.
    ///
    /// Costs live progress, but a file can always be written to. Use this for
    /// anything that may spawn a daemon -- notably `sccache --start-server`.
    ViaFile,
}

struct Shared {
    lines: Mutex<Vec<String>>,
    last_at: AtomicU64,
    longest_silence_ms: AtomicU64,
}

/// Run `program` with `args`, reporting progress and killing it if it outlives
/// `sv.timeout`.
pub fn run_supervised(
    program: &Path,
    args: &[String],
    cwd: Option<&Path>,
    env: &HashMap<String, String>,
    sv: &Supervision,
    capture: Capture,
) -> Result<RunOutcome> {
    let started = Instant::now();
    info!(
        label = %sv.label,
        command = %format!("{} {}", program.display(), args.join(" ")),
        "spawn"
    );

    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::null());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }

    // A unique sink per run: concurrent harnesses must not share one.
    let sink = (capture == Capture::ViaFile).then(|| {
        std::env::temp_dir().join(format!(
            "sccache-bench-{}-{}-{}.out",
            std::process::id(),
            sv.label.replace(|c: char| !c.is_alphanumeric(), "_"),
            started.elapsed().as_nanos()
        ))
    });

    match &sink {
        Some(path) => {
            let out = std::fs::File::create(path)
                .with_context(|| format!("creating {}", path.display()))?;
            let err = out.try_clone()?;
            cmd.stdout(Stdio::from(out)).stderr(Stdio::from(err));
        }
        None => {
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        }
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn {}", program.display()))?;
    let pid = child.id();

    let shared = Arc::new(Shared {
        lines: Mutex::new(Vec::new()),
        last_at: AtomicU64::new(0),
        longest_silence_ms: AtomicU64::new(0),
    });

    // Both streams are drained on their own threads. Reading them inline would
    // block the child as soon as it out-talked us, which looks exactly like a
    // hang and is the classic way to deadlock on this.
    let mut readers = Vec::new();
    for (stream, which) in [
        (
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
            "out",
        ),
        (
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
            "err",
        ),
    ]
    .into_iter()
    .filter_map(|(s, w)| s.map(|s| (s, w)))
    {
        let shared = Arc::clone(&shared);
        let origin = started;
        let label = sv.label.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                let now_ms = origin.elapsed().as_millis() as u64;
                let prev = shared.last_at.swap(now_ms, Ordering::Relaxed);
                shared
                    .longest_silence_ms
                    .fetch_max(now_ms.saturating_sub(prev), Ordering::Relaxed);
                debug!(label = %label, stream = which, "{line}");
                if let Ok(mut lines) = shared.lines.lock() {
                    lines.push(line);
                }
            }
        }));
    }

    let mut last_beat = Instant::now();
    let completion = loop {
        match child.try_wait().context("failed to poll child")? {
            Some(status) => break Completion::Exited(status.code().unwrap_or(-1)),
            None => {
                let elapsed = started.elapsed();
                if elapsed >= sv.timeout {
                    warn!(
                        label = %sv.label,
                        pid,
                        seconds = elapsed.as_secs_f64(),
                        "hard timeout; killing process tree"
                    );
                    eprintln!(
                        "  [{}] hard timeout after {:.0}s -- killing process tree (pid {})",
                        sv.label,
                        elapsed.as_secs_f64(),
                        pid
                    );
                    kill_tree(pid, &mut child);
                    break Completion::TimedOut;
                }
                if last_beat.elapsed() >= sv.heartbeat {
                    emit_heartbeat(&shared, sv, elapsed);
                    last_beat = Instant::now();
                }
                std::thread::sleep(Duration::from_millis(150));
            }
        }
    };

    for r in readers {
        let _ = r.join();
    }

    // Collect a file sink now that the child is gone.
    if let Some(path) = &sink {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(mut lines) = shared.lines.lock()
        {
            for line in text.lines() {
                debug!(label = %sv.label, stream = "file", "{line}");
                lines.push(line.to_string());
            }
        }
        let _ = std::fs::remove_file(path);
    }

    let lines = shared
        .lines
        .lock()
        .map(|l| l.clone())
        .unwrap_or_else(|e| e.into_inner().clone());
    let last_line = lines.iter().rev().find(|l| !l.trim().is_empty()).cloned();
    let longest_silence = Duration::from_millis(shared.longest_silence_ms.load(Ordering::Relaxed));
    let elapsed = started.elapsed();

    info!(
        label = %sv.label,
        outcome = ?completion,
        seconds = elapsed.as_secs_f64(),
        longest_silence = longest_silence.as_secs_f64(),
        "finished"
    );

    Ok(RunOutcome {
        completion,
        elapsed,
        last_line,
        longest_silence,
        output: lines,
    })
}

fn emit_heartbeat(shared: &Shared, sv: &Supervision, elapsed: Duration) {
    let last_at_ms = shared.last_at.load(Ordering::Relaxed);
    let silent_for = elapsed.saturating_sub(Duration::from_millis(last_at_ms));
    let stalled = silent_for >= sv.stall;

    let recent = shared
        .lines
        .lock()
        .ok()
        .and_then(|l| l.iter().rev().find(|s| !s.trim().is_empty()).cloned())
        .unwrap_or_else(|| "(no output yet)".into());

    info!(
        label = %sv.label,
        seconds = elapsed.as_secs_f64(),
        budget = sv.timeout.as_secs_f64(),
        silent_for = silent_for.as_secs_f64(),
        stalled,
        "alive"
    );

    let mut msg = format!(
        "  [{}] {:.0}s / {:.0}s  {}",
        sv.label,
        elapsed.as_secs_f64(),
        sv.timeout.as_secs_f64(),
        truncate(recent.trim(), 88)
    );
    if stalled {
        msg.push_str(&format!(" [STALLED {:.0}s]", silent_for.as_secs_f64()));
    }
    eprintln!("{msg}");
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let head: String = s.chars().take(max.saturating_sub(3)).collect();
    format!("{head}...")
}

/// Kill the child *and everything it spawned*.
///
/// Killing only the direct child is not enough: ninja's compiler processes would
/// survive, keep the cache and build directories busy, and quietly poison the next
/// scenario.
fn kill_tree(pid: u32, child: &mut std::process::Child) {
    #[cfg(windows)]
    {
        // /T takes the tree, /F skips the polite request. Best effort: if taskkill
        // is unavailable we still fall through to the direct kill.
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = pid; // the direct kill below is all we do off Windows
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Run a command expected to be quick and to succeed, returning its output.
///
/// Uses [`Capture::ViaFile`], since most of these are sccache control commands and
/// any of them may end up spawning the daemon.
pub fn run_capture(
    program: &Path,
    args: &[String],
    cwd: Option<&Path>,
    env: &HashMap<String, String>,
    timeout: Duration,
) -> Result<String> {
    let sv = Supervision {
        heartbeat: Duration::from_secs(15),
        timeout,
        stall: Duration::from_secs(15),
        label: program
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "cmd".into()),
    };
    let out = run_supervised(program, args, cwd, env, &sv, Capture::ViaFile)?;
    if !out.success() {
        bail!(
            "{} {:?} failed ({:?}):\n{}",
            program.display(),
            args,
            out.completion,
            out.tail(40)
        );
    }
    Ok(out.output.join("\n"))
}
