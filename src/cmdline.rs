// Copyright 2016 Mozilla Foundation
// SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
// SPDX-License-Identifier: Apache-2.0
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::errors::*;
use clap::{Arg, ArgAction, ArgGroup, ValueEnum, error::ErrorKind};
use serde::Deserialize;
use std::collections::HashMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use which::which_in_all;

use crate::envvar::START_SERVER as ENV_VAR_INTERNAL_START_SERVER;

/// The file a copy of sccion started under a tool's name reads from its own
/// directory, to learn which executable it stands in for:
///
/// ```toml
/// [wraps]
/// moc = "C:/Qt/6.8.5/msvc2022_64/bin/moc.exe"
/// ```
///
/// A build system knows which tool it means and the PATH does not, so this is
/// how one says so: it writes the copy and this file side by side, and
/// nothing about the environment the build later runs in can change the
/// answer. A relative path is taken from this file's directory. Without the
/// file, or without an entry for the name, the copy falls back to searching
/// the PATH.
///
/// Read from beside the executable, never by walking up the way the project
/// file is found: whoever can write here can already replace the copy itself,
/// so the file grants nothing a repository or a build directory above it
/// could not otherwise reach.
pub const WRAPS_FILE: &str = "sccion-wraps.toml";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WrapsFile {
    wraps: HashMap<String, PathBuf>,
}

/// Names the tools that sccions further up this process tree stand in for, in
/// the environment of everything they run.
///
/// Read and written only by the masquerade handling below. It is not a
/// setting, and it reaches no cache key: compilers hash only the variables
/// they list.
const MASQUERADING: &str = "SCCION_MASQUERADING";

#[derive(Debug, Clone, ValueEnum, Default)]
pub enum StatsFormat {
    #[default]
    Text,
    Json,
}

impl StatsFormat {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Json => "json",
        }
    }
}

impl FromStr for StatsFormat {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            _ => bail!("Unrecognized stats format: {:?}", s),
        }
    }
}

/// A specific command to run.
pub enum Command {
    /// Show cache statistics and exit.
    ShowStats(StatsFormat, bool),
    /// Run background server.
    InternalStartServer,
    /// Start background server as a subprocess.
    StartServer,
    /// Stop background server.
    StopServer,
    /// Zero cache statistics and exit.
    ZeroStats,
    /// Report the environment sccion will run in, and exit.
    CheckEnv(StatsFormat),
    /// Write a `.sccion.toml` for the tree this was run in, and exit.
    Init,
    /// Write sccion's CMake module for this version, print its directory, and
    /// exit.
    CMakeModuleDir,
    /// Show the status of the distributed client.
    DistStatus,
    /// Perform a login to authenticate for distributed compilation.
    DistAuth,
    /// Package a toolchain for distributed compilation (executable, out)
    PackageToolchain(PathBuf, PathBuf),
    /// Run a compiler command.
    Compile {
        /// The binary to execute.
        exe: OsString,
        /// The commandline arguments to pass to `exe`.
        cmdline: Vec<OsString>,
        /// The directory in which to execute the command.
        cwd: PathBuf,
        /// The environment variables to use for execution.
        env_vars: Vec<(OsString, OsString)>,
    },
    DebugPreprocessorCacheEntries,
}

fn flag_infer_long_and_short(name: &'static str) -> Arg {
    flag_infer_long(name).short(name.chars().next().expect("Name needs at least one char"))
}

fn flag_infer_long(name: &'static str) -> Arg {
    Arg::new(name).long(name)
}

