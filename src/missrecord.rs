// SPDX-FileCopyrightText: Copyright natyamatsya and sccache contributors
// SPDX-License-Identifier: Apache-2.0

//! What sccache knows about a compilation it did not serve from cache.
//!
//! A miss is the interesting event when tuning a cache, and the one event
//! `--show-stats` counts but cannot explain: it reports 141 misses without
//! saying which translation units, or why. The debug log says which, but only
//! inside a trace of every compilation -- enormous, and slow enough to change
//! what is being measured.
//!
//! Three things shape what a record holds.
//!
//! **It must name the compilation, not the output basename.** Three concurrent
//! `mocs_compilation.cpp.obj` lines interleave and cannot be told apart, which
//! has already sent two diagnoses to the wrong target. The input path can tell
//! them apart; the output basename cannot.
//!
//! **It must distinguish "not this time" from "not ever".** A caller told
//! `cold` should try again after the cache warms. A caller told
//! `uncacheable: time_macros` should stop, because every retry reaches the same
//! answer. Those want completely different responses and the counters cannot
//! tell them apart.
//!
//! **Recorded paths go through the same basedir stripping the cache keys do.**
//! Otherwise two checkouts of one project produce records that cannot be
//! compared, which is the trap manifest keys fell into before `7ea3117`.
//!
//! Records live in a bounded ring in the daemon, so the last few thousand can
//! be asked about without anything being enabled in advance. When a build wants
//! all of them, `SCCACHE_MISS_LOG` names a file and the ring's bound stops
//! being the limit.

use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// How many records the daemon keeps.
///
/// Deliberately a constant rather than a setting. The ring exists so that the
/// recent past can always be asked about; a build that wants every record wants
/// `SCCACHE_MISS_LOG`, which is unbounded and is the honest answer to "I need
/// all of them". Sizing knob and file sink would be two ways to say one thing.
/// At roughly 300 bytes a record this is about a megabyte, and it covers a cold
/// build of a 2,500 translation unit project with room to spare.
const RING_CAPACITY: usize = 4096;

/// The variable naming a file every record is also appended to.
pub const MISS_LOG_VAR: &str = "SCCACHE_MISS_LOG";

/// Why the compilation was not served from cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Classification {
    /// The key simply was not there.
    Cold,
    /// The entry was there and could not be read back.
    ReadError,
    /// The cache did not answer in time.
    TimedOut,
    /// The caller asked for a recompile.
    ForcedRecache,
    /// The caller asked for the result not to be cached.
    ForcedNoCache,
    /// This compilation can never be cached, for the reason named. Retrying it
    /// unchanged reaches the same verdict, which is what separates this from
    /// every other variant.
    Uncacheable(String),
}

impl Classification {
    /// A stable token for the log file.
    pub fn as_token(&self) -> String {
        match self {
            Classification::Cold => "cold".into(),
            Classification::ReadError => "read_error".into(),
            Classification::TimedOut => "timed_out".into(),
            Classification::ForcedRecache => "forced_recache".into(),
            Classification::ForcedNoCache => "forced_no_cache".into(),
            Classification::Uncacheable(why) => format!("uncacheable:{why}"),
        }
    }
}

/// Whether preprocessor cache mode was in play, and what it said.
///
/// This is the discriminator worth having. A direct-mode *hit* yields the object
/// key straight out of the preprocessor cache entry, so a miss after one means
/// the object was evicted or never stored -- nothing about the source changed. A
/// direct-mode *miss* means the inputs were re-preprocessed and hashed afresh,
/// so the key moved because the content did. Those two want completely
/// different fixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DirectState {
    Hit,
    Miss,
    /// Preprocessor cache mode never engaged, which is materially different from
    /// it engaging and missing.
    Off,
}

impl DirectState {
    pub fn as_str(self) -> &'static str {
        match self {
            DirectState::Hit => "direct-hit",
            DirectState::Miss => "direct-miss",
            DirectState::Off => "direct-off",
        }
    }
}

impl From<Option<bool>> for DirectState {
    fn from(hit: Option<bool>) -> Self {
        match hit {
            Some(true) => DirectState::Hit,
            Some(false) => DirectState::Miss,
            None => DirectState::Off,
        }
    }
}

/// One compilation that was not served from cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissRecord {
    /// Assigned by the daemon in the order records arrive. There is no wall
    /// clock here on purpose: this is written on the compile path, and a
    /// timestamp costs more than it is worth for ordering within one build.
    pub seq: u64,
    pub classification: Classification,
    pub direct: DirectState,
    /// The cache key that was looked up.
    pub key: String,
    /// The output, as the rest of sccache describes it -- usually a basename.
    pub output: String,
    /// The primary input, basedir-relative. Absent for compilations that have
    /// no single one.
    pub input: Option<String>,
    /// The language, as `--show-stats` names it.
    pub language: String,
    /// The checkout this compilation ran in, basedir-relative.
    pub workspace: Option<String>,
    /// Who asked, when they said. See `SCCACHE_AGENT_ID`.
    pub agent: Option<String>,
    /// A digest of the invocation's arguments, basedir-stripped, so that two
    /// records can be compared for "was this even the same command" without
    /// carrying the whole command line.
    pub command_digest: Option<String>,
}

impl MissRecord {
    /// The line written to `SCCACHE_MISS_LOG`.
    ///
    /// Tab separated, one record per line, and the columns are appended to
    /// rather than reordered when a field is added, so a one-liner that reads
    /// column 3 for the key keeps reading the key.
    fn to_line(&self) -> String {
        let opt = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".into());
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            self.classification.as_token(),
            self.direct.as_str(),
            self.key,
            self.output,
            opt(&self.input),
            self.language,
            opt(&self.workspace),
            opt(&self.agent),
            opt(&self.command_digest),
            self.seq,
        )
    }
}

