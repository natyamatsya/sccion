// SPDX-FileCopyrightText: Copyright natyamatsya and sccache contributors
// SPDX-License-Identifier: Apache-2.0

//! Qt's meta-object compiler.
//!
//! moc behaves like a compiler -- deterministic output from a header plus flags
//! -- but it does not fit [`CCompilerImpl`](super::c::CCompilerImpl), which is
//! built around producing preprocessed output and hashing that. Three measured
//! facts rule that model out here:
//!
//! * `moc -E` takes 249ms against 211ms for moc itself, so preprocessing to
//!   compute a key costs more than the work it would avoid.
//! * `moc -E` emits no line markers, so the include list cannot be recovered
//!   from it the way sccache does for C compilers.
//! * On the reference build moc is 18.4% of busy CPU while the AUTOMOC driver
//!   around it is 1.2%, so the cost is here and worth reaching.
//!
//! So moc is cached in direct mode only, driven by `--output-dep-file`. There
//! is no preprocessing on either path, which means -- unlike MSVC -- no
//! cold-build penalty for the privilege.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;

use async_trait::async_trait;

use crate::cache::{FileObjectSource, Storage};
use crate::compiler::c::dependency_result_key;
use crate::compiler::preprocessor_cache::{PreprocessorCacheEntry, includes_from_dep_file};
use crate::compiler::{
    AlreadyProduced, CacheControl, Cacheable, ColorMode, Compilation, CompileCommand, Compiler,
    CompilerArguments, CompilerHasher, CompilerKind, HashResult, SingleCompileCommand,
};
use crate::dist;
use crate::mock_command::CommandCreatorSync;
use crate::util::{Digest, run_input_output, strip_basedirs_in_arg};

use crate::errors::*;

/// Bumped when anything about how a moc key is derived changes.
const MOC_CACHE_VERSION: &[u8] = b"moc-1";

/// Qt's meta-object compiler.
#[derive(Clone, Debug)]
pub struct Moc {
    pub executable: PathBuf,
    pub executable_digest: String,
}

/// True when `p` names moc.
///
/// Qt 6.8 ships the binary as `qtmoc`; earlier versions and some distributions
/// call it `moc`. Both are accepted, and the match is on the file stem because
/// the path is whatever the build system recorded at configure time.
pub fn is_moc<P: AsRef<Path>>(p: P) -> bool {
    matches!(
        p.as_ref()
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .as_deref(),
        Some("moc") | Some("qtmoc")
    )
}

/// Flags that take their value as the following argument when given alone.
///
/// moc accepts both `-Ifoo` and `-I foo`. Missing the separated form would make
/// the value look like a second input file and reject the compilation -- the
/// same class of mistake that made every `/`-form MSVC flag uncacheable.
const VALUE_FLAGS: &[&str] = &[
    "-o",
    "-I",
    "-F",
    "-D",
    "-U",
    "-p",
    "-b",
    "-n",
    "-M",
    "--include",
    "--dep-file-path",
    "--dep-file-rule-name",
    "--compiler-flavor",
];

#[derive(Debug, Clone)]
pub struct ParsedMoc {
    /// Everything except `-o` and its value, which is what gets hashed.
    hashed_args: Vec<OsString>,
    /// The full argument list, for actually running moc.
    all_args: Vec<OsString>,
    input: PathBuf,
    output: PathBuf,
    /// `--output-json` makes moc write `<output>.json` as well.
    json_output: Option<PathBuf>,
    /// Where the caller already asked for the dependency report, if it did.
    dep_file: Option<PathBuf>,
}

