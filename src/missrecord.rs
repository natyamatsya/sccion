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
pub const RING_CAPACITY: usize = 4096;

/// The variable naming a file every record is also appended to.
pub const MISS_LOG_VAR: &str = "SCCACHE_MISS_LOG";

/// Ask for [`KeyComponents`] on every record.
pub const TRACE_COMPONENTS_VAR: &str = "SCCACHE_TRACE_COMPONENTS";

/// Record cache *hits* as well as misses.
pub const TRACE_ALL_VAR: &str = "SCCACHE_TRACE_ALL";

/// Whether hits are recorded too.
///
/// Off by default because the interesting event is a miss, and a warm build
/// that hits 2,557 times would push everything worth keeping out of the ring.
///
/// On, the records become a full `(input, key, outcome)` trace of a build --
/// which is the whole of a key-stability check. Capture one, change sccache,
/// capture another, and diff the keys: any key that moved is either the point
/// of the change or a bug in it. That check is otherwise a person remembering
/// what two counters said an hour ago.
pub fn trace_all(env_vars: &[(OsString, OsString)]) -> bool {
    flag(env_vars, TRACE_ALL_VAR)
}

fn flag(env_vars: &[(OsString, OsString)], name: &str) -> bool {
    env_vars
        .iter()
        .find(|(k, _)| k == OsStr::new(name))
        .map(|(_, v)| {
            let v = v.to_string_lossy().to_lowercase();
            matches!(v.as_str(), "1" | "true" | "on")
        })
        .unwrap_or(false)
}

/// Whether to break every key into its components.
///
/// Off unless asked for, and asked for per compilation rather than per daemon,
/// so that one build can be traced without slowing every other build sharing
/// the machine. It is a second pass over everything that goes into a key --
/// including the preprocessed text, which can be megabytes -- and that is a real
/// cost on the hit path, which otherwise does no hashing at all.
///
/// The workflow it exists for: two checkouts are not sharing, so trace both,
/// compare, and turn it off again.
pub fn trace_components(env_vars: &[(OsString, OsString)]) -> bool {
    flag(env_vars, TRACE_COMPONENTS_VAR)
}

/// What became of one compilation's cache lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Classification {
    /// Served from cache. Only recorded under `SCCACHE_TRACE_ALL`, since a
    /// build that hits everything would otherwise fill the ring with the
    /// uninteresting case.
    Hit,
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
    /// The cache answered with something the compiler would not have produced.
    ///
    /// The one classification that is an alarm rather than an observation:
    /// every other kind of miss costs time, and this costs correctness. Only
    /// produced by `SCCACHE_VERIFY_HITS`.
    WrongHit,
    /// This compilation can never be cached, for the reason named. Retrying it
    /// unchanged reaches the same verdict, which is what separates this from
    /// every other variant.
    Uncacheable(String),
}

impl Classification {
    /// A stable token for the log file.
    pub fn as_token(&self) -> String {
        match self {
            Classification::Hit => "hit".into(),
            Classification::WrongHit => "wrong_hit".into(),
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

/// The pieces a cache key is made of, each digested on its own.
///
/// A key is one opaque string, so two keys either match or do not and there is
/// nothing in between. That is exactly the wrong shape for the question asked
/// most often when something is wrong with the cache: *why* do these two
/// differ. Answering it for two worktrees of one project took, on 2026-08-09,
/// dumping two logs, comparing key sets by hand and correlating the result
/// against `compile_commands.json`. With these it is a field-by-field
/// comparison.
///
/// Digests rather than values, deliberately. Localising a difference to "the
/// arguments differ" or "the inputs differ" is most of the answer and costs
/// about two hundred bytes; carrying the arguments themselves, or a
/// preprocessed translation unit, costs megabytes and is only occasionally what
/// anyone wants.
///
/// **Computed separately from the key, never woven into it.** Deriving the key
/// from these would be tidier and would move every cache key in existence. The
/// property that matters is only that a component differs when its section of
/// the input differs, which an independent pass over the same bytes gives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyComponents {
    /// Compiler identity: its digest, the language, and the cache format.
    pub compiler: String,
    /// The arguments as hashed -- basedir-stripped, so two checkouts agree.
    pub arguments: String,
    /// `extra_hash_files`, and anything else folded in wholesale.
    pub extra_files: String,
    /// The environment variables sccache considers part of a compilation.
    pub env: String,
    /// The working directory, when it is in the key at all.
    pub working_directory: String,
    /// What was compiled: preprocessed text, or the reported input list.
    pub inputs: String,
}

impl KeyComponents {
    /// The names of the components that differ between two keys.
    ///
    /// Empty when they agree everywhere, which for two different keys means the
    /// difference is in something not broken out here -- worth knowing, and the
    /// reason this returns a list rather than an `Option`.
    pub fn differences(&self, other: &KeyComponents) -> Vec<&'static str> {
        let mut out = Vec::new();
        for (name, a, b) in [
            ("compiler", &self.compiler, &other.compiler),
            ("arguments", &self.arguments, &other.arguments),
            ("extra_files", &self.extra_files, &other.extra_files),
            ("env", &self.env, &other.env),
            (
                "working_directory",
                &self.working_directory,
                &other.working_directory,
            ),
            ("inputs", &self.inputs, &other.inputs),
        ] {
            if a != b {
                out.push(name);
            }
        }
        out
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
    /// The cache key that was looked up. Absent when the compilation was
    /// refused before one could be computed.
    pub key: Option<String>,
    /// The output, as the rest of sccache describes it -- usually a basename,
    /// and the compiler's own name when the arguments were never parsed.
    pub output: String,
    /// The primary input, basedir-relative. Absent for compilations that have
    /// no single one, and for those whose arguments were never parsed.
    pub input: Option<String>,
    /// The language, as `--show-stats` names it. Absent when the arguments were
    /// never parsed far enough to say.
    pub language: Option<String>,
    /// The checkout this compilation ran in, basedir-relative.
    pub workspace: Option<String>,
    /// Who asked, when they said. See `SCCACHE_AGENT_ID`.
    pub agent: Option<String>,
    /// A digest of the invocation's arguments, basedir-stripped, so that two
    /// records can be compared for "was this even the same command" without
    /// carrying the whole command line.
    pub command_digest: Option<String>,
    /// What the key was made of, when component tracing was on.
    ///
    /// Off by default: filling it means a second pass over everything that goes
    /// into a key, including the preprocessed text, which is real work on a path
    /// that otherwise does none. See `SCCACHE_TRACE_COMPONENTS`.
    #[serde(default)]
    pub components: Option<KeyComponents>,
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
            opt(&self.key),
            self.output,
            opt(&self.input),
            opt(&self.language),
            opt(&self.workspace),
            opt(&self.agent),
            opt(&self.command_digest),
            self.seq,
        )
    }
}