/// The daemon's bounded ring of recent misses, plus any files it mirrors them
/// to.
#[derive(Debug, Default)]
pub struct MissLog {
    ring: Mutex<VecDeque<MissRecord>>,
    next_seq: AtomicU64,
    /// Open sinks, by path. A map rather than one handle because two concurrent
    /// builds may each name their own log, and because the path comes from the
    /// compilation rather than from the daemon -- see [`sink_from_env`].
    sinks: Mutex<HashMap<PathBuf, std::fs::File>>,
}

impl MissLog {
    /// Record one miss. `sink` is the file it is also appended to, if any.
    pub fn record(&self, mut record: MissRecord, sink: Option<&Path>) {
        record.seq = self.next_seq.fetch_add(1, Ordering::Relaxed);

        if let Some(path) = sink {
            self.append(path, &record);
        }

        let mut ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        if ring.len() == RING_CAPACITY {
            ring.pop_front();
        }
        ring.push_back(record);
    }

    /// The most recent records, newest last, at most `limit` of them.
    pub fn recent(&self, limit: usize) -> Vec<MissRecord> {
        let ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        ring.iter().skip(ring.len().saturating_sub(limit)).cloned().collect()
    }

    fn append(&self, path: &Path, record: &MissRecord) {
        let mut sinks = self.sinks.lock().unwrap_or_else(|e| e.into_inner());
        let file = match sinks.get_mut(path) {
            Some(file) => file,
            None => {
                match std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    Ok(f) => sinks.entry(path.to_path_buf()).or_insert(f),
                    Err(e) => {
                        warn!("Failed to open {} at {}: {}", MISS_LOG_VAR, path.display(), e);
                        return;
                    }
                }
            }
        };
        // Errors are dropped deliberately: a diagnostic that can fail a
        // compilation is worse than one that occasionally loses a line.
        let _ = writeln!(file, "{}", record.to_line());
    }
}

/// The file a compilation asks its misses to be logged to.
///
/// Read from the environment the *client sent with the request*, never from the
/// daemon's own. The daemon is started by whichever build got there first and
/// then outlives it, so a variable read from its environment does nothing unless
/// that shell happened to start the server, and stops working after an idle
/// timeout. Every other per-compilation variable is read this way.
pub fn sink_from_env(env_vars: &[(OsString, OsString)]) -> Option<PathBuf> {
    env_vars
        .iter()
        .find(|(k, _)| k == OsStr::new(MISS_LOG_VAR))
        .map(|(_, v)| PathBuf::from(v))
        .filter(|p| !p.as_os_str().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(output: &str) -> MissRecord {
        MissRecord {
            seq: 0,
            classification: Classification::Cold,
            direct: DirectState::Off,
            key: "key".into(),
            output: output.into(),
            input: None,
            language: "C/C++".into(),
            workspace: None,
            agent: None,
            command_digest: None,
        }
    }

    #[test]
    fn the_ring_keeps_the_most_recent_and_drops_the_oldest() {
        let log = MissLog::default();
        for i in 0..RING_CAPACITY + 10 {
            log.record(record(&format!("out{i}")), None);
        }

        let recent = log.recent(RING_CAPACITY * 2);
        assert_eq!(recent.len(), RING_CAPACITY, "the ring is bounded");
        assert_eq!(
            recent.last().unwrap().output,
            format!("out{}", RING_CAPACITY + 9),
            "and it is the newest that survive"
        );
        assert_eq!(recent.first().unwrap().output, "out10");
    }

    /// Sequence numbers order records within a build, which is the only thing
    /// they are for -- so they must keep counting past the ring's capacity.
    #[test]
    fn sequence_numbers_do_not_restart_when_the_ring_wraps() {
        let log = MissLog::default();
        for i in 0..RING_CAPACITY + 5 {
            log.record(record(&format!("out{i}")), None);
        }

        let recent = log.recent(1);
        assert_eq!(recent[0].seq as usize, RING_CAPACITY + 4);
    }

    #[test]
    fn the_sink_is_read_from_the_compilations_environment() {
        let env = vec![
            (OsString::from("PATH"), OsString::from("/bin")),
            (OsString::from(MISS_LOG_VAR), OsString::from("D:/misses.tsv")),
        ];
        assert_eq!(sink_from_env(&env), Some(PathBuf::from("D:/misses.tsv")));

        // An empty value is how a shell says "unset" without unsetting, and
        // opening a file named "" would only produce a warning per compilation.
        let empty = vec![(OsString::from(MISS_LOG_VAR), OsString::new())];
        assert_eq!(sink_from_env(&empty), None);
        assert_eq!(sink_from_env(&[]), None);
    }

    #[test]
    fn a_record_is_written_as_one_tab_separated_line() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("misses.tsv");
        let log = MissLog::default();

        let mut r = record("foo.obj");
        r.input = Some("src/foo.c".into());
        r.classification = Classification::Uncacheable("time_macros".into());
        log.record(r, Some(&path));
        log.record(record("bar.obj"), Some(&path));

        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2, "the file is appended to, not rewritten");

        let cols: Vec<_> = lines[0].split('\t').collect();
        assert_eq!(cols[0], "uncacheable:time_macros");
        assert_eq!(cols[1], "direct-off");
        assert_eq!(cols[2], "key", "the key stays in column 3");
        assert_eq!(cols[3], "foo.obj");
        assert_eq!(cols[4], "src/foo.c");
        assert_eq!(cols[6], "-", "an absent field is a dash, never an empty column");
    }
}