pub fn parse_arguments(arguments: &[OsString]) -> CompilerArguments<ParsedMoc> {
    let mut hashed_args = Vec::new();
    let mut input: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut wants_json = false;
    let mut wants_dep_file = false;
    let mut dep_file: Option<PathBuf> = None;
    let mut it = arguments.iter().peekable();

    while let Some(arg) = it.next() {
        let s = arg.to_string_lossy();

        if s == "-E" {
            // Preprocess-only is not a compilation and has no cacheable output.
            return CompilerArguments::NotCompilation;
        }
        if s == "--output-json" {
            wants_json = true;
            hashed_args.push(arg.clone());
            continue;
        }
        if s == "--output-dep-file" {
            // The caller wants a dependency file of its own -- CMake's AUTOMOC
            // always does. Use theirs rather than refusing: the flag being
            // present is what makes this compilation cacheable, and adding a
            // second one would only fight over the path.
            wants_dep_file = true;
            continue;
        }

        // Separated value form: the flag alone, value next.
        if let Some(flag) = VALUE_FLAGS.iter().find(|f| s == **f) {
            let Some(val) = it.next() else {
                return CompilerArguments::CannotCache("flag without a value", None);
            };
            match *flag {
                "-o" => output = Some(PathBuf::from(val)),
                // Where the dependency file goes says nothing about what moc
                // produces, so it must not split the key.
                "--dep-file-path" => dep_file = Some(PathBuf::from(val)),
                "--dep-file-rule-name" => {}
                _ => {
                    hashed_args.push(arg.clone());
                    hashed_args.push(val.clone());
                }
            }
            continue;
        }
        // Attached value form.
        if let Some(flag) = VALUE_FLAGS.iter().find(|f| s.starts_with(**f) && s.len() > f.len()) {
            match *flag {
                "-o" => output = Some(PathBuf::from(&s[2..])),
                "--dep-file-path" => {
                    dep_file = Some(PathBuf::from(s.trim_start_matches("--dep-file-path").trim_start_matches('=')))
                }
                "--dep-file-rule-name" => {}
                _ => hashed_args.push(arg.clone()),
            }
            continue;
        }
        if s.starts_with('-') {
            hashed_args.push(arg.clone());
            continue;
        }

        // A bare argument is the input.
        if input.is_some() {
            return CompilerArguments::CannotCache("multiple input files", None);
        }
        input = Some(PathBuf::from(arg));
    }

    let (Some(input), Some(output)) = (input, output) else {
        // Without an explicit -o moc writes to stdout, which sccache has no
        // output file to cache.
        return CompilerArguments::CannotCache("moc without an input and -o", None);
    };

    let json_output = wants_json.then(|| {
        let mut p = output.clone().into_os_string();
        p.push(".json");
        PathBuf::from(p)
    });

    // Without --dep-file-path moc writes beside the output, as `<output>.d`.
    // CMake's AUTOMOC relies on exactly that and passes no path, so refusing
    // here rejected all 669 invocations of the reference build.
    if wants_dep_file && dep_file.is_none() {
        let mut p = output.clone().into_os_string();
        p.push(".d");
        dep_file = Some(PathBuf::from(p));
    }

    CompilerArguments::Ok(ParsedMoc {
        hashed_args,
        all_args: arguments.to_vec(),
        input,
        output,
        json_output,
        dep_file,
    })
}

#[async_trait]
impl<T: CommandCreatorSync> Compiler<T> for Moc {
    fn kind(&self) -> CompilerKind {
        CompilerKind::Moc
    }

    #[cfg(feature = "dist-client")]
    fn get_toolchain_packager(&self) -> Box<dyn crate::dist::pkg::ToolchainPackager> {
        // moc is never distributed; nothing should ask for this.
        unimplemented!("moc does not support distributed compilation")
    }

    fn parse_arguments(
        &self,
        arguments: &[OsString],
        _cwd: &Path,
        _env_vars: &[(OsString, OsString)],
    ) -> CompilerArguments<Box<dyn CompilerHasher<T> + 'static>> {
        match parse_arguments(arguments) {
            CompilerArguments::Ok(parsed) => CompilerArguments::Ok(Box::new(MocHasher {
                executable: self.executable.clone(),
                executable_digest: self.executable_digest.clone(),
                parsed,
            })),
            CompilerArguments::CannotCache(r, d) => CompilerArguments::CannotCache(r, d),
            CompilerArguments::NotCompilation => CompilerArguments::NotCompilation,
        }
    }

    fn box_clone(&self) -> Box<dyn Compiler<T>> {
        Box::new(self.clone())
    }
}

#[derive(Debug, Clone)]
pub struct MocHasher {
    executable: PathBuf,
    executable_digest: String,
    parsed: ParsedMoc,
}

impl MocHasher {
    /// Key for the manifest: identifies *which* compilation this is, before
    /// anything is known about what it reads.
    fn manifest_key(&self, cwd: &Path, basedirs: &[Vec<u8>]) -> String {
        let mut m = Digest::new();
        m.update(MOC_CACHE_VERSION);
        m.update(self.executable_digest.as_bytes());
        for a in &self.parsed.hashed_args {
            let stripped = strip_basedirs_in_arg(a.as_encoded_bytes(), basedirs);
            m.update(&(stripped.len() as u64).to_le_bytes());
            m.update(&stripped);
        }
        let input = cwd.join(&self.parsed.input);
        let stripped = strip_basedirs_in_arg(input.as_os_str().as_encoded_bytes(), basedirs);
        m.update(&stripped);
        m.finish()
    }
}