/// A digest of an invocation's arguments, with basedirs stripped.
///
/// The question a record has to answer is "was this even the same invocation",
/// and the whole argument vector is both enormous and full of paths. Stripping
/// basedirs first is what makes the answer the same in two checkouts of one
/// project, exactly as it does for the cache keys themselves.
pub fn command_digest(arguments: &[OsString], basedirs: &[Vec<u8>]) -> String {
    let mut digest = crate::util::Digest::new();
    for arg in arguments {
        digest.update(&crate::util::strip_basedirs_in_arg(
            arg.as_encoded_bytes(),
            basedirs,
        ));
        digest.update(b"\0");
    }
    digest.finish()
}

/// A path as a record should carry it: relative to a basedir when one covers
/// it, so that records from two checkouts can be compared at all.
pub fn relative_to_basedirs(path: &Path, basedirs: &[Vec<u8>]) -> String {
    String::from_utf8_lossy(&crate::util::strip_basedirs_in_arg(
        path.as_os_str().as_encoded_bytes(),
        basedirs,
    ))
    .into_owned()
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
        self.recent_where(limit, |_| true)
    }

    /// As [`MissLog::recent`], keeping only records the predicate accepts.
    ///
    /// The filter runs before the limit, so a busy caller's records cannot
    /// crowd a quiet one's out of an answer scoped to the quiet one.
    pub fn recent_where(
        &self,
        limit: usize,
        keep: impl Fn(&MissRecord) -> bool,
    ) -> Vec<MissRecord> {
        let ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        let matching: Vec<&MissRecord> = ring.iter().filter(|r| keep(r)).collect();
        matching
            .iter()
            .skip(matching.len().saturating_sub(limit))
            .map(|r| (*r).clone())
            .collect()
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
                        warn!(
                            "Failed to open {} at {}: {}",
                            MISS_LOG_VAR,
                            path.display(),
                            e
                        );
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
            key: Some("key".into()),
            output: output.into(),
            input: None,
            language: Some("C/C++".into()),
            workspace: None,
            agent: None,
            command_digest: None,
            components: None,
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

    /// Scoping has to filter before it limits. Filtering after would let a busy
    /// neighbour's records fill the window and hand back nothing for the
    /// workspace actually asked about -- an empty answer that looks like "no
    /// misses" rather than "your records were crowded out".
    #[test]
    fn scoped_records_are_filtered_before_they_are_limited() {
        let log = MissLog::default();
        // One record for "mine", then a wall of somebody else's.
        let mut mine = record("mine.obj");
        mine.workspace = Some("D:/mine".into());
        log.record(mine, None);
        for i in 0..100 {
            let mut theirs = record(&format!("theirs{i}.obj"));
            theirs.workspace = Some("D:/theirs".into());
            log.record(theirs, None);
        }

        let got = log.recent_where(10, |r| r.workspace.as_deref() == Some("D:/mine"));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].output, "mine.obj");
    }

    #[test]
    fn the_sink_is_read_from_the_compilations_environment() {
        let env = vec![
            (OsString::from("PATH"), OsString::from("/bin")),
            (
                OsString::from(MISS_LOG_VAR),
                OsString::from("D:/misses.tsv"),
            ),
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
        assert_eq!(
            cols[6], "-",
            "an absent field is a dash, never an empty column"
        );
    }
}