/// Get the [`clap::Command`] used for argument parsing.
fn get_clap_command() -> clap::Command {
    clap::Command::new(env!("CARGO_PKG_NAME"))
        .version(env!("CARGO_PKG_VERSION"))
        .max_term_width(110)
        .after_help(concat!(
            "Enabled features:\n",
            "    S3:        ",
            cfg!(feature = "s3"),
            "\n",
            "    Redis:     ",
            cfg!(feature = "redis"),
            "\n",
            "    Memcached: ",
            cfg!(feature = "memcached"),
            "\n",
            "    GCS:       ",
            cfg!(feature = "gcs"),
            "\n",
            "    GHA:       ",
            cfg!(feature = "gha"),
            "\n",
            "    Azure:     ",
            cfg!(feature = "azure"),
            "\n",
            "    WebDAV:    ",
            cfg!(feature = "webdav"),
            "\n",
            "    OSS:       ",
            cfg!(feature = "oss"),
            "\n",
            "    COS:       ",
            cfg!(feature = "cos"),
            "\n"
        ))
        .args(&[
            flag_infer_long_and_short("show-stats")
                .help("show cache statistics")
                .action(ArgAction::SetTrue),
            flag_infer_long("show-adv-stats")
                .help("show advanced cache statistics")
                .action(ArgAction::SetTrue),
            flag_infer_long("start-server")
                .help("start background server")
                .action(ArgAction::SetTrue),
            flag_infer_long("debug-preprocessor-cache")
                .help("show all preprocessor cache entries")
                .action(ArgAction::SetTrue),
            flag_infer_long("stop-server")
                .help("stop background server")
                .action(ArgAction::SetTrue),
            flag_infer_long_and_short("zero-stats")
                .help("zero statistics counters")
                .action(ArgAction::SetTrue),
            flag_infer_long("check-env")
                .help("report the environment sccion will run in and exit")
                .action(ArgAction::SetTrue),
            flag_infer_long("init")
                .help("write a .sccion.toml for this checkout and exit")
                .action(ArgAction::SetTrue),
            flag_infer_long("cmake-module-dir")
                .help("write sccion's CMake module for this version, print its directory and exit")
                .action(ArgAction::SetTrue),
            flag_infer_long("dist-auth")
                .help("authenticate for distributed compilation")
                .action(ArgAction::SetTrue),
            flag_infer_long("dist-status")
                .help("show status of the distributed client")
                .action(ArgAction::SetTrue),
            flag_infer_long("package-toolchain")
                .help("package toolchain for distributed compilation")
                .value_parser(clap::value_parser!(PathBuf))
                .num_args(2)
                .value_names(["EXE", "OUT"]),
            flag_infer_long("stats-format")
                .help("set output format of statistics")
                .value_name("FMT")
                .value_parser(clap::value_parser!(StatsFormat))
                .default_value(StatsFormat::default().as_str()),
            Arg::new("CMD")
                .value_parser(clap::value_parser!(OsString))
                .trailing_var_arg(true)
                .action(ArgAction::Append),
        ])
        .group(
            ArgGroup::new("one_and_only_one")
                .args([
                    "dist-auth",
                    "debug-preprocessor-cache",
                    "dist-status",
                    "show-stats",
                    "show-adv-stats",
                    "start-server",
                    "stop-server",
                    "zero-stats",
                    "check-env",
                    "init",
                    "cmake-module-dir",
                    "package-toolchain",
                    "CMD",
                ])
                .required(true),
        )
}