#[async_trait]
impl<T: CommandCreatorSync> CompilerHasher<T> for MocHasher {
    async fn generate_hash_key(
        &mut self,
        creator: &T,
        cwd: PathBuf,
        env_vars: Vec<(OsString, OsString)>,
        _may_dist: bool,
        pool: &tokio::runtime::Handle,
        _rewrite_includes_only: bool,
        storage: Arc<dyn Storage>,
        cache_control: CacheControl,
    ) -> Result<HashResult<T>> {
        let config = storage.preprocessor_cache_mode_config();
        let basedirs = crate::compiler::c::effective_basedirs_for(storage.basedirs(), &cwd);
        let manifest_key = self.manifest_key(&cwd, &basedirs);

        // 1. Ask the manifest whether everything this compilation read is
        //    unchanged. When it is, the result key falls out with nothing run.
        if config.use_preprocessor_cache_mode && cache_control == CacheControl::Default {
            if let Ok(Some(mut seekable)) = storage.get_preprocessor_cache_entry(&manifest_key).await
            {
                let mut buf = Vec::new();
                if std::io::Read::read_to_end(&mut seekable, &mut buf).is_ok()
                    && let Ok(mut entry) = PreprocessorCacheEntry::read(&buf)
                {
                    let mut updated = false;
                    if let Some(key) = entry.lookup_result_digest(config, &mut updated, pool).await {
                        debug!("moc: manifest hit for {}", self.parsed.input.display());
                        return Ok(HashResult {
                            key,
                            compilation: Box::new(MocCompilation {
                                executable: self.executable.clone(),
                                parsed: self.parsed.clone(),
                                env_vars,
                                cwd,
                                already_produced: false,
                                stdout: Vec::new(),
                                stderr: Vec::new(),
                                cacheable: Cacheable::Yes,
                            }),
                            weak_toolchain_key: self.executable_digest.clone(),
                            preprocessor_cache_hit: Some(true),
                        });
                    }
                }
            }
        }

        // 2. Otherwise moc has to run before its inputs are known. Ask it for
        //    the dependency list while it does the work it was going to do
        //    anyway -- the output it produces is kept, so this is not a
        //    throwaway pass like `cl -E` would be.
        // Reuse the caller's dependency file when it asked for one -- CMake's
        // AUTOMOC always does -- and only add our own when it did not. Adding a
        // second would fight over the path with theirs.
        let dep_dir;
        let (dep_path, args) = match &self.parsed.dep_file {
            Some(p) => (cwd.join(p), self.parsed.all_args.clone()),
            None => {
                dep_dir = tempfile::Builder::new().prefix("sccache-moc").tempdir()?;
                let p = dep_dir.path().join("deps.d");
                let mut args = self.parsed.all_args.clone();
                args.push("--output-dep-file".into());
                args.push("--dep-file-path".into());
                args.push(p.clone().into_os_string());
                (p, args)
            }
        };

        use crate::mock_command::RunCommand;
        let mut cmd = creator.clone().new_command_sync(&self.executable);
        cmd.args(&args)
            .env_clear()
            .envs(env_vars.iter().map(|(k, v)| (k, v)))
            .current_dir(&cwd);
        let output = run_input_output(cmd, None).await?;
        if !output.status.success() {
            return Err(ProcessError(output).into());
        }

        let includes = match std::fs::read_to_string(&dep_path) {
            Ok(contents) => {
                includes_from_dep_file(&contents, &cwd, config.ignore_time_macros, pool).await
            }
            Err(e) => {
                debug!("moc: no dependency file ({e}); not caching");
                None
            }
        };

        let Some(includes) = includes else {
            // Nothing verifiable, so nothing cacheable. The key is made unique
            // so it cannot collide with a real entry, and the compilation is
            // marked uncacheable so nothing is stored under it.
            let mut m = Digest::new();
            m.update(manifest_key.as_bytes());
            m.update(&std::time::SystemTime::now().elapsed().unwrap_or_default().as_nanos().to_le_bytes());
            m.update(&std::process::id().to_le_bytes());
            return Ok(HashResult {
                key: m.finish(),
                compilation: Box::new(MocCompilation {
                    executable: self.executable.clone(),
                    parsed: self.parsed.clone(),
                    env_vars,
                    cwd,
                    already_produced: true,
                    stdout: output.stdout,
                    stderr: output.stderr,
                    cacheable: Cacheable::No,
                }),
                weak_toolchain_key: self.executable_digest.clone(),
                preprocessor_cache_hit: Some(false),
            });
        };

        let key = dependency_result_key(&manifest_key, &includes, &basedirs);

        if config.use_preprocessor_cache_mode {
            let mut entry = PreprocessorCacheEntry::new();
            entry.add_result(std::time::SystemTime::now(), &key, includes);
            if let Err(e) = storage.put_preprocessor_cache_entry(&manifest_key, entry).await {
                debug!("moc: failed to store manifest: {e}");
            }
        }

        Ok(HashResult {
            key,
            compilation: Box::new(MocCompilation {
                executable: self.executable.clone(),
                parsed: self.parsed.clone(),
                env_vars,
                cwd,
                // moc already ran, and its output is on disk where it belongs.
                already_produced: true,
                stdout: output.stdout,
                stderr: output.stderr,
                cacheable: Cacheable::Yes,
            }),
            weak_toolchain_key: self.executable_digest.clone(),
            preprocessor_cache_hit: Some(false),
        })
    }

