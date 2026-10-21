// SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
// SPDX-License-Identifier: Apache-2.0

//! What the adapter can ask, and who answers it.
//!
//! The trait exists so the dispatcher can be tested without a daemon. It is
//! deliberately a narrow restatement of `sccion::api` rather than a second
//! abstraction over it: every method here is one call there.

use std::path::Path;

use anyhow::Result;
use sccion::api::{EnvReport, MissRecord, ServerInfo, WorkspaceInfo};

pub trait Backend: Send + Sync {
    fn register_workspace(&self, path: &Path) -> Result<WorkspaceInfo>;
    fn describe_workspace(&self, handle: &str) -> Result<WorkspaceInfo>;
    fn stats(&self) -> Result<ServerInfo>;
    fn miss_records(&self, handle: Option<&str>, limit: usize) -> Result<Vec<MissRecord>>;
    fn environment(&self, cwd: &Path) -> EnvReport;
}

/// The real thing: a local sccion daemon.
pub struct DaemonBackend {
    daemon: sccion::api::Daemon,
    config: sccion::config::Config,
}

impl DaemonBackend {
    pub fn new() -> Result<Self> {
        let config = sccion::config::Config::load()?;
        // The daemon refuses too, and it is the authority -- this only turns
        // "every tool call fails" into one clear line at startup, which is what
        // an operator who disabled this on a build server should see.
        if !config.mcp_enabled {
            anyhow::bail!(
                "the MCP query surface is disabled on this machine; \
                 set `mcp = true` in the sccache config or SCCACHE_MCP=true to enable it"
            );
        }
        Ok(DaemonBackend {
            daemon: sccion::api::Daemon::from_config(&config),
            config,
        })
    }
}

impl Backend for DaemonBackend {
    fn register_workspace(&self, path: &Path) -> Result<WorkspaceInfo> {
        self.daemon.register_workspace(path)
    }

    fn describe_workspace(&self, handle: &str) -> Result<WorkspaceInfo> {
        self.daemon.describe_workspace(handle)
    }

    fn stats(&self) -> Result<ServerInfo> {
        self.daemon.stats()
    }

    fn miss_records(&self, handle: Option<&str>, limit: usize) -> Result<Vec<MissRecord>> {
        self.daemon.miss_records(handle, limit)
    }

    fn environment(&self, cwd: &Path) -> EnvReport {
        sccion::api::environment(&self.config, cwd)
    }
}

/// Daemons that answer from a script, so the questions can be tested without
/// one running.
///
/// Here rather than in a test module of their own because both the query layer
/// and the server surface need them, and two fakes that drifted apart would
/// make one of the two test suites quietly meaningless.
#[cfg(test)]
pub mod fake {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use anyhow::Result;
    use sccion::api::{
        Classification, DirectState, EnvReport, MissRecord, ServerInfo, WorkspaceInfo,
    };

    use super::Backend;

    /// One workspace, one uncacheable record, and a handle named `nope` that
    /// nothing minted.
    pub struct Fake;

    impl Backend for Fake {
        fn register_workspace(&self, path: &Path) -> Result<WorkspaceInfo> {
            Ok(WorkspaceInfo {
                handle: "d34db33f".repeat(4),
                root: path.to_string_lossy().into_owned(),
                vcs: "git".into(),
            })
        }

        fn describe_workspace(&self, handle: &str) -> Result<WorkspaceInfo> {
            if handle == "nope" {
                anyhow::bail!("unknown workspace handle");
            }
            Ok(WorkspaceInfo {
                handle: handle.to_owned(),
                root: "D:/wt".into(),
                vcs: "git".into(),
            })
        }

        fn stats(&self) -> Result<ServerInfo> {
            Ok(ServerInfo {
                stats: Default::default(),
                cache_location: "D:/cache".into(),
                cache_size: Some(1),
                max_cache_size: Some(2),
                use_preprocessor_cache_mode: true,
                version: "test".into(),
                basedirs: vec![],
            })
        }

        fn miss_records(&self, handle: Option<&str>, _limit: usize) -> Result<Vec<MissRecord>> {
            if handle == Some("nope") {
                anyhow::bail!("unknown workspace handle");
            }
            Ok(vec![MissRecord {
                seq: 1,
                classification: Classification::Uncacheable("time_macros".into()),
                direct: DirectState::Off,
                key: None,
                output: "foo.obj".into(),
                input: Some("src/foo.c".into()),
                language: Some("C/C++".into()),
                workspace: Some("D:/wt".into()),
                agent: Some("agent-7".into()),
                command_digest: Some("abc".into()),
                components: Some(sccion::api::KeyComponents {
                    compiler: "c0".into(),
                    arguments: "a0".into(),
                    extra_files: "x0".into(),
                    env: "e0".into(),
                    working_directory: "w0".into(),
                    inputs: "i0".into(),
                }),
            }])
        }