/// What sccion wraps when it was started as `name` rather than as itself: the
/// executable [`WRAPS_FILE`] in `dir` names for it, or else the `name` on
/// `path` that is not this executable.
///
/// Wrapping `name` as it stands would make sccion its own compiler. The
/// compilation, and the probe that detects the compiler before it, would start
/// sccion again under the same name, which does the same without end --
/// measured at about 100 new processes a second, with orphans left running
/// after the tree was killed. Every way of getting there is refused, with an
/// error that says what to fix: the wraps file names this copy or nothing that
/// exists, the PATH has no other `name`, or what was found is another copy of
/// sccion (a second hard link, say, which comparing canonical paths cannot
/// tell from the real tool) that leads back. `outer` is how the last shows
/// itself: that copy finds its own name already among the ones sccions above
/// it stand in for.
fn resolve_masquerade(
    name: &OsStr,
    dir: &Path,
    path: &OsStr,
    cwd: &Path,
    own: &Path,
    outer: Option<&OsStr>,
) -> Result<PathBuf> {
    let shown = name.to_string_lossy();
    let key = masquerade_key(name);
    if outer.is_some_and(|o| env::split_paths(o).any(|n| masquerade_key(n.as_os_str()) == key)) {
        bail!(
            "started as `{shown}` ({}) by another sccion already standing in for `{shown}`, so \
             the `{shown}` that one resolved to is a copy of sccion too, and running it would \
             start sccion again without end. What a copy of sccion named `{shown}` stands in for \
             -- the entry in the {WRAPS_FILE} beside it, or else the next `{shown}` on the PATH \
             -- must be the real tool; or invoke `sccion <path to the real {shown}> ...` instead.",
            own.display()
        );
    }

    let is_own =
        |p: &Path| matches!((p.canonicalize(), own.canonicalize()), (Ok(a), Ok(b)) if a == b);
    if let Some((file, named)) = wrapped_by_file(dir, name)? {
        if !named.is_file() {
            bail!(
                "started as `{shown}` ({}), so it stands in for the `{shown}` that {} names, \
                 but there is no file at {}.",
                own.display(),
                file.display(),
                named.display()
            );
        }
        if is_own(&named) {
            bail!(
                "started as `{shown}` ({}), and {} names this very file as the `{shown}` to \
                 stand in for; running it would start sccion again without end. Name the real \
                 `{shown}` there.",
                own.display(),
                file.display()
            );
        }
        return Ok(named);
    }

    // Every `name` on the PATH in order, skipping each that is this file
    // however its directory is spelled there -- `.`, a symlink, a second entry.
    // Leaving out the directory this one was found in would compare spellings,
    // and one it missed would lead back here.
    let found = which_in_all(name, Some(path), cwd)
        .ok()
        .and_then(|mut all| all.find(|p| !is_own(p)));
    found.ok_or_else(|| {
        anyhow!(
            "started as `{shown}` ({}), a name other than its own, so it stands in for \
             the `{shown}` on the PATH -- but the PATH has no `{shown}` except this one, and \
             running it would start sccion again without end. Name the real `{shown}` in a \
             {WRAPS_FILE} beside this copy, put its directory on the PATH, or invoke \
             `sccion <path to the real {shown}> ...` instead.",
            own.display()
        )
    })
}

/// The entry for `name` in the [`WRAPS_FILE`] in `dir`, with the file it came
/// from, or `None` when there is no such file or it has no entry for `name`.
fn wrapped_by_file(dir: &Path, name: &OsStr) -> Result<Option<(PathBuf, PathBuf)>> {
    let file = dir.join(WRAPS_FILE);
    let text = match fs::read_to_string(&file) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("could not read {}", file.display())),
    };
    let wraps: WrapsFile = toml::from_str(&text).with_context(|| {
        format!(
            "{} is not a wraps file: it needs a [wraps] table of tool = \"path\"",
            file.display()
        )
    })?;
    let key = masquerade_key(name);
    let named = wraps
        .wraps
        .into_iter()
        .find(|(tool, _)| masquerade_key(OsStr::new(tool)) == key)
        .map(|(_, path)| dir.join(path));
    Ok(named.map(|named| (file, named)))
}

/// How a tool is named in [`MASQUERADING`]: its file stem, lowercased, so that
/// `moc`, `moc.exe` and `MOC.EXE` are one tool -- the same leniency the check
/// for sccion's own name applies.
fn masquerade_key(name: &OsStr) -> String {
    Path::new(name)
        .file_stem()
        .unwrap_or(name)
        .to_string_lossy()
        .to_lowercase()
}

/// [`MASQUERADING`] for what this sccion runs: the names above it, and `name`.
fn masquerading_with(outer: Option<&OsStr>, name: &OsStr) -> OsString {
    let key = OsString::from(masquerade_key(name));
    let names: Vec<OsString> = outer
        .map(|o| env::split_paths(o).map(PathBuf::into_os_string).collect())
        .unwrap_or_default();
    // A name holding the list separator cannot be listed; leaving it out
    // loses only the guard for that one name.
    env::join_paths(names.iter().cloned().chain([key]))
        .or_else(|_| env::join_paths(names))
        .unwrap_or_default()
}