    fn color_mode(&self) -> ColorMode {
        ColorMode::Off
    }

    fn output_pretty(&self) -> std::borrow::Cow<'_, str> {
        self.parsed
            .output
            .file_name()
            .map(|s| s.to_string_lossy())
            .unwrap_or(std::borrow::Cow::Borrowed("moc"))
    }

    fn box_clone(&self) -> Box<dyn CompilerHasher<T>> {
        Box::new(self.clone())
    }

    fn language(&self) -> crate::compiler::Language {
        crate::compiler::Language::Cxx
    }
}

#[derive(Debug)]
pub struct MocCompilation {
    executable: PathBuf,
    parsed: ParsedMoc,
    env_vars: Vec<(OsString, OsString)>,
    cwd: PathBuf,
    /// Set when moc has already run during hashing and left its output in
    /// place, so re-running would repeat work that is already done.
    already_produced: bool,
    /// moc's diagnostics from that run, replayed instead of lost.
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// `No` when nothing verifiable could be recorded about what moc read, so
    /// the result must not be stored under a key that could later be hit.
    cacheable: Cacheable,
}

impl<T: CommandCreatorSync> Compilation<T> for MocCompilation {
    fn generate_compile_commands(
        &self,
        _path_transformer: &mut dist::PathTransformer,
        _rewrite_includes_only: bool,
    ) -> Result<(
        Box<dyn CompileCommand<T>>,
        Option<dist::CompileCommand>,
        Cacheable,
    )> {
        if self.already_produced {
            return Ok((
                crate::compiler::CCompileCommand::new(AlreadyProduced {
                    executable: self.executable.clone(),
                    arguments: self.parsed.all_args.clone(),
                    env_vars: self.env_vars.clone(),
                    cwd: self.cwd.clone(),
                    output: process::Output {
                        status: crate::mock_command::exit_status(0),
                        stdout: self.stdout.clone(),
                        stderr: self.stderr.clone(),
                    },
                }),
                None,
                self.cacheable,
            ));
        }
        let cmd = SingleCompileCommand {
            executable: self.executable.clone(),
            arguments: self.parsed.all_args.clone(),
            env_vars: self.env_vars.clone(),
            cwd: self.cwd.clone(),
        };
        Ok((
            crate::compiler::CCompileCommand::new(cmd),
            None,
            self.cacheable,
        ))
    }

    #[cfg(feature = "dist-client")]
    fn into_dist_packagers(
        self: Box<Self>,
        _path_transformer: dist::PathTransformer,
    ) -> Result<crate::compiler::DistPackagers> {
        bail!("moc does not support distributed compilation")
    }