        fn environment(&self, _cwd: &Path) -> EnvReport {
            empty_report()
        }
    }

    /// Answers like [`Fake`], but its records carry no key components.
    pub struct Untraced;

    impl Backend for Untraced {
        fn register_workspace(&self, path: &Path) -> Result<WorkspaceInfo> {
            Fake.register_workspace(path)
        }

        fn describe_workspace(&self, handle: &str) -> Result<WorkspaceInfo> {
            Fake.describe_workspace(handle)
        }

        fn stats(&self) -> Result<ServerInfo> {
            Fake.stats()
        }

        fn miss_records(&self, handle: Option<&str>, limit: usize) -> Result<Vec<MissRecord>> {
            let mut r = Fake.miss_records(handle, limit)?;
            r[0].components = None;
            Ok(r)
        }

        fn environment(&self, cwd: &Path) -> EnvReport {
            Fake.environment(cwd)
        }
    }

    /// A daemon's ring: one record per input, oldest first, and a fetch that
    /// returns only the most recent `limit` -- the part of the real daemon a
    /// filter has to see past.
    pub struct Ring(pub Vec<&'static str>);

    impl Backend for Ring {
        fn register_workspace(&self, path: &Path) -> Result<WorkspaceInfo> {
            Fake.register_workspace(path)
        }

        fn describe_workspace(&self, handle: &str) -> Result<WorkspaceInfo> {
            Fake.describe_workspace(handle)
        }

        fn stats(&self) -> Result<ServerInfo> {
            Fake.stats()
        }

        fn miss_records(&self, handle: Option<&str>, limit: usize) -> Result<Vec<MissRecord>> {
            let template = Fake.miss_records(handle, limit)?.remove(0);
            let records: Vec<MissRecord> = self
                .0
                .iter()
                .enumerate()
                .map(|(i, input)| MissRecord {
                    seq: i as u64,
                    input: Some((*input).to_owned()),
                    output: format!("{input}.obj"),
                    ..template.clone()
                })
                .collect();
            let skip = records.len().saturating_sub(limit);
            Ok(records.into_iter().skip(skip).collect())
        }

        fn environment(&self, cwd: &Path) -> EnvReport {
            Fake.environment(cwd)
        }
    }

    /// Records which directory the environment report was asked about.
    #[derive(Default)]
    pub struct Located(Mutex<Option<PathBuf>>);

    impl Located {
        pub fn asked_about(&self) -> Option<PathBuf> {
            self.0.lock().unwrap().clone()
        }
    }

    impl Backend for Located {
        fn register_workspace(&self, path: &Path) -> Result<WorkspaceInfo> {
            Fake.register_workspace(path)
        }

        fn describe_workspace(&self, handle: &str) -> Result<WorkspaceInfo> {
            Fake.describe_workspace(handle)
        }

        fn stats(&self) -> Result<ServerInfo> {
            Fake.stats()
        }

        fn miss_records(&self, handle: Option<&str>, limit: usize) -> Result<Vec<MissRecord>> {
            Fake.miss_records(handle, limit)
        }

        fn environment(&self, cwd: &Path) -> EnvReport {
            *self.0.lock().unwrap() = Some(cwd.to_path_buf());
            empty_report()
        }
    }

    /// Enough of a report to be serialised; the contents are `checkenv`'s
    /// business and are tested there.
    pub fn empty_report() -> EnvReport {
        let volume = sccion::api::VolumeReport {
            path: "D:/wt".into(),
            root: Some("D:\\".into()),
            filesystem: "ReFS".into(),
            dev_drive: Some("trusted".into()),
            supports_block_cloning: true,
            case_sensitive: Some(true),
            spotlight_indexed: None,
        };
        EnvReport {
            sccion_version: "test".into(),
            sccion_path: None,
            cache: volume.clone(),
            working_directory: volume,
            same_volume: true,
            cache_location: "D:/cache".into(),
            max_cache_size: 0,
            compression: "none".into(),
            project_config: None,
            wraps_file: "sccion-wraps.toml".into(),
            preprocessor_cache_mode: sccion::api::Setting {
                name: "use_preprocessor_cache_mode".into(),
                value: true,
                origin: sccion::api::Origin::Machine,
                machine_value: None,
                note: None,
            },
            preprocessor_cache_settings: vec![],
            defender: None,
            findings: vec![],
        }
    }
}