/// Parse the commandline args into a `Result<Command>` to execute.
pub fn try_parse() -> Result<Command> {
    trace!("parse");

    let cwd = env::current_dir().context("sccion: Couldn't determine current working directory")?;

    // We only care if it's `1`
    let internal_start_server =
        crate::envvar::var(ENV_VAR_INTERNAL_START_SERVER).as_deref() == Some("1");
    let mut args: Vec<_> = env::args_os().collect();

    if !internal_start_server {
        // current_exe() follows symlinks on some OSes, so try argv[0] to get the original invocation
        if let Ok(exe) = match env::args().next() {
            Some(s) if !s.is_empty() => Ok(PathBuf::from(s)),
            _ => env::current_exe(),
        } {
            match exe
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_lowercase())
            {
                // If the executable has its standard name, do nothing.
                //
                // `sccache` counts as standard too. A compatibility symlink is
                // the obvious way to keep an existing
                // `CMAKE_CXX_COMPILER_LAUNCHER=sccache` working across the
                // rename, and without this the name would be taken for a
                // compiler to wrap -- sending us to look for another `sccache`
                // on the PATH, and failing in a way that names no cause.
                Some(ref e) if e == env!("CARGO_PKG_NAME") || e == "sccache" => {}
                // Otherwise, if it was copied/linked under a different $name, act
                // as if it were invoked with `sccion $name`, with $name resolved
                // to the real tool rather than to ourselves.
                _ => {
                    if let Some(exe_filename) = exe.file_name() {
                        let own = env::current_exe()?;
                        // The wraps file sits beside the copy as it was started.
                        // A bare name was found on the PATH, which is where
                        // `own` is.
                        let dir = exe
                            .parent()
                            .filter(|d| !d.as_os_str().is_empty())
                            .or(own.parent())
                            .unwrap_or(cwd.as_path())
                            .to_path_buf();
                        let outer = env::var_os(MASQUERADING);
                        let wrapped = resolve_masquerade(
                            exe_filename,
                            &dir,
                            &env::var_os("PATH").unwrap_or_default(),
                            &cwd,
                            &own,
                            outer.as_deref(),
                        )?;
                        // SAFETY: nothing else runs yet. Parsing comes before the
                        // runtime, the server or any other thread is started, so
                        // no other thread can be reading the environment.
                        unsafe {
                            env::set_var(
                                MASQUERADING,
                                masquerading_with(outer.as_deref(), exe_filename),
                            );
                        }
                        args[0] = wrapped.into();
                        args.insert(0, env!("CARGO_PKG_NAME").into());
                    }
                }
            }
        }
    }

    let matches_result = get_clap_command().try_get_matches_from(args);

    // A command can either be from `ENV_VAR_INTERNAL_START_SERVER` being set or from command-line
    // args. Validate things so that error messages are nice and the returned opts are correct
    match (internal_start_server, matches_result) {
        (true, Err(e)) => {
            // Need to make sure that the error from `clap` is due to a missing command and not
            // some other issue
            if e.kind() == ErrorKind::MissingRequiredArgument {
                Ok(Command::InternalStartServer)
            } else {
                Err(e.into())
            }
        }
        (false, Err(e)) => Err(e.into()),
        (true, Ok(_)) => {
            // `ENV_VAR_INTERNAL_START_SERVER` and a match means that more than one command was
            // provided
            bail!("`{ENV_VAR_INTERNAL_START_SERVER}=1` can't be used with other commands");
        }
        (false, Ok(matches)) => {
            if matches.get_flag("show-stats") {
                let fmt = matches
                    .get_one("stats-format")
                    .cloned()
                    .expect("There is a default value");
                Ok(Command::ShowStats(fmt, false))
            } else if matches.get_flag("show-adv-stats") {
                let fmt = matches
                    .get_one("stats-format")
                    .cloned()
                    .expect("There is a default value");
                Ok(Command::ShowStats(fmt, true))
            } else if matches.get_flag("start-server") {
                Ok(Command::StartServer)
            } else if matches.get_flag("debug-preprocessor-cache") {
                Ok(Command::DebugPreprocessorCacheEntries)
            } else if matches.get_flag("stop-server") {
                Ok(Command::StopServer)
            } else if matches.get_flag("check-env") {
                Ok(Command::CheckEnv(
                    matches
                        .get_one("stats-format")
                        .cloned()
                        .unwrap_or(StatsFormat::Text),
                ))
            } else if matches.get_flag("init") {
                Ok(Command::Init)
            } else if matches.get_flag("cmake-module-dir") {
                Ok(Command::CMakeModuleDir)
            } else if matches.get_flag("zero-stats") {
                Ok(Command::ZeroStats)
            } else if matches.get_flag("dist-auth") {
                Ok(Command::DistAuth)
            } else if matches.get_flag("dist-status") {
                Ok(Command::DistStatus)
            } else if matches.contains_id("package-toolchain") {
                let mut toolchain_values = matches
                    .get_many("package-toolchain")
                    .expect("`package-toolchain` requires two values")
                    .cloned()
                    .collect::<Vec<PathBuf>>();
                let maybe_out = toolchain_values.pop();
                let maybe_exe = toolchain_values.pop();
                match (maybe_exe, maybe_out) {
                    (Some(exe), Some(out)) => Ok(Command::PackageToolchain(exe, out)),
                    _ => unreachable!("clap should enforce two values"),
                }
            } else if matches.contains_id("CMD") {
                let mut env_vars = env::vars_os().collect::<Vec<_>>();

                // If we're running under rr, avoid the `LD_PRELOAD` bits, as it will
                // almost surely do the wrong thing, as the compiler gets executed
                // in a different process tree.
                env_vars.retain(|(k, _v)| {
                    k != "LD_PRELOAD"
                        && k != "RUNNING_UNDER_RR"
                        && k != "HOSTNAME"
                        && k != "PWD"
                        && k != "HOST"
                        && k != "RPM_BUILD_ROOT"
                        && k != "SOURCE_DATE_EPOCH"
                        && k != "RPM_PACKAGE_RELEASE"
                        && k != "MINICOM"
                        && k != "DESTDIR"
                        && k != "RPM_PACKAGE_VERSION"
                        && k != "CARGO_MAKEFLAGS"
                });

                let cmd = matches
                    .get_many("CMD")
                    .expect("CMD is required")
                    .cloned()
                    .collect::<Vec<OsString>>();
                match cmd.as_slice() {
                    [exe, cmdline @ ..] => Ok(Command::Compile {
                        exe: exe.to_owned(),
                        cmdline: cmdline.to_owned(),
                        cwd,
                        env_vars,
                    }),
                    _ => unreachable!("clap should enforce at least one value in cmd"),
                }
            } else {
                unreachable!("Either the arg group or env variable should provide a command");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// The platform's spelling of a tool called `stem`.
    fn exe_name(stem: &str) -> OsString {
        format!("{stem}{}", env::consts::EXE_SUFFIX).into()
    }

    /// An empty file in `dir` that the PATH search accepts as tool `stem`.
    fn fake_tool(dir: &Path, stem: &str) -> PathBuf {
        let p = dir.join(exe_name(stem));
        fs::write(&p, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    #[test]
    fn masquerade_wraps_the_real_tool_found_after_itself() {
        let own_dir = tempfile::tempdir().unwrap();
        let real_dir = tempfile::tempdir().unwrap();
        let own = fake_tool(own_dir.path(), "tool");
        let real = fake_tool(real_dir.path(), "tool");
        let path = env::join_paths([own_dir.path(), real_dir.path()]).unwrap();

        let wrapped = resolve_masquerade(
            &exe_name("tool"),
            own_dir.path(),
            &path,
            own_dir.path(),
            &own,
            None,
        )
        .unwrap();
        assert_eq!(
            wrapped.canonicalize().unwrap(),
            real.canonicalize().unwrap()
        );
    }

    /// The PATH can name this copy's directory more than once, spelled
    /// differently each time; none of them is the real tool.
    #[test]
    fn masquerade_skips_every_spelling_of_its_own_directory() {
        let own_dir = tempfile::tempdir().unwrap();
        let real_dir = tempfile::tempdir().unwrap();
        let own = fake_tool(own_dir.path(), "tool");
        let real = fake_tool(real_dir.path(), "tool");
        // The same directory under a name that compares unequal to its own:
        // through a symlink, or in other case on a volume that folds it.
        #[cfg(unix)]
        let links = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        let respelled = {
            let link = links.path().join("own");
            std::os::unix::fs::symlink(own_dir.path(), &link).unwrap();
            link
        };
        #[cfg(windows)]
        let respelled = PathBuf::from(own_dir.path().to_string_lossy().to_uppercase());
        let path = env::join_paths([
            own_dir.path().to_path_buf(),
            respelled,
            real_dir.path().to_path_buf(),
        ])
        .unwrap();

        let wrapped = resolve_masquerade(
            &exe_name("tool"),
            own_dir.path(),
            &path,
            own_dir.path(),
            &own,
            None,
        )
        .unwrap();
        assert_eq!(
            wrapped.canonicalize().unwrap(),
            real.canonicalize().unwrap()
        );
    }

    #[test]
    fn masquerade_with_only_itself_on_the_path_is_refused() {
        let own_dir = tempfile::tempdir().unwrap();
        let own = fake_tool(own_dir.path(), "tool");
        let path = env::join_paths([own_dir.path()]).unwrap();

        let err = resolve_masquerade(
            &exe_name("tool"),
            own_dir.path(),
            &path,
            own_dir.path(),
            &own,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("the PATH has no `tool"), "{err}");
        assert!(err.contains("sccion <path to the real tool"), "{err}");
    }

    #[test]
    fn masquerade_with_no_such_tool_on_the_path_is_refused() {
        let own_dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let own = fake_tool(own_dir.path(), "tool");
        let path = env::join_paths([elsewhere.path()]).unwrap();

        let err = resolve_masquerade(
            &exe_name("tool"),
            own_dir.path(),
            &path,
            own_dir.path(),
            &own,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("the PATH has no `tool"), "{err}");
    }

    #[test]
    fn masquerade_reached_through_another_copy_is_refused() {
        // Two copies of sccion named `tool`: the first resolves to the second,
        // which a path comparison takes for the real tool. The second sees its
        // own name in what the first set, and stops.
        let own_dir = tempfile::tempdir().unwrap();
        let real_dir = tempfile::tempdir().unwrap();
        let own = fake_tool(own_dir.path(), "tool");
        fake_tool(real_dir.path(), "tool");
        let path = env::join_paths([own_dir.path(), real_dir.path()]).unwrap();
        let outer = masquerading_with(None, &exe_name("tool"));

        let err = resolve_masquerade(
            &exe_name("tool"),
            own_dir.path(),
            &path,
            own_dir.path(),
            &own,
            Some(&outer),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("already standing in for `tool"), "{err}");
    }

    #[test]
    fn masquerade_inside_a_different_one_is_allowed() {
        let own_dir = tempfile::tempdir().unwrap();
        let real_dir = tempfile::tempdir().unwrap();
        let own = fake_tool(own_dir.path(), "tool");
        fake_tool(real_dir.path(), "tool");
        let path = env::join_paths([own_dir.path(), real_dir.path()]).unwrap();
        let outer = masquerading_with(None, &exe_name("other"));

        assert!(
            resolve_masquerade(
                &exe_name("tool"),
                own_dir.path(),
                &path,
                own_dir.path(),
                &own,
                Some(&outer)
            )
            .is_ok()
        );
    }

    #[test]
    fn masquerading_lists_each_tool_once_per_level_by_stem() {
        let one = masquerading_with(None, OsStr::new("MOC.EXE"));
        assert_eq!(one, OsString::from("moc"));
        let two = masquerading_with(Some(&one), OsStr::new("cl.exe"));
        let names: Vec<_> = env::split_paths(&two).collect();
        assert_eq!(names, [PathBuf::from("moc"), PathBuf::from("cl")]);
        assert_eq!(
            masquerade_key(OsStr::new("moc")),
            masquerade_key(OsStr::new("Moc.exe"))
        );
    }

    /// A [`WRAPS_FILE`] in `dir` naming `target` for `stem`.
    fn wraps_file(dir: &Path, stem: &str, target: &Path) {
        // A literal string, so a Windows path needs no escaping.
        let target = target.display();
        fs::write(
            dir.join(WRAPS_FILE),
            format!("[wraps]\n{stem} = '{target}'\n"),
        )
        .unwrap();
    }

    #[test]
    fn wraps_file_names_the_tool_whatever_the_path_holds() {
        let own_dir = tempfile::tempdir().unwrap();
        let real_dir = tempfile::tempdir().unwrap();
        let decoy_dir = tempfile::tempdir().unwrap();
        let own = fake_tool(own_dir.path(), "tool");
        let real = fake_tool(real_dir.path(), "tool");
        fake_tool(decoy_dir.path(), "tool");
        wraps_file(own_dir.path(), "tool", &real);
        // The PATH offers a different `tool`, and the file still wins.
        let path = env::join_paths([decoy_dir.path()]).unwrap();

        let wrapped = resolve_masquerade(
            &exe_name("TOOL"),
            own_dir.path(),
            &path,
            own_dir.path(),
            &own,
            None,
        )
        .unwrap();
        assert_eq!(
            wrapped.canonicalize().unwrap(),
            real.canonicalize().unwrap()
        );
    }

    #[test]
    fn wraps_file_path_is_relative_to_the_file() {
        let own_dir = tempfile::tempdir().unwrap();
        fs::create_dir(own_dir.path().join("real")).unwrap();
        let own = fake_tool(own_dir.path(), "tool");
        let real = fake_tool(&own_dir.path().join("real"), "tool");
        let relative = Path::new("real").join(exe_name("tool"));
        wraps_file(own_dir.path(), "tool", &relative);

        let wrapped = resolve_masquerade(
            &exe_name("tool"),
            own_dir.path(),
            OsStr::new(""),
            own_dir.path(),
            &own,
            None,
        )
        .unwrap();
        assert_eq!(
            wrapped.canonicalize().unwrap(),
            real.canonicalize().unwrap()
        );
    }

    #[test]
    fn wraps_file_without_an_entry_falls_back_to_the_path() {
        let own_dir = tempfile::tempdir().unwrap();
        let real_dir = tempfile::tempdir().unwrap();
        let own = fake_tool(own_dir.path(), "tool");
        let real = fake_tool(real_dir.path(), "tool");
        wraps_file(own_dir.path(), "other", &real);
        let path = env::join_paths([real_dir.path()]).unwrap();

        let wrapped = resolve_masquerade(
            &exe_name("tool"),
            own_dir.path(),
            &path,
            own_dir.path(),
            &own,
            None,
        )
        .unwrap();
        assert_eq!(
            wrapped.canonicalize().unwrap(),
            real.canonicalize().unwrap()
        );
    }

    #[test]
    fn wraps_file_naming_this_copy_is_refused() {
        let own_dir = tempfile::tempdir().unwrap();
        let own = fake_tool(own_dir.path(), "tool");
        wraps_file(own_dir.path(), "tool", &own);

        let err = resolve_masquerade(
            &exe_name("tool"),
            own_dir.path(),
            OsStr::new(""),
            own_dir.path(),
            &own,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("names this very file"), "{err}");
    }

    #[test]
    fn wraps_file_naming_nothing_that_exists_is_refused() {
        let own_dir = tempfile::tempdir().unwrap();
        let own = fake_tool(own_dir.path(), "tool");
        wraps_file(
            own_dir.path(),
            "tool",
            &own_dir.path().join("gone").join(exe_name("tool")),
        );

        let err = resolve_masquerade(
            &exe_name("tool"),
            own_dir.path(),
            OsStr::new(""),
            own_dir.path(),
            &own,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("there is no file at"), "{err}");
    }

    #[test]
    fn malformed_wraps_file_is_refused_and_named() {
        let own_dir = tempfile::tempdir().unwrap();
        let own = fake_tool(own_dir.path(), "tool");
        fs::write(own_dir.path().join(WRAPS_FILE), "tool = \"x\"\n").unwrap();

        let err = format!(
            "{:#}",
            resolve_masquerade(
                &exe_name("tool"),
                own_dir.path(),
                OsStr::new(""),
                own_dir.path(),
                &own,
                None
            )
            .unwrap_err()
        );
        assert!(
            err.contains(WRAPS_FILE) && err.contains("[wraps] table"),
            "{err}"
        );
    }
}