    fn outputs<'a>(&'a self) -> Box<dyn Iterator<Item = FileObjectSource> + 'a> {
        let mut v = vec![FileObjectSource {
            key: "moc".to_string(),
            path: self.parsed.output.clone(),
            optional: false,
        }];
        if let Some(dep) = &self.parsed.dep_file {
            // The build system asked for this and depends on it; restoring the
            // generated source without it would leave the dependency graph
            // referring to a file that is not there.
            v.push(FileObjectSource {
                key: "moc_dep".to_string(),
                path: dep.clone(),
                optional: false,
            });
        }
        if let Some(json) = &self.parsed.json_output {
            // --output-json writes a second file beside the source; a hit that
            // restored only the .cpp would leave a stale one next to it.
            v.push(FileObjectSource {
                key: "moc_json".to_string(),
                path: json.clone(),
                optional: false,
            });
        }
        Box::new(v.into_iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn recognises_both_names_qt_ships() {
        assert!(is_moc("D:/qt/bin/qtmoc.exe"));
        assert!(is_moc("/usr/lib/qt6/moc"));
        assert!(!is_moc("cl.exe"));
        assert!(!is_moc("mocha"));
    }

    #[test]
    fn separated_flag_values_are_not_mistaken_for_the_input() {
        // `-I foo` must not make `foo` look like a second input file. Getting
        // this wrong is what made every unknown /-form MSVC flag uncacheable.
        let a = os(&["-I", "inc", "-D", "FOO=1", "-o", "out.cpp", "in.h"]);
        let p = match parse_arguments(&a) {
            CompilerArguments::Ok(p) => p,
            o => panic!("{o:?}"),
        };
        assert_eq!(p.input, PathBuf::from("in.h"));
        assert_eq!(p.output, PathBuf::from("out.cpp"));
        assert!(!p.hashed_args.iter().any(|x| x == "out.cpp"));
    }

    #[test]
    fn attached_flag_values_work_too() {
        let a = os(&["-Iinc", "-DFOO=1", "-oout.cpp", "in.h"]);
        let p = match parse_arguments(&a) {
            CompilerArguments::Ok(p) => p,
            o => panic!("{o:?}"),
        };
        assert_eq!(p.input, PathBuf::from("in.h"));
        assert_eq!(p.output, PathBuf::from("out.cpp"));
    }

    #[test]
    fn output_json_is_a_second_artifact() {
        let a = os(&["--output-json", "-o", "moc_x.cpp", "x.h"]);
        let p = match parse_arguments(&a) {
            CompilerArguments::Ok(p) => p,
            o => panic!("{o:?}"),
        };
        // Restoring only the .cpp would leave a stale .json beside it.
        assert_eq!(p.json_output, Some(PathBuf::from("moc_x.cpp.json")));
    }

    #[test]
    fn the_output_path_is_not_part_of_the_key() {
        // Two builds writing the same generated source to different paths are
        // the same compilation; including -o would split them needlessly and
        // defeat sharing between checkouts.
        let a = parse_arguments(&os(&["-Iinc", "-o", "a/out.cpp", "in.h"]));
        let b = parse_arguments(&os(&["-Iinc", "-o", "b/out.cpp", "in.h"]));
        match (a, b) {
            (CompilerArguments::Ok(a), CompilerArguments::Ok(b)) => {
                assert_eq!(a.hashed_args, b.hashed_args)
            }
            _ => panic!("both should parse"),
        }
    }

    #[test]
    fn preprocess_only_is_not_a_compilation() {
        assert!(matches!(
            parse_arguments(&os(&["-E", "-o", "out.i", "in.h"])),
            CompilerArguments::NotCompilation
        ));
    }

    #[test]
    fn a_caller_supplied_dep_file_is_used_rather_than_duplicated() {
        // CMake's AUTOMOC always asks moc for one. Refusing on sight rejected
        // every one of the 669 invocations in the reference build.
        let a = os(&[
            "--output-dep-file",
            "--dep-file-path",
            "out.cpp.d",
            "-o",
            "out.cpp",
            "in.h",
        ]);
        let p = match parse_arguments(&a) {
            CompilerArguments::Ok(p) => p,
            o => panic!("{o:?}"),
        };
        assert_eq!(p.dep_file, Some(PathBuf::from("out.cpp.d")));
        // Where the report is written says nothing about what moc produces.
        assert!(!p.hashed_args.iter().any(|x| x == "out.cpp.d"));
    }

    #[test]
    fn a_dep_file_with_no_path_defaults_beside_the_output() {
        // Verified against qtmoc: `-o sub/moc_w.cpp --output-dep-file` writes
        // sub/moc_w.cpp.d. CMake's AUTOMOC relies on this and passes no path.
        let p = match parse_arguments(&os(&["--output-dep-file", "-o", "out.cpp", "in.h"])) {
            CompilerArguments::Ok(p) => p,
            o => panic!("{o:?}"),
        };
        assert_eq!(p.dep_file, Some(PathBuf::from("out.cpp.d")));
    }

    #[test]
    fn without_an_output_there_is_nothing_to_cache() {
        assert!(matches!(
            parse_arguments(&os(&["in.h"])),
            CompilerArguments::CannotCache(..)
        ));
    }
}
