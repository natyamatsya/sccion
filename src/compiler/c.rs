// Copyright 2016 Mozilla Foundation
// SPDX-FileCopyrightText: Copyright natyamatsya and sccache contributors
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

use std::time::Instant;

use crate::cache::{FileObjectSource, Storage};
use crate::compiler::preprocessor_cache::preprocessor_cache_entry_hash_key;
use crate::compiler::{
    AlreadyProduced, CCompileCommand, Cacheable, ColorMode, Compilation, CompileCommand, Compiler,
    CompilerArguments, CompilerHasher, CompilerKind, HashResult, Language, NotCacheable,
    SingleCompileCommand,
};
#[cfg(feature = "dist-client")]
use crate::compiler::{DistPackagers, NoopOutputsRewriter};
use crate::config::PreprocessorCacheModeConfig;
use crate::dist;
#[cfg(feature = "dist-client")]
use crate::dist::pkg;
use crate::missrecord::KeyComponents;
use crate::mock_command::CommandCreatorSync;
use crate::util::{
    Digest, HashToDigest, MetadataCtimeExt, TimeMacroFinder, Timestamp, decode_path, encode_path,
    hash_all_memoized, strip_basedirs, strip_basedirs_in_arg, strip_basedirs_in_dir,
};
use async_trait::async_trait;
use fs_err as fs;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::hash::Hash;
use std::io;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::{Arc, LazyLock};

use crate::errors::*;

use super::CacheControl;
use super::preprocessor_cache::PreprocessorCacheEntry;

/// The basedirs in force for a compilation in `cwd`.
///
/// Configured basedirs come first so an explicit setting wins any tie, followed
/// by whatever a project file at or above `cwd` contributes. Returns the
/// configured set untouched when there is no project file, which is every
/// project that has not opted in.
pub fn effective_basedirs_for(configured: &[Vec<u8>], cwd: &Path) -> Vec<Vec<u8>> {
    let discovered = crate::projectconf::discover_basedirs(cwd);
    if discovered.is_empty() {
        return configured.to_vec();
    }
    let mut out = configured.to_vec();
    for d in discovered {
        if !out.contains(&d) {
            out.push(d);
        }
    }
    out
}

/// Whether this compilation creates or consumes a precompiled header.
///
/// A PCH cannot cross checkouts. Restoring one built elsewhere is not rejected
/// by the compiler -- it is accepted, and then the translation unit fails with
/// a wall of "type redefinition" errors, because the compiler can no longer
/// match the force-included header against the one baked into the PCH and
/// processes it a second time on top. Measured on a 1,898 TU build: the
/// restored .pch was byte-identical to the one produced in the other checkout,
/// and every -Yu unit that used it failed to compile.
///
/// So these compilations keep their absolute paths in the key, which confines
/// them to the checkout that produced them. Everything else still shares.
fn involves_pch(parsed_args: &ParsedArguments) -> bool {
    parsed_args.outputs.contains_key("pch")
        || parsed_args.common_args.iter().any(|a| {
            let a = a.to_string_lossy();
            a.starts_with("-Yu") || a.starts_with("/Yu")
        })
}

/// What a preprocessing run produced.
///
/// Carries the compiler's report of what it read, when it can make one. sccache
/// otherwise recovers that list by scraping `#line` records back out of the
/// preprocessed text, which only works because the text was produced -- and
/// producing it is the expensive part. A compiler that will simply say what it
/// opened (`/sourceDependencies`, `--output-dep-file`, `-MD`) makes the scrape,
/// and eventually the extra preprocessing pass, unnecessary.
///
/// `From<process::Output>` exists so compilers with nothing to report stay
/// one-liners.
pub struct PreprocessorOutput {
    pub output: process::Output,
    /// Inputs the compiler reported reading, if it was asked and could answer.
    pub reported_inputs: Option<Vec<PathBuf>>,
}

impl From<process::Output> for PreprocessorOutput {
    fn from(output: process::Output) -> Self {
        Self {
            output,
            reported_inputs: None,
        }
    }
}

/// The key a dependency-keyed result is stored under: the manifest key plus
/// everything the compilation read.
///
/// Shared with moc, which keys the same way for the same reason: it also has to
/// run before its inputs are known.
///
/// Nothing here is hashed that the manifest key does not already cover except
/// the inputs themselves, which is the whole point -- the manifest maps this
/// same list back to this same key, so a later compilation that reads identical
/// files finds the result without the compiler running at all.
///
/// Paths are basedir-stripped for the same reason arguments are: two checkouts
/// of one tree read files at different absolute paths, and a key that pins them
/// is a key neither can share.
pub fn dependency_result_key(
    manifest_key: &str,
    includes: &[(String, PathBuf)],
    basedirs: &[Vec<u8>],
) -> String {
    let mut m = Digest::new();
    m.update(manifest_key.as_bytes());
    for (digest, path) in includes {
        // Framed with an explicit length: the digests and paths are otherwise
        // concatenated into one stream, where `a/b.h` + `c.h` and `a/b.hc` +
        // `.h` are indistinguishable.
        let stripped = strip_basedirs_in_arg(path.as_os_str().as_encoded_bytes(), basedirs);
        m.update(&(stripped.len() as u64).to_le_bytes());
        m.update(&stripped);
        m.update(digest.as_bytes());
    }
    m.finish()
}

/// A key nothing can collide with, for a compilation that must not be cached.
///
/// The compilation still has to come back with *a* key -- the caller looks one
/// up before it is told the result is uncacheable -- and any key derived only
/// from the compilation itself is a key some other run could legitimately hold.
///
/// `pub(crate)` because moc has the same problem and must not solve it a
/// second, subtly different way.
pub(crate) fn uncacheable_key(manifest_key: &str) -> String {
    let mut m = Digest::new();
    m.update(manifest_key.as_bytes());
    m.update(b"uncacheable");
    m.update(
        &std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_le_bytes(),
    );
    m.update(&std::process::id().to_le_bytes());
    m.finish()
}

/// A generic implementation of the `Compiler` trait for C/C++ compilers.
#[derive(Clone)]
pub struct CCompiler<I>
where
    I: CCompilerImpl,
{
    executable: PathBuf,
    executable_digest: String,
    compiler: I,
}

/// A generic implementation of the `CompilerHasher` trait for C/C++ compilers.
#[derive(Debug, Clone)]
pub struct CCompilerHasher<I>
where
    I: CCompilerImpl,
{
    parsed_args: ParsedArguments,
    executable: PathBuf,
    executable_digest: String,
    compiler: I,
}

/// Artifact produced by a C/C++ compiler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactDescriptor {
    /// Path to the artifact.
    pub path: PathBuf,
    /// Whether the artifact is an optional object file.
    pub optional: bool,
}

/// The results of parsing a compiler commandline.
#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct ParsedArguments {
    /// The input source file.
    pub input: PathBuf,
    /// Whether to prepend the input with `--`
    pub double_dash_input: bool,
    /// The type of language used in the input source file.
    pub language: Language,
    /// The flag required to compile for the given language
    pub compilation_flag: OsString,
    /// The file in which to generate dependencies.
    pub depfile: Option<PathBuf>,
    /// Output files and whether it's optional, keyed by a simple name, like "obj".
    pub outputs: HashMap<&'static str, ArtifactDescriptor>,
    /// Commandline arguments for dependency generation.
    pub dependency_args: Vec<OsString>,
    /// Commandline arguments for the preprocessor (not including common_args).
    pub preprocessor_args: Vec<OsString>,
    /// Commandline arguments for the preprocessor or the compiler.
    pub common_args: Vec<OsString>,
    /// Commandline arguments for the compiler that specify the architecture given
    pub arch_args: Vec<OsString>,
    /// Commandline arguments for the preprocessor or the compiler that don't affect the computed hash.
    pub unhashed_args: Vec<OsString>,
    /// Extra unhashed files that need to be sent along with dist compiles.
    pub extra_dist_files: Vec<PathBuf>,
    /// Extra files that need to have their contents hashed.
    pub extra_hash_files: Vec<PathBuf>,
    /// Whether or not the `-showIncludes` argument is passed on MSVC
    pub msvc_show_includes: bool,
    /// Whether the compilation is generating profiling or coverage data.
    pub profile_generate: bool,
    /// The color mode.
    pub color_mode: ColorMode,
    /// arguments are incompatible with rewrite_includes_only
    pub suppress_rewrite_includes_only: bool,
    /// Arguments are incompatible with preprocessor cache mode
    pub too_hard_for_preprocessor_cache_mode: Option<OsString>,
}

impl ParsedArguments {
    pub fn output_pretty(&self) -> Cow<'_, str> {
        self.outputs
            .get("obj")
            .and_then(|o| o.path.file_name())
            .map(|s| s.to_string_lossy())
            .unwrap_or(Cow::Borrowed("Unknown filename"))
    }
}

/// A generic implementation of the `Compilation` trait for C/C++ compilers.
struct CCompilation<I: CCompilerImpl> {
    parsed_args: ParsedArguments,
    is_locally_preprocessed: bool,
    #[cfg(feature = "dist-client")]
    preprocessed_input: Vec<u8>,
    executable: PathBuf,
    compiler: I,
    cwd: PathBuf,
    env_vars: Vec<(OsString, OsString)>,
    /// Set when the compiler already ran during hashing, because its inputs
    /// could not be known until it had. Carries the verdict from that run:
    /// `Cacheable::No` when nothing verifiable could be recorded about what it
    /// read, or when the compilation failed.
    already_produced: Option<(AlreadyProduced, Cacheable)>,
}

/// Supported C compilers.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum CCompilerKind {
    /// GCC
    Gcc,
    /// clang
    Clang,
    /// Diab
    Diab,
    /// Microsoft Visual C++
    Msvc,
    /// NVIDIA CUDA compiler
    Nvcc,
    /// NVIDIA CUDA front-end
    CudaFE,
    /// NVIDIA CUDA optimizer and PTX generator
    Cicc,
    /// NVIDIA CUDA PTX assembler
    Ptxas,
    /// NVIDIA hpc c, c++ compiler
    Nvhpc,
    /// Tasking VX
    TaskingVX,
}

/// An interface to a specific C compiler.
#[async_trait]
pub trait CCompilerImpl: Clone + fmt::Debug + Send + Sync + 'static {
    /// Return the kind of compiler.
    fn kind(&self) -> CCompilerKind;
    /// Return true iff this is g++ or clang++.
    fn plusplus(&self) -> bool;
    /// Return the compiler version reported by the compiler executable.
    fn version(&self) -> Option<String>;
    /// Determine whether `arguments` are supported by this compiler.
    fn parse_arguments(
        &self,
        arguments: &[OsString],
        cwd: &Path,
        env_vars: &[(OsString, OsString)],
    ) -> CompilerArguments<ParsedArguments>;
    /// Run the C preprocessor with the specified set of arguments.
    #[allow(clippy::too_many_arguments)]
    async fn preprocess<T>(
        &self,
        creator: &T,
        executable: &Path,
        parsed_args: &ParsedArguments,
        cwd: &Path,
        env_vars: &[(OsString, OsString)],
        may_dist: bool,
        rewrite_includes_only: bool,
        preprocessor_cache_mode: bool,
    ) -> Result<PreprocessorOutput>
    where
        T: CommandCreatorSync;
    /// The format in which this compiler reports the files it read while
    /// performing the real compilation, if it can be asked to report at all.
    ///
    /// Answering here opts the compiler into dependency-keyed hashing, which
    /// runs [`Self::generate_compile_commands`]' command directly rather than
    /// through a [`CompileCommand`]. An implementation that returns `Some` must
    /// therefore produce a command that is a plain single process invocation.
    ///
    /// `None` (the default) means the compilation's inputs can only be learned
    /// by preprocessing.
    ///
    /// It takes the parsed arguments because the answer is not a property of
    /// the compiler alone: the same compiler will report a *complete* list or a
    /// deliberately partial one depending on which flag it was given, and a
    /// partial list is the one shape that produces a wrong hit rather than a
    /// miss.
    fn dependency_report_format(
        &self,
        _parsed_args: &ParsedArguments,
    ) -> Option<crate::compiler::depfile::DepFileFormat> {
        None
    }
    /// Arguments that make the compiler write that report to `path`.
    ///
    /// `path` is the caller's own dependency file when they asked for one:
    /// nothing else produces it in this mode, and it is the same list.
    fn dependency_report_args(&self, _path: &Path) -> Vec<OsString> {
        Vec::new()
    }
    /// Generate a command that can be used to invoke the C compiler to perform
    /// the compilation.
    fn generate_compile_commands<T>(
        &self,
        path_transformer: &mut dist::PathTransformer,
        executable: &Path,
        parsed_args: &ParsedArguments,
        cwd: &Path,
        env_vars: &[(OsString, OsString)],
        rewrite_includes_only: bool,
    ) -> Result<(
        Box<dyn CompileCommand<T>>,
        Option<dist::CompileCommand>,
        Cacheable,
    )>
    where
        T: CommandCreatorSync;
}

impl<I> CCompiler<I>
where
    I: CCompilerImpl,
{
    pub async fn new(
        compiler: I,
        executable: PathBuf,
        pool: &tokio::runtime::Handle,
    ) -> Result<CCompiler<I>> {
        let digest = Digest::file(executable.clone(), pool).await?;

        Ok(CCompiler {
            executable,
            executable_digest: {
                if let Some(version) = compiler.version() {
                    let mut m = Digest::new();
                    m.update(digest.as_bytes());
                    m.update(version.as_bytes());
                    m.finish()
                } else {
                    digest
                }
            },
            compiler,
        })
    }

    fn extract_rocm_arg(args: &ParsedArguments, flag: &str) -> Option<PathBuf> {
        args.common_args.iter().find_map(|arg| match arg.to_str() {
            Some(sarg) if sarg.starts_with(flag) => {
                Some(PathBuf::from(sarg[arg.len()..].to_string()))
            }
            _ => None,
        })
    }

    fn extract_rocm_env(env_vars: &[(OsString, OsString)], name: &str) -> Option<PathBuf> {
        env_vars.iter().find_map(|(k, v)| match v.to_str() {
            Some(path) if k == name => Some(PathBuf::from(path.to_string())),
            _ => None,
        })
    }

    // See https://clang.llvm.org/docs/HIPSupport.html for details regarding the
    // order in which the environment variables and command-line arguments control the
    // directory to search for bitcode libraries.
    fn search_hip_device_libs(
        args: &ParsedArguments,
        env_vars: &[(OsString, OsString)],
    ) -> Vec<PathBuf> {
        let rocm_path_arg: Option<PathBuf> = Self::extract_rocm_arg(args, "--rocm-path=");
        let hip_device_lib_path_arg: Option<PathBuf> =
            Self::extract_rocm_arg(args, "--hip-device-lib-path=");
        let rocm_path_env: Option<PathBuf> = Self::extract_rocm_env(env_vars, "ROCM_PATH");
        let hip_device_lib_path_env: Option<PathBuf> =
            Self::extract_rocm_env(env_vars, "HIP_DEVICE_LIB_PATH");

        let hip_device_lib_path: PathBuf = hip_device_lib_path_arg
            .or(hip_device_lib_path_env)
            .or(rocm_path_arg.map(|path| path.join("amdgcn").join("bitcode")))
            .or(rocm_path_env.map(|path| path.join("amdgcn").join("bitcode")))
            // This is the default location in official AMD packages and containers.
            .unwrap_or(PathBuf::from("/opt/rocm/amdgcn/bitcode"));

        hip_device_lib_path
            .read_dir()
            .ok()
            .map(|f| {
                let mut device_libs = f
                    .flatten()
                    .filter(|f| f.path().extension().is_some_and(|ext| ext == "bc"))
                    .map(|f| f.path())
                    .collect::<Vec<_>>();
                device_libs.sort_unstable();
                device_libs
            })
            .unwrap_or_default()
    }
}

impl<T: CommandCreatorSync, I: CCompilerImpl> Compiler<T> for CCompiler<I> {
    fn kind(&self) -> CompilerKind {
        CompilerKind::C(self.compiler.kind())
    }
    #[cfg(feature = "dist-client")]
    fn get_toolchain_packager(&self) -> Box<dyn pkg::ToolchainPackager> {
        Box::new(CToolchainPackager {
            executable: self.executable.clone(),
            kind: self.compiler.kind(),
        })
    }
    fn parse_arguments(
        &self,
        arguments: &[OsString],
        cwd: &Path,
        env_vars: &[(OsString, OsString)],
    ) -> CompilerArguments<Box<dyn CompilerHasher<T> + 'static>> {
        match self.compiler.parse_arguments(arguments, cwd, env_vars) {
            CompilerArguments::Ok(mut args) => {
                // Handle SCCACHE_EXTRAFILES
                for (k, v) in env_vars.iter() {
                    if k.as_os_str() == OsStr::new("SCCACHE_EXTRAFILES") {
                        args.extra_hash_files.extend(std::env::split_paths(&v));
                    }
                }

                // Handle cache invalidation for the ROCm device bitcode libraries. Every HIP
                // object links in some LLVM bitcode libraries (.bc files), so in some sense
                // every HIP object compilation has an direct dependency on those bitcode
                // libraries.
                //
                // The bitcode libraries are unlikely to change **except** when a ROCm version
                // changes, so for correctness we should take these bitcode libraries into
                // account by adding them to `extra_hash_files`.
                //
                // In reality, not every available bitcode library is needed, but that is
                // too much to handle on our side so we just hash every bitcode library we find.
                if args.language == Language::Hip {
                    args.extra_hash_files
                        .extend(Self::search_hip_device_libs(&args, env_vars));
                }

                CompilerArguments::Ok(Box::new(CCompilerHasher {
                    parsed_args: args,
                    executable: self.executable.clone(),
                    executable_digest: self.executable_digest.clone(),
                    compiler: self.compiler.clone(),
                }))
            }
            CompilerArguments::CannotCache(why, extra_info) => {
                CompilerArguments::CannotCache(why, extra_info)
            }
            CompilerArguments::NotCompilation => CompilerArguments::NotCompilation,
        }
    }

    fn box_clone(&self) -> Box<dyn Compiler<T>> {
        Box::new((*self).clone())
    }
}

impl<I> CCompilerHasher<I>
where
    I: CCompilerImpl,
{
    /// Perform the compilation, and key on the inputs the compiler reports
    /// having read while doing it.
    ///
    /// Reached only on a manifest miss, where the alternative is a preprocessor
    /// run whose sole purpose is to make the same list recoverable from text.
    /// The compilation was going to happen anyway on all but one path -- a
    /// manifest miss whose result key is nonetheless already in the cache --
    /// and that is the case this trades away.
    ///
    /// Refusing to cache is the failure mode throughout. A dependency list is
    /// used later to decide whether a cached result is still valid, so a list
    /// that is quietly short cannot cause a miss; it causes a *hit* on a stale
    /// result.
    #[allow(clippy::too_many_arguments)]
    async fn compile_keyed_on_dependencies<T>(
        &self,
        creator: &T,
        cwd: &Path,
        env_vars: &[(OsString, OsString)],
        manifest_key: &str,
        basedirs: &[Vec<u8>],
        storage: &Arc<dyn Storage>,
        config: PreprocessorCacheModeConfig,
        pool: &tokio::runtime::Handle,
        start_of_compilation: std::time::SystemTime,
        format: crate::compiler::depfile::DepFileFormat,
    ) -> Result<HashResult<T>>
    where
        T: CommandCreatorSync,
    {
        // Point the report at the dependency file the caller asked for, when
        // they asked for one. Nothing else produces it in this mode -- sccache
        // normally generates it during the preprocessor run that is no longer
        // happening -- and it is the same list either way, so writing it where
        // the build expects to find it costs nothing and asking twice would
        // only fight over the path.
        let scratch;
        let report_path = match &self.parsed_args.depfile {
            Some(depfile) => cwd.join(depfile),
            None => {
                scratch = tempfile::Builder::new()
                    .prefix("sccache-deps")
                    .tempdir()
                    .context("failed to reserve a place for the dependency report")?;
                scratch.path().join("deps")
            }
        };

        let mut parsed_args = self.parsed_args.clone();
        parsed_args
            .dependency_args
            .extend(self.compiler.dependency_report_args(&report_path));

        let (command, _dist_command, cacheable) = self.compiler.generate_compile_commands::<T>(
            &mut dist::PathTransformer::new(),
            &self.executable,
            &parsed_args,
            cwd,
            env_vars,
            false,
        )?;
        // Run the command directly rather than through `CompileCommand`, which
        // wants a service this side of the process does not have. Opting into
        // dependency reporting is a promise that the command is a plain single
        // invocation; see `CCompilerImpl::dependency_report_format`.
        let command = SingleCompileCommand {
            executable: command.get_executable(),
            arguments: command.get_arguments(),
            env_vars: command.get_env_vars(),
            cwd: command.get_cwd(),
        };
        // This *is* the compilation, so this is where its cost has to be
        // measured. `get_cached_or_compile` times the later
        // `dist_or_local_compile`, which on this path has nothing left to do --
        // leaving "Average compiler" reading 0.000 s for every build with
        // dependency keying on, which is the fork's default.
        let compile_started = Instant::now();
        let output = match command.run(creator).await {
            Ok(output) => output,
            // A compilation that fails is not a failure of sccache, and must
            // not be reported as one: it goes back through the normal path so
            // that it is counted as a compile failure and the compiler's own
            // diagnostics reach the user. It is simply never cached.
            Err(e) => match e.downcast::<ProcessError>() {
                Ok(ProcessError(output)) => output,
                Err(e) => return Err(e),
            },
        };
        let compiled_for = compile_started.elapsed();
        let produced = AlreadyProduced {
            executable: command.executable.clone(),
            arguments: command.arguments.clone(),
            env_vars: command.env_vars.clone(),
            cwd: command.cwd.clone(),
            output,
        };

        // A compilation that failed wrote no object worth keying, and whatever
        // report it left behind describes a run that did not finish.
        let includes = if produced.output.status.success() {
            match fs::read_to_string(&report_path) {
                Ok(text) => match format.read(&text) {
                    // The compiler opened nothing besides the source, whose
                    // path and content are already in the manifest key. That
                    // leaves nothing to verify later, which is a complete
                    // answer and not a missing one -- generated resource and
                    // table files are like this by construction, and reading it
                    // as "unverifiable" made every one of them uncacheable.
                    Some(paths) if paths.is_empty() => Ok(Vec::new()),
                    Some(paths) => {
                        crate::compiler::preprocessor_cache::includes_from_paths(
                            paths,
                            cwd,
                            config.ignore_time_macros,
                            pool,
                        )
                        .await
                    }
                    None => {
                        debug!(
                            "[{}]: dependency report at {} could not be read; not caching",
                            self.parsed_args.output_pretty(),
                            report_path.display()
                        );
                        Err(NotCacheable::DependencyReportUnreadable)
                    }
                },
                Err(e) => {
                    debug!(
                        "[{}]: no dependency report at {} ({e}); not caching",
                        self.parsed_args.output_pretty(),
                        report_path.display()
                    );
                    Err(NotCacheable::DependencyReportMissing)
                }
            }
        } else {
            Err(NotCacheable::CompilationFailed)
        };

        let (key, cacheable) = match &includes {
            Ok(includes) => (
                dependency_result_key(manifest_key, includes, basedirs),
                cacheable,
            ),
            Err(why) => (uncacheable_key(manifest_key), Cacheable::No(*why)),
        };

        // Record what was read against the key it produced, so the next
        // compilation of this source with these files unchanged needs neither a
        // preprocessor nor a compiler.
        if let (Ok(includes), Cacheable::Yes) = (includes, cacheable) {
            let mut entry = PreprocessorCacheEntry::new();
            entry.add_result(start_of_compilation, &key, includes, basedirs);
            if let Err(e) = storage
                .put_preprocessor_cache_entry(manifest_key, entry)
                .await
            {
                debug!("Failed to update preprocessor cache: {}", e);
            }
        }

        let weak_toolchain_key = format!(
            "{}-{}",
            self.executable.to_string_lossy(),
            self.executable_digest
        );
        Ok(HashResult {
            key,
            compilation: Box::new(CCompilation {
                parsed_args: self.parsed_args.clone(),
                // The compilation ran locally and wrote its own dependency
                // file, if it was asked for one. A cache hit found after the
                // fact must leave that alone rather than restore another
                // compilation's.
                is_locally_preprocessed: true,
                #[cfg(feature = "dist-client")]
                preprocessed_input: PREPROCESSING_SKIPPED_COMPILE_POISON.to_vec(),
                executable: self.executable.clone(),
                compiler: self.compiler.clone(),
                cwd: cwd.to_owned(),
                env_vars: env_vars.to_owned(),
                already_produced: Some((produced, cacheable)),
            }),
            weak_toolchain_key,
            preprocessor_cache_hit: Some(false),
            key_components: None,
            // The one path that has already compiled by this point.
            compiled_for: Some(compiled_for),
        })
    }
}

#[async_trait]
impl<T, I> CompilerHasher<T> for CCompilerHasher<I>
where
    T: CommandCreatorSync,
    I: CCompilerImpl,
{
    async fn generate_hash_key(
        &mut self,
        creator: &T,
        cwd: PathBuf,
        env_vars: Vec<(OsString, OsString)>,
        may_dist: bool,
        pool: &tokio::runtime::Handle,
        rewrite_includes_only: bool,
        storage: Arc<dyn Storage>,
        cache_control: CacheControl,
    ) -> Result<HashResult<T>> {
        let start_of_compilation = std::time::SystemTime::now();

        // What the project being built asks for, from a `.sccache.toml` found
        // at or above this compilation. Discovery is keyed on the compilation's
        // own directory rather than on the daemon's, because one daemon serves
        // every checkout on the machine.
        let project = crate::projectconf::discover(&cwd);

        // Configured basedirs, plus any the project contributes.
        let basedirs = if involves_pch(&self.parsed_args) {
            // Keep the absolute paths, so this key cannot be reused by another
            // checkout. See `involves_pch`.
            Vec::new()
        } else {
            effective_basedirs_for(storage.basedirs(), &cwd)
        };

        let mut extra_hashes =
            hash_all_memoized(&self.parsed_args.extra_hash_files, &pool.clone()).await?;
        // Files the project says every compilation in the tree depends on --
        // a toolchain manifest, a generated config header. The same idea as
        // `SCCACHE_EXTRAFILES`, named by the repository that knows about them
        // rather than by whoever remembered to export the variable. Hashed
        // here rather than added to `extra_hash_files`, because they belong in
        // the key and not in the input package a distributed compile ships.
        if !project.extra_hash_files.is_empty() {
            extra_hashes.extend(hash_all_memoized(&project.extra_hash_files, &pool.clone()).await?);
        }
        // A string the project can change to invalidate its own entries, the
        // committed counterpart of `SCCACHE_C_CUSTOM_CACHE_BUSTER`. It goes in
        // with the extra hashes because those already reach both keys, and it
        // has to reach both: busting only the result key would leave the
        // manifest handing out the old one.
        if let Some(buster) = &project.custom_cache_buster {
            extra_hashes.push(format!("custom_cache_buster={buster}"));
        }
        let extra_hashes = extra_hashes;
        // Create an argument vector containing both preprocessor and arch args, to
        // use in creating a hash key
        let mut preprocessor_and_arch_args = self.parsed_args.preprocessor_args.clone();
        preprocessor_and_arch_args.extend(self.parsed_args.arch_args.clone());
        // common_args is used in preprocessing too
        preprocessor_and_arch_args.extend(self.parsed_args.common_args.clone());

        let absolute_input_path: Cow<'_, _> = if self.parsed_args.input.is_absolute() {
            Cow::Borrowed(&self.parsed_args.input)
        } else {
            Cow::Owned(cwd.join(&self.parsed_args.input))
        };

        // Try to look for a cached preprocessing step for this compilation
        // request.
        let mut preprocessor_cache_mode_config = storage.preprocessor_cache_mode_config();
        // Whether a tree expands `__DATE__` is a fact about the tree, so a
        // committed project file outranks the machine here. The scan reads
        // bytes and cannot tell a macro that expands from one merely named, and
        // toolchain headers name them -- libclang's `Preprocessor.h` alone is
        // enough to make every unit that includes it uncacheable in direct
        // mode.
        if let Some(ignore) = project.ignore_time_macros {
            preprocessor_cache_mode_config.ignore_time_macros = ignore;
        }
        let too_hard_for_preprocessor_cache_mode = self
            .parsed_args
            .too_hard_for_preprocessor_cache_mode
            .is_some();
        if let Some(arg) = &self.parsed_args.too_hard_for_preprocessor_cache_mode {
            debug!(
                "generate_hash_key: Cannot use preprocessor cache because of {:?}",
                arg
            );
        }

        let needs_preprocessing = self.parsed_args.language.needs_c_preprocessing();

        let use_preprocessor_cache_mode = if needs_preprocessing {
            let can_use_preprocessor_cache_mode = preprocessor_cache_mode_config
                .use_preprocessor_cache_mode
                && !too_hard_for_preprocessor_cache_mode;

            let mut use_preprocessor_cache_mode = can_use_preprocessor_cache_mode;

            // Allow overrides from the env, and from the project being built.
            // Both can only decline: whether the mode is available at all
            // depends on the cache backend, which is the machine's business and
            // not something a checkout gets to assert.
            let declined = env_vars
                .iter()
                .find_map(|(key, val)| {
                    (key == "SCCACHE_DIRECT")
                        .then(|| val.to_str())
                        .flatten()
                        .map(|val| matches!(val.to_lowercase().as_str(), "false" | "off" | "0"))
                })
                .or_else(|| project.use_preprocessor_cache_mode.map(|on| !on));
            if declined == Some(true) {
                use_preprocessor_cache_mode = false;
            }

            if can_use_preprocessor_cache_mode && !use_preprocessor_cache_mode {
                debug!("generate_hash_key: Preprocessor cache mode declined");
            }

            use_preprocessor_cache_mode
        } else {
            debug!(
                "generate_hash_key: Disabling preprocessor cache because {} language doesn't need C preprocessing",
                self.parsed_args.language.as_str()
            );
            false
        };

        // Dependency keying replaces only the way a compilation's inputs are
        // *learned*, so it is meaningless without the manifest that consumes
        // them, and it has nothing to hand a distributed compile, which needs
        // preprocessed source.
        //
        // Asked in order of how specific the answer is: this invocation, then
        // the project being built, then the machine. Whether it pays off
        // depends on how often a compilation misses its manifest while its
        // object is still in the cache, which is a property of the project --
        // so a committed `.sccache.toml` outranks a machine-wide setting, and
        // `SCCACHE_DEPEND` outranks both for the run in front of you.
        let mut use_dependency_keying = use_preprocessor_cache_mode && !may_dist;
        if use_dependency_keying {
            let from_env = env_vars.iter().find_map(|(key, val)| {
                (key == "SCCACHE_DEPEND")
                    .then(|| val.to_str())
                    .flatten()
                    .map(|val| !matches!(val.to_lowercase().as_str(), "false" | "off" | "0"))
            });
            use_dependency_keying = from_env
                .or(project.use_dependency_keying)
                .unwrap_or(preprocessor_cache_mode_config.use_dependency_keying);
        }

        let mut preprocessor_key = if use_preprocessor_cache_mode {
            preprocessor_cache_entry_hash_key(
                &self.executable_digest,
                self.parsed_args.language,
                &preprocessor_and_arch_args,
                &extra_hashes,
                &env_vars,
                &absolute_input_path,
                self.compiler.plusplus(),
                preprocessor_cache_mode_config,
                &basedirs,
                &cwd,
            )?
        } else {
            None
        };

        // Whether direct mode was actually in play for this compilation, recorded before
        // the lookup so that "no entry stored" counts as a miss rather than as "mode not
        // in play". `preprocessor_key` is reset to `None` further down if the preprocessed
        // output turns out to be unusable, which would otherwise erase the distinction.
        let preprocessor_cache_hit =
            if preprocessor_key.is_some() && cache_control == CacheControl::Default {
                Some(false)
            } else {
                None
            };

        let (preprocessor_output, include_files) = if needs_preprocessing {
            if let Some(preprocessor_key) = &preprocessor_key
                && cache_control == CacheControl::Default
                && let Some(mut seekable) = storage
                    .get_preprocessor_cache_entry(preprocessor_key)
                    .await?
            {
                let mut buf = vec![];
                seekable.read_to_end(&mut buf)?;
                // An entry that cannot be read is an entry that is not there.
                // It was a hard error, which turns a format change -- or one
                // truncated file -- into a build that fails outright rather
                // than one that misses and moves on. An empty entry has no
                // results, so it misses and nothing is written back.
                let mut preprocessor_cache_entry = match PreprocessorCacheEntry::read(&buf) {
                    Ok(entry) => entry,
                    Err(e) => {
                        debug!(
                            "Ignoring unreadable preprocessor cache entry {preprocessor_key}: {e:?}"
                        );
                        PreprocessorCacheEntry::new()
                    }
                };
                let mut updated = false;
                let hit = preprocessor_cache_entry
                    .lookup_result_digest(
                        preprocessor_cache_mode_config,
                        &mut updated,
                        pool,
                        &basedirs,
                    )
                    .await;

                let mut update_failed = false;
                if updated {
                    // Time macros have been found, we need to update
                    // the preprocessor cache entry. See [`PreprocessorCacheEntry::result_matches`].
                    debug!("Preprocessor cache updated because of time macros: {preprocessor_key}");

                    if let Err(e) = storage
                        .put_preprocessor_cache_entry(preprocessor_key, preprocessor_cache_entry)
                        .await
                    {
                        debug!("Failed to update preprocessor cache: {}", e);
                        update_failed = true;
                    }
                }

                if !update_failed {
                    if let Some(key) = hit {
                        debug!("Preprocessor cache hit: {preprocessor_key}");
                        // A compiler binary may be a symlink to another and
                        // so has the same digest, but that means
                        // the toolchain will not contain the correct path
                        // to invoke the compiler! Add the compiler
                        // executable path to try and prevent this
                        let weak_toolchain_key = format!(
                            "{}-{}",
                            self.executable.to_string_lossy(),
                            self.executable_digest
                        );
                        return Ok(HashResult {
                            key,
                            compilation: Box::new(CCompilation {
                                parsed_args: self.parsed_args.clone(),
                                is_locally_preprocessed: false,
                                #[cfg(feature = "dist-client")]
                                preprocessed_input: PREPROCESSING_SKIPPED_COMPILE_POISON.to_vec(),
                                executable: self.executable.clone(),
                                compiler: self.compiler.to_owned(),
                                cwd: cwd.clone(),
                                env_vars: env_vars.clone(),
                                already_produced: None,
                            }),
                            weak_toolchain_key,
                            preprocessor_cache_hit: Some(true),
                            key_components: None,
                            compiled_for: None,
                        });
                    } else {
                        debug!("Preprocessor cache miss: {preprocessor_key}");
                    }
                }
            }

            // A manifest miss leaves the compilation's inputs unknown, and
            // sccache's answer has always been to preprocess and recover them
            // from the text. That is an entire extra compiler run -- 285ms of
            // the 681ms a direct-mode miss costs on the reference build -- to
            // learn something the compiler will state outright while doing the
            // work it was going to do anyway.
            //
            // So when it will, perform the compilation here instead and key on
            // what it reports. The result key is the manifest key plus that
            // list, which is what the manifest maps back to, and the object
            // stays where the compiler put it.
            if let Some(manifest_key) = preprocessor_key.clone()
                && use_dependency_keying
                && let Some(format) = self.compiler.dependency_report_format(&self.parsed_args)
            {
                return self
                    .compile_keyed_on_dependencies(
                        creator,
                        &cwd,
                        &env_vars,
                        &manifest_key,
                        &basedirs,
                        &storage,
                        preprocessor_cache_mode_config,
                        pool,
                        start_of_compilation,
                        format,
                    )
                    .await;
            }

            let result = self
                .compiler
                .preprocess(
                    creator,
                    &self.executable,
                    &self.parsed_args,
                    &cwd,
                    &env_vars,
                    may_dist,
                    rewrite_includes_only,
                    use_preprocessor_cache_mode,
                )
                .await;
            let out_pretty = self.parsed_args.output_pretty().into_owned();
            let result = result.map_err(|e| {
                debug!("[{}]: preprocessor failed: {:?}", out_pretty, e);
                e
            });

            let outputs = self.parsed_args.outputs.clone();
            let args_cwd = cwd.clone();

            let mut preprocessor_result = result.or_else(move |err| {
                // Errors remove all traces of potential output.
                debug!("removing files {:?}", outputs);

                let v: std::result::Result<(), std::io::Error> =
                    outputs.values().try_for_each(|output| {
                        let mut path = args_cwd.clone();
                        path.push(&output.path);
                        match fs::metadata(&path) {
                            // File exists, remove it.
                            Ok(_) => fs::remove_file(&path),
                            _ => Ok(()),
                        }
                    });
                if v.is_err() {
                    warn!("Could not remove files after preprocessing failed!");
                }

                match err.downcast::<ProcessError>() {
                    Ok(ProcessError(output)) => {
                        debug!(
                            "[{}]: preprocessor returned error status {:?}",
                            out_pretty,
                            output.status.code()
                        );
                        // Drop the stdout since it's the preprocessor output,
                        // just hand back stderr and the exit status.
                        bail!(ProcessError(process::Output {
                            stdout: vec!(),
                            ..output
                        }))
                    }
                    Err(err) => Err(err),
                }
            })?;

            // Remember include files needed in this preprocessing step
            let mut include_files = HashMap::new();
            if preprocessor_key.is_some() {
                // Prefer what the compiler said it read over what can be
                // recovered from its output. The scrape only works because the
                // preprocessed text exists, and reading it back is guesswork
                // about a format nobody promised -- the reason `#line`
                // detection was silently broken for every compiler until it was
                // found. A compiler that answers directly is both cheaper and
                // not a parse of prose.
                let reported = preprocessor_result.reported_inputs.take();
                let ok = match reported {
                    Some(paths) if !paths.is_empty() => {
                        match crate::compiler::preprocessor_cache::includes_from_paths(
                            paths,
                            &cwd,
                            preprocessor_cache_mode_config.ignore_time_macros,
                            pool,
                        )
                        .await
                        {
                            Ok(list) => {
                                include_files
                                    .extend(list.into_iter().map(|(digest, path)| (path, digest)));
                                true
                            }
                            // The reported list could not be verified, and a
                            // list that cannot be verified must not be cached
                            // against -- falling back to the scrape here would
                            // paper over exactly the case the report exists to
                            // make explicit.
                            Err(why) => {
                                debug!(
                                    "[{}]: reported inputs unusable ({why})",
                                    self.parsed_args.output_pretty()
                                );
                                false
                            }
                        }
                    }
                    _ => process_preprocessed_file(
                        &absolute_input_path,
                        &cwd,
                        &mut preprocessor_result.output.stdout,
                        &mut include_files,
                        preprocessor_cache_mode_config,
                        start_of_compilation,
                        StandardFsAbstraction,
                    )?,
                };
                if !ok {
                    debug!("Disabling preprocessor cache mode");
                    preprocessor_key = None;
                }
            }

            trace!(
                "[{}]: Preprocessor output is {} bytes",
                self.parsed_args.output_pretty(),
                preprocessor_result.output.stdout.len()
            );

            (preprocessor_result.output.stdout, include_files)
        } else {
            // No preprocessing is supported - input is already preprocessed
            (
                std::fs::read(absolute_input_path.as_path())?,
                HashMap::new(),
            )
        };

        // Create an argument vector containing both common and arch args, to
        // use in creating a hash key
        let mut common_and_arch_args = self.parsed_args.common_args.clone();
        common_and_arch_args.extend(self.parsed_args.arch_args.clone());

        let key_params = HashKeyParams::new(
            &self.executable_digest,
            self.parsed_args.language,
            &common_and_arch_args,
            &preprocessor_output,
        )
        .with_extra_hashes(&extra_hashes)
        .with_env_vars(&env_vars)
        .with_plusplus(self.compiler.plusplus())
        .with_basedirs(&basedirs)
        .with_working_directory(
            preprocessor_cache_mode_config
                .hash_working_directory
                .then_some(cwd.as_path()),
        );
        let key = key_params.compute();
        // A second pass over everything the key was made of, only when this
        // compilation asked for it.
        let key_components =
            crate::missrecord::trace_components(&env_vars).then(|| key_params.components());

        // Cache the preprocessing step
        if let Some(preprocessor_key) = preprocessor_key
            && !include_files.is_empty()
        {
            let mut preprocessor_cache_entry = PreprocessorCacheEntry::new();
            let mut files: Vec<_> = include_files
                .into_iter()
                .map(|(path, digest)| (digest, path))
                .collect();
            files.sort_unstable_by(|a, b| a.1.cmp(&b.1));
            preprocessor_cache_entry.add_result(start_of_compilation, &key, files, &basedirs);

            if let Err(e) = storage
                .put_preprocessor_cache_entry(&preprocessor_key, preprocessor_cache_entry)
                .await
            {
                debug!("Failed to update preprocessor cache: {}", e);
            }
        }

        // A compiler binary may be a symlink to another and so has the same digest, but that means
        // the toolchain will not contain the correct path to invoke the compiler! Add the compiler
        // executable path to try and prevent this
        let weak_toolchain_key = format!(
            "{}-{}",
            self.executable.to_string_lossy(),
            self.executable_digest
        );
        Ok(HashResult {
            key,
            compilation: Box::new(CCompilation {
                parsed_args: self.parsed_args.clone(),
                is_locally_preprocessed: true,
                #[cfg(feature = "dist-client")]
                preprocessed_input: preprocessor_output,
                executable: self.executable.clone(),
                compiler: self.compiler.clone(),
                cwd,
                env_vars,
                already_produced: None,
            }),
            weak_toolchain_key,
            preprocessor_cache_hit,
            key_components,
            compiled_for: None,
        })
    }

    fn color_mode(&self) -> ColorMode {
        self.parsed_args.color_mode
    }

    fn output_pretty(&self) -> Cow<'_, str> {
        self.parsed_args.output_pretty()
    }

    fn input_pretty(&self) -> Option<Cow<'_, str>> {
        Some(self.parsed_args.input.to_string_lossy())
    }

    fn box_clone(&self) -> Box<dyn CompilerHasher<T>> {
        Box::new((*self).clone())
    }

    fn language(&self) -> Language {
        self.parsed_args.language
    }
}

const PRAGMA_GCC_PCH_PREPROCESS: &[u8] = b"pragma GCC pch_preprocess";
const HASH_31_COMMAND_LINE_NEWLINE: &[u8] = b"# 31 \"<command-line>\"\n";
const HASH_32_COMMAND_LINE_2_NEWLINE: &[u8] = b"# 32 \"<command-line>\" 2\n";
const INCBIN_DIRECTIVE: &[u8] = b".incbin";

/// Remember the include files in the preprocessor output if it can be cached.
/// Returns `false` if preprocessor cache mode should be disabled.
fn process_preprocessed_file(
    input_file: &Path,
    cwd: &Path,
    bytes: &mut [u8],
    included_files: &mut HashMap<PathBuf, String>,
    config: PreprocessorCacheModeConfig,
    time_of_compilation: std::time::SystemTime,
    fs_impl: impl PreprocessorFSAbstraction,
) -> Result<bool> {
    let mut start = 0;
    let mut hash_start = 0;
    let total_len = bytes.len();
    let mut normalized_include_paths: HashMap<Vec<u8>, Option<Vec<u8>>> = HashMap::new();
    // There must be at least 7 characters (# 1 "x") left to potentially find an
    // include file path.
    while start < total_len.saturating_sub(7) {
        let mut slice = &bytes[start..];
        // Check if we look at a line containing the file name of an included file.
        // At least the following formats exist (where N is a positive integer):
        //
        // GCC:
        //
        //   # N "file"
        //   # N "file" N
        //   #pragma GCC pch_preprocess "file"
        //
        // HP's compiler:
        //
        //   #line N "file"
        //
        // AIX's compiler:
        //
        //   #line N "file"
        //   #line N
        //
        // Note that there may be other lines starting with '#' left after
        // preprocessing as well, for instance "#    pragma".
        if slice[0] == b'#'
        // GCC:
        && ((slice[1] == b' ' && slice[2] >= b'0' && slice[2] <= b'9')
            // GCC precompiled header:
            || slice[1..].starts_with(PRAGMA_GCC_PCH_PREPROCESS)
            // HP/AIX/MSVC:
            // Note the range must cover the trailing space, or this compares a
            // 4-byte slice against a 5-byte literal and is never equal.
            || (&slice[1..6] == b"line "))
        && (start == 0 || bytes[start - 1] == b'\n')
        {
            match process_preprocessor_line(
                input_file,
                cwd,
                included_files,
                config,
                time_of_compilation,
                bytes,
                start,
                hash_start,
                total_len,
                &mut normalized_include_paths,
                &fs_impl,
            )? {
                ControlFlow::Continue((s, h)) => {
                    start = s;
                    hash_start = h;
                }
                ControlFlow::Break((s, h, continue_preprocessor_cache_mode)) => {
                    if !continue_preprocessor_cache_mode {
                        return Ok(false);
                    }
                    start = s;
                    hash_start = h;
                    continue;
                }
            }
        } else if slice
            .strip_prefix(INCBIN_DIRECTIVE)
            .filter(|slice| {
                slice.starts_with(b"\"") || slice.starts_with(b" \"") || slice.starts_with(b" \\\"")
            })
            .is_some()
        {
            // An assembler .inc bin (without the space) statement, which could be
            // part of inline assembly, refers to an external file. If the file
            // changes, the hash should change as well, but finding out what file to
            // hash is too hard for sccache, so just bail out.
            debug!("Found potential unsupported .inc bin directive in source code");
            return Ok(false);
        } else if slice.starts_with(b"___________") && (start == 0 || bytes[start - 1] == b'\n') {
            // Unfortunately the distcc-pump wrapper outputs standard output lines:
            // __________Using distcc-pump from /usr/bin
            // __________Using # distcc servers in pump mode
            // __________Shutting down distcc-pump include server
            while start < total_len && slice[0] != b'\n' {
                start += 1;
                if start < total_len {
                    slice = &bytes[start..];
                }
            }
            slice = &bytes[start..];
            if slice[0] == b'\n' {
                start += 1;
            }
            hash_start = start;
            continue;
        } else {
            start += 1;
        }
    }
    Ok(true)
}

/// What to do after handling a preprocessor number line.
/// The `Break` variant is `(start, hash_start, continue_preprocessor_cache_mode)`.
/// The `Continue` variant is `(start, hash_start)`.
type PreprocessedLineAction = ControlFlow<(usize, usize, bool), (usize, usize)>;

#[allow(clippy::too_many_arguments)]
fn process_preprocessor_line(
    input_file: &Path,
    cwd: &Path,
    included_files: &mut HashMap<PathBuf, String>,
    config: PreprocessorCacheModeConfig,
    time_of_compilation: std::time::SystemTime,
    bytes: &mut [u8],
    mut start: usize,
    mut hash_start: usize,
    total_len: usize,
    normalized_include_paths: &mut HashMap<Vec<u8>, Option<Vec<u8>>>,
    fs_impl: &impl PreprocessorFSAbstraction,
) -> Result<PreprocessedLineAction> {
    let mut slice = &bytes[start..];
    // Workarounds for preprocessor linemarker bugs in GCC version 6.
    if slice.get(2) == Some(&b'3') {
        if slice.starts_with(HASH_31_COMMAND_LINE_NEWLINE) {
            // Bogus extra line with #31, after the regular #1:
            // Ignore the whole line, and continue parsing.
            while start < hash_start && slice[0] != b'\n' {
                start += 1;
            }
            start += 1;
            hash_start = start;
            return Ok(ControlFlow::Break((start, hash_start, true)));
        } else if slice.starts_with(HASH_32_COMMAND_LINE_2_NEWLINE) {
            // Bogus wrong line with #32, instead of regular #1:
            // Replace the line number with the usual one.
            start += 1;
            bytes[start..=start + 2].copy_from_slice(b"# 1");
            hash_start = start;
            slice = &bytes[start..];
        }
    }
    while start < total_len && slice[0] != b'"' && slice[0] != b'\n' {
        start += 1;
        if start < total_len {
            slice = &bytes[start..];
        }
    }
    slice = &bytes[start..];
    if start < total_len && slice[0] == b'\n' {
        // a newline before the quotation mark -> no match
        return Ok(ControlFlow::Break((start, hash_start, true)));
    }
    start += 1;
    if start >= total_len {
        bail!("Failed to parse included file path");
    }
    // `start` points to the beginning of an include file path
    hash_start = start;
    slice = &bytes[start..];
    while start < total_len && slice[0] != b'"' {
        start += 1;
        if start < total_len {
            slice = &bytes[start..];
        }
    }
    if start == hash_start {
        // Skip empty file name.
        return Ok(ControlFlow::Break((start, hash_start, true)));
    }
    // Look for preprocessor flags, after the "filename".
    let mut system = false;
    let mut pointer = start + 1;
    while pointer < total_len && bytes[pointer] != b'\n' {
        if bytes[pointer] == b'3' {
            // System header.
            system = true;
        }
        pointer += 1;
    }

    // `hash_start` and `start` span the include file path.
    let include_path = &bytes[hash_start..start];
    // We need to normalize the path now since it's part of the
    // hash and since we need to deduplicate the include files.
    // We cache the results since they are often quite a bit repeated.
    let include_path: &[u8] = if let Some(opt) = normalized_include_paths.get(include_path) {
        match opt {
            Some(normalized) => normalized,
            None => include_path,
        }
    } else {
        let path_buf = decode_path(include_path)?;
        let normalized = normalize_path(&path_buf);
        if normalized == path_buf {
            // `None` is a marker that the normalization is the same
            normalized_include_paths.insert(include_path.to_owned(), None);
            include_path
        } else {
            let mut encoded = Vec::with_capacity(include_path.len());
            encode_path(&mut encoded, &normalized)?;
            normalized_include_paths.insert(include_path.to_owned(), Some(encoded));
            // No entry API on hashmaps, so we need to query again
            normalized_include_paths
                .get(include_path)
                .unwrap()
                .as_ref()
                .unwrap()
        }
    };

    if !remember_include_file(
        include_path,
        input_file,
        cwd,
        included_files,
        system,
        config,
        time_of_compilation,
        fs_impl,
    )? {
        return Ok(ControlFlow::Break((start, hash_start, false)));
    }
    // Everything of interest between hash_start and start has been hashed now.
    hash_start = start;
    Ok(ControlFlow::Continue((start, hash_start)))
}

/// Copied from cargo.
///
/// Normalize a path, removing things like `.` and `..`.
///
/// CAUTION: This does not resolve symlinks (unlike
/// [`std::fs::canonicalize`]).
pub fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut components = path.components().peekable();
    let mut ret = if let Some(c @ Component::Prefix(..)) = components.peek().copied() {
        components.next();
        PathBuf::from(c.as_os_str())
    } else {
        PathBuf::new()
    };

    for component in components {
        match component {
            Component::Prefix(..) => unreachable!(),
            Component::RootDir => {
                ret.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                ret.pop();
            }
            Component::Normal(c) => {
                ret.push(c);
            }
        }
    }
    ret
}

/// Limited abstraction of `std::fs::Metadata`, allowing us to create fake
/// values during testing.
#[derive(Debug, Eq, PartialEq, Clone)]
struct PreprocessorFileMetadata {
    is_dir: bool,
    is_file: bool,
    modified: Option<Timestamp>,
    ctime_or_creation: Option<Timestamp>,
}

impl From<std::fs::Metadata> for PreprocessorFileMetadata {
    fn from(meta: std::fs::Metadata) -> Self {
        Self {
            is_dir: meta.is_dir(),
            is_file: meta.is_file(),
            modified: meta.modified().ok().map(Into::into),
            ctime_or_creation: meta.ctime_or_creation().ok(),
        }
    }
}

/// An abstraction to filesystem access for use during the preprocessor
/// caching phase, to make testing easier.
///
/// This may help non-local preprocessor caching in the future, if it ends up
/// being viable.
trait PreprocessorFSAbstraction {
    fn metadata(&self, path: impl AsRef<Path>) -> io::Result<PreprocessorFileMetadata> {
        std::fs::metadata(path).map(Into::into)
    }

    fn open(&self, path: impl AsRef<Path>) -> io::Result<Box<dyn std::io::Read>> {
        Ok(Box::new(std::fs::File::open(path)?))
    }
}

/// Provides filesystem access with the expected standard library functions.
struct StandardFsAbstraction;

impl PreprocessorFSAbstraction for StandardFsAbstraction {}

// Returns false if the include file was "too new" (meaning modified during or
// after the start of the compilation) and therefore should disable
// the preprocessor cache mode, otherwise true.
#[allow(clippy::too_many_arguments)]
fn remember_include_file(
    mut path: &[u8],
    input_file: &Path,
    cwd: &Path,
    included_files: &mut HashMap<PathBuf, String>,
    system: bool,
    config: PreprocessorCacheModeConfig,
    time_of_compilation: std::time::SystemTime,
    fs_impl: &impl PreprocessorFSAbstraction,
) -> Result<bool> {
    // TODO if precompiled header.
    if path.len() >= 2 && path[0] == b'<' && path[path.len() - 1] == b'>' {
        // Typically <built-in> or <command-line>.
        return Ok(true);
    }

    if system && config.skip_system_headers {
        // Don't remember this system header.
        return Ok(true);
    }

    // Canonicalize path for comparison; Clang uses ./header.h.
    #[cfg(windows)]
    {
        if path.starts_with(br".\") || path.starts_with(b"./") {
            path = &path[2..];
        }
    }
    #[cfg(not(windows))]
    {
        if path.starts_with(b"./") {
            path = &path[2..];
        }
    }
    let mut path = decode_path(path).context("failed to decode path")?;
    if path.is_relative() {
        path = cwd.join(path);
    }

    if included_files.contains_key(&path) {
        // Already known include file
        return Ok(true);
    }

    if path == input_file {
        // Don't remember the input file.
        return Ok(true);
    }
    let meta = match fs_impl.metadata(&path) {
        Ok(meta) => meta,
        Err(e) => {
            debug!("Failed to stat include file {}: {}", path.display(), e);
            return Ok(false);
        }
    };
    if meta.is_dir {
        // Ignore directory, typically $PWD.
        return Ok(true);
    }
    if !meta.is_file {
        // Device, pipe, socket or other strange creature.
        debug!("Non-regular include file {}", path.display());
        return Ok(false);
    }

    // TODO add an option to ignore some header files?
    if include_is_too_new(&path, &meta, time_of_compilation) {
        return Ok(false);
    }

    // Let's hash the include file content.
    let file = match fs_impl.open(&path) {
        Ok(file) => file,
        Err(e) => {
            debug!("Failed to open header file {}: {}", path.display(), e);
            return Ok(false);
        }
    };

    let (file_digest, finder) = if config.ignore_time_macros {
        match Digest::reader_sync(file) {
            Ok(file_digest) => (file_digest, TimeMacroFinder::new()),
            Err(e) => {
                debug!("Failed to read header file {}: {}", path.display(), e);
                return Ok(false);
            }
        }
    } else {
        match Digest::reader_sync_time_macros(file) {
            Ok((file_digest, finder)) => (file_digest, finder),
            Err(e) => {
                debug!("Failed to read header file {}: {}", path.display(), e);
                return Ok(false);
            }
        }
    };

    if finder.found_time() {
        debug!("Found __TIME__ in header file {}", path.display());
        return Ok(false);
    }

    included_files.insert(path, file_digest);

    Ok(true)
}

/// Opt out of preprocessor cache mode because of a race condition.
///
/// The race condition consists of these events:
///
/// - the preprocessor is run
/// - an include file is modified by someone
/// - the new include file is hashed by sccache
/// - the real compiler is run on the preprocessor's output, which contains
///   data from the old header file
/// - the wrong object file is stored in the cache.
fn include_is_too_new(
    path: &Path,
    meta: &PreprocessorFileMetadata,
    time_of_compilation: std::time::SystemTime,
) -> bool {
    // The comparison using >= is intentional, due to a possible race between
    // starting compilation and writing the include file.
    if let Some(mtime) = meta.modified
        && mtime >= time_of_compilation.into()
    {
        debug!("Include file {} is too new", path.display());
        return true;
    }

    // The same >= logic as above applies to the change time of the file.
    if let Some(ctime) = meta.ctime_or_creation
        && ctime >= time_of_compilation.into()
    {
        debug!("Include file {} is too new", path.display());
        return true;
    }

    false
}

// Used as "preprocessed code" when no preprocessing was done so that compilation fails. That should never
// happen though because, where necessary, the situation is detected and preprocessing is *then* done to
// salvage the situation. Previously, an empty u8 vector was used, which is unfortunately a valid C and C++
// compilation unit and caused errors that only surfaced when linking: the symbols expected from the
// compilation unit were of course not produced.
#[cfg(feature = "dist-client")]
const PREPROCESSING_SKIPPED_COMPILE_POISON: &[u8] = b"([{SCCACHE -*-* INVALID_C_CPP_CODE([{\"";

impl<T: CommandCreatorSync, I: CCompilerImpl> Compilation<T> for CCompilation<I> {
    fn generate_compile_commands(
        &self,
        path_transformer: &mut dist::PathTransformer,
        rewrite_includes_only: bool,
    ) -> Result<(
        Box<dyn CompileCommand<T>>,
        Option<dist::CompileCommand>,
        Cacheable,
    )> {
        // The compiler ran during hashing because there was no other way to
        // learn what it read. Its output is on disk and its diagnostics are
        // here; running it again would only reproduce both.
        if let Some((produced, cacheable)) = &self.already_produced {
            return Ok((CCompileCommand::new(produced.clone()), None, *cacheable));
        }
        self.compiler.generate_compile_commands(
            path_transformer,
            &self.executable,
            &self.parsed_args,
            &self.cwd,
            &self.env_vars,
            rewrite_includes_only,
        )
    }

    fn produced_during_hashing(&self) -> bool {
        self.already_produced.is_some()
    }

    #[cfg(feature = "dist-client")]
    fn into_dist_packagers(
        self: Box<Self>,
        path_transformer: dist::PathTransformer,
    ) -> Result<DistPackagers> {
        let CCompilation {
            parsed_args,
            cwd,
            preprocessed_input,
            executable,
            compiler,
            ..
        } = *self;
        trace!("Dist inputs: {:?}", parsed_args.input);

        let input_path = cwd.join(&parsed_args.input);
        let inputs_packager = Box::new(CInputsPackager {
            input_path,
            preprocessed_input,
            path_transformer,
            extra_dist_files: parsed_args.extra_dist_files,
            extra_hash_files: parsed_args.extra_hash_files,
        });
        let toolchain_packager = Box::new(CToolchainPackager {
            executable,
            kind: compiler.kind(),
        });
        let outputs_rewriter = Box::new(NoopOutputsRewriter);
        Ok((inputs_packager, toolchain_packager, outputs_rewriter))
    }

    fn is_locally_preprocessed(&self) -> bool {
        self.is_locally_preprocessed
    }

    fn outputs<'a>(&'a self) -> Box<dyn Iterator<Item = FileObjectSource> + 'a> {
        Box::new(
            self.parsed_args
                .outputs
                .iter()
                .map(|(k, output)| FileObjectSource {
                    key: k.to_string(),
                    path: output.path.clone(),
                    optional: output.optional,
                }),
        )
    }
}

#[cfg(feature = "dist-client")]
struct CInputsPackager {
    input_path: PathBuf,
    path_transformer: dist::PathTransformer,
    preprocessed_input: Vec<u8>,
    extra_dist_files: Vec<PathBuf>,
    extra_hash_files: Vec<PathBuf>,
}

#[cfg(feature = "dist-client")]
impl pkg::InputsPackager for CInputsPackager {
    fn write_inputs(self: Box<Self>, wtr: &mut dyn io::Write) -> Result<dist::PathTransformer> {
        let CInputsPackager {
            input_path,
            mut path_transformer,
            preprocessed_input,
            extra_dist_files,
            extra_hash_files,
        } = *self;

        let mut builder = tar::Builder::new(wtr);

        {
            let input_path = pkg::simplify_path(&input_path)?;
            let dist_input_path = path_transformer.as_dist(&input_path).with_context(|| {
                format!("unable to transform input path {}", input_path.display())
            })?;

            let mut file_header = pkg::make_tar_header(&input_path, &dist_input_path)?;
            file_header.set_size(preprocessed_input.len() as u64); // The metadata is from non-preprocessed
            file_header.set_cksum();
            builder.append(&file_header, preprocessed_input.as_slice())?;
        }

        for input_path in extra_hash_files.iter().chain(extra_dist_files.iter()) {
            let input_path = pkg::simplify_path(input_path)?;

            if !super::CAN_DIST_DYLIBS
                && input_path
                    .extension()
                    .is_some_and(|ext| ext == std::env::consts::DLL_EXTENSION)
            {
                bail!(
                    "Cannot distribute dylib input {} on this platform",
                    input_path.display()
                )
            }

            let dist_input_path = path_transformer.as_dist(&input_path).with_context(|| {
                format!("unable to transform input path {}", input_path.display())
            })?;

            let mut file = io::BufReader::new(fs::File::open(&input_path)?);
            let mut output = vec![];
            io::copy(&mut file, &mut output)?;

            let mut file_header = pkg::make_tar_header(&input_path, &dist_input_path)?;
            file_header.set_size(output.len() as u64);
            file_header.set_cksum();
            builder.append(&file_header, &*output)?;
        }

        // Finish archive
        let _ = builder.into_inner();
        Ok(path_transformer)
    }
}

#[cfg(feature = "dist-client")]
#[allow(unused)]
struct CToolchainPackager {
    executable: PathBuf,
    kind: CCompilerKind,
}

#[cfg(feature = "dist-client")]
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
impl pkg::ToolchainPackager for CToolchainPackager {
    fn write_pkg(self: Box<Self>, f: fs::File) -> Result<()> {
        use std::os::unix::ffi::OsStringExt;

        info!("Generating toolchain {}", self.executable.display());
        let mut package_builder = pkg::ToolchainPackageBuilder::new();
        package_builder.add_common()?;
        package_builder.add_executable_and_deps(self.executable.clone())?;

        // Helper to use -print-file-name and -print-prog-name to look up
        // files by path.
        let named_file = |kind: &str, name: &str| -> Option<PathBuf> {
            let mut output = process::Command::new(&self.executable)
                .arg(format!("-print-{}-name={}", kind, name))
                .output()
                .ok()?;
            debug!(
                "find named {} {} output:\n{}\n===\n{}",
                kind,
                name,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            if !output.status.success() {
                debug!("exit failure");
                return None;
            }

            // Remove the trailing newline (if present)
            if output.stdout.last() == Some(&b'\n') {
                output.stdout.pop();
            }

            // Create our PathBuf from the raw bytes.  Assume that relative
            // paths can be found via PATH.
            let path: PathBuf = OsString::from_vec(output.stdout).into();
            if path.is_absolute() {
                Some(path)
            } else {
                which::which(path).ok()
            }
        };

        // Helper to add a named file/program by to the package.
        // We ignore the case where the file doesn't exist, as we don't need it.
        let add_named_prog =
            |builder: &mut pkg::ToolchainPackageBuilder, name: &str| -> Result<()> {
                if let Some(path) = named_file("prog", name) {
                    builder.add_executable_and_deps(path)?;
                }
                Ok(())
            };
        let add_named_file =
            |builder: &mut pkg::ToolchainPackageBuilder, name: &str| -> Result<()> {
                if let Some(path) = named_file("file", name) {
                    builder.add_file(path)?;
                }
                Ok(())
            };

        // Add basic |as| and |objcopy| programs.
        add_named_prog(&mut package_builder, "as")?;
        add_named_prog(&mut package_builder, "objcopy")?;

        // Linker configuration.
        if Path::new("/etc/ld.so.conf").is_file() {
            package_builder.add_file("/etc/ld.so.conf".into())?;
        }
        let ld_conf_dir = Path::new("/etc/ld.so.conf.d");
        if ld_conf_dir.is_dir() {
            package_builder.add_dir_contents(ld_conf_dir)?;
        }

        // Compiler-specific handling
        match self.kind {
            CCompilerKind::Clang => {
                // Clang uses internal header files, so add them.
                if let Some(limits_h) = named_file("file", "include/limits.h") {
                    info!("limits_h = {}", limits_h.display());
                    package_builder.add_dir_contents(limits_h.parent().unwrap())?;
                }
            }

            CCompilerKind::Gcc => {
                // Various external programs / files which may be needed by gcc
                add_named_prog(&mut package_builder, "cc1")?;
                add_named_prog(&mut package_builder, "cc1plus")?;
                add_named_file(&mut package_builder, "specs")?;
                add_named_file(&mut package_builder, "liblto_plugin.so")?;
            }

            CCompilerKind::Cicc
            | CCompilerKind::CudaFE
            | CCompilerKind::Ptxas
            | CCompilerKind::Nvcc => {}

            CCompilerKind::Nvhpc => {
                // Various programs called by the nvc nvc++ front end.
                add_named_file(&mut package_builder, "cpp1")?;
                add_named_file(&mut package_builder, "cpp2")?;
                add_named_file(&mut package_builder, "opt")?;
                add_named_prog(&mut package_builder, "llc")?;
                add_named_prog(&mut package_builder, "acclnk")?;
            }

            _ => unreachable!(),
        }

        // Bundle into a compressed tarfile.
        package_builder.into_compressed_tar(f)
    }
}

/// The cache is versioned by the inputs to `HashKeyParams::compute`.
pub const CACHE_VERSION: &[u8] = b"13";

/// Environment variables that are factored into the cache key.
static CACHED_ENV_VARS: LazyLock<HashSet<&'static OsStr>> = LazyLock::new(|| {
    [
        // SCCACHE_C_CUSTOM_CACHE_BUSTER has no particular meaning behind it,
        // serving as a way for the user to factor custom data into the hash.
        // One can set it to different values for different invocations
        // to prevent cache reuse between them.
        "SCCACHE_C_CUSTOM_CACHE_BUSTER",
        "MACOSX_DEPLOYMENT_TARGET",
        "IPHONEOS_DEPLOYMENT_TARGET",
        "TVOS_DEPLOYMENT_TARGET",
        "WATCHOS_DEPLOYMENT_TARGET",
        "SDKROOT",
        "CCC_OVERRIDE_OPTIONS",
    ]
    .iter()
    .map(OsStr::new)
    .collect()
});

/// Parameters for computing a hash key for C/C++ compilation caching.
///
/// Construct with required fields via [`HashKeyParams::new`], then add optional
/// fields with builder methods.
///
/// # Example
/// ```ignore
/// let hash = HashKeyParams::new(
///     "abc123",          // compiler_digest
///     Language::C,        // language
///     &args,             // arguments
///     output             // preprocessor_output
/// )
/// .with_extra_hashes(&extra)
/// .with_plusplus(true)
/// .compute();
/// ```
#[derive(Debug, Clone)]
pub struct HashKeyParams<'a> {
    compiler_digest: &'a str,
    language: Language,
    arguments: &'a [OsString],
    extra_hashes: &'a [String],
    env_vars: &'a [(OsString, OsString)],
    preprocessor_output: &'a [u8],
    plusplus: bool,
    basedirs: &'a [Vec<u8>],
    working_directory: Option<&'a Path>,
}

impl<'a> HashKeyParams<'a> {
    /// Creates a new hash key parameter set with required fields.
    ///
    /// Optional fields have default values and can be set via `with_*` methods.
    ///
    /// # Arguments
    /// * `compiler_digest` - Hash of the compiler executable
    /// * `language` - Source language being compiled
    /// * `arguments` - Compiler arguments
    /// * `preprocessor_output` - Preprocessed source to hash
    pub fn new(
        compiler_digest: &'a str,
        language: Language,
        arguments: &'a [OsString],
        preprocessor_output: &'a [u8],
    ) -> Self {
        Self {
            compiler_digest,
            language,
            arguments,
            preprocessor_output,
            extra_hashes: &[],
            env_vars: &[],
            plusplus: false,
            basedirs: &[],
            working_directory: None,
        }
    }

    /// Sets additional hash data to include in the cache key.
    pub fn with_extra_hashes(mut self, extra_hashes: &'a [String]) -> Self {
        self.extra_hashes = extra_hashes;
        self
    }

    /// Sets the environment variables to consider for caching.
    pub fn with_env_vars(mut self, env_vars: &'a [(OsString, OsString)]) -> Self {
        self.env_vars = env_vars;
        self
    }

    /// Sets whether this is a C++ compiler (affects clang/clang++ distinction).
    pub fn with_plusplus(mut self, plusplus: bool) -> Self {
        self.plusplus = plusplus;
        self
    }

    /// Sets the base directories for path normalization.
    pub fn with_basedirs(mut self, basedirs: &'a [Vec<u8>]) -> Self {
        self.basedirs = basedirs;
        self
    }

    /// Sets the directory the compilation runs in, when it belongs in the key.
    ///
    /// Nothing else in the key carries it: the arguments and the preprocessed
    /// output mention it only when the build happens to use absolute paths, and
    /// `-EP` emits no paths at all. Two builds of the same relative sources from
    /// different directories otherwise share an entry, and the debug
    /// information in the object names whichever directory produced it first.
    ///
    /// Basedirs still apply, so declaring a checkout root keeps the two
    /// checkouts of one project sharing -- that is what declaring it asserts.
    pub fn with_working_directory(mut self, cwd: Option<&'a Path>) -> Self {
        self.working_directory = cwd;
        self
    }

    /// Computes the hash key based on the configured parameters.
    ///
    /// If `basedirs` are provided, paths in the preprocessor output will be normalized by
    /// stripping the longest matching basedir prefix. This enables cache hits across different
    /// absolute paths (similar to ccache's CCACHE_BASEDIR).
    ///
    /// # Note
    /// If you change any of the inputs to the hash, you should change `CACHE_VERSION`.
    pub fn compute(&self) -> String {
        let mut m = Digest::new();
        m.update(self.compiler_digest.as_bytes());
        // clang and clang++ have different behavior despite being byte-for-byte identical binaries, so
        // we have to incorporate that into the hash as well.
        m.update(&[self.plusplus as u8]);
        m.update(CACHE_VERSION);
        m.update(self.language.as_str().as_bytes());
        for arg in self.arguments {
            if self.basedirs.is_empty() {
                arg.hash(&mut HashToDigest { digest: &mut m });
            } else {
                // Basedirs were only ever applied to the preprocessor output,
                // which left every path-bearing *flag* pinning the key to an
                // absolute path: -I, /FI, -Fp, and the trim/prefix-map flags
                // whose entire purpose is to make the output path-independent.
                // A tree at a second path therefore missed on every entry, no
                // matter how thoroughly the source itself was normalised.
                //
                // Framed with an explicit length, because the arguments are
                // otherwise concatenated into one digest and `-Ifoo` `bar`
                // would hash the same as `-Ifoobar`. `Hash for OsStr` supplies
                // that framing on the other branch; here it has to be written.
                let stripped = strip_basedirs_in_arg(arg.as_encoded_bytes(), self.basedirs);
                m.update(&(stripped.len() as u64).to_le_bytes());
                m.update(&stripped);
            }
        }
        for hash in self.extra_hashes {
            m.update(hash.as_bytes());
        }

        for (var, val) in self.env_vars.iter() {
            if CACHED_ENV_VARS.contains(var.as_os_str()) {
                var.hash(&mut HashToDigest { digest: &mut m });
                m.update(&b"="[..]);
                val.hash(&mut HashToDigest { digest: &mut m });
            }
        }

        if let Some(cwd) = self.working_directory {
            // Basedir-stripped, like everything else that names a path: a
            // declared checkout root means two checkouts of it are the same
            // compilation, and this must not be the one thing that disagrees.
            let stripped = strip_basedirs_in_dir(cwd, self.basedirs);
            m.update(b"cwd=");
            m.update(&(stripped.len() as u64).to_le_bytes());
            m.update(&stripped);
        }

        // Strip basedirs from preprocessor output if configured
        let preprocessor_output_to_hash = strip_basedirs(self.preprocessor_output, self.basedirs);

        m.update(&preprocessor_output_to_hash);
        m.finish()
    }

    /// The same inputs, digested section by section.
    ///
    /// A deliberate second pass rather than a rearrangement of [`compute`]:
    /// deriving the key from these would be tidier and would move every cache
    /// key in existence. What has to hold is only that a component changes when
    /// its section of the input changes, and hashing the same bytes again gives
    /// that.
    ///
    /// Only called when a compilation asks for it -- this walks the preprocessed
    /// text a second time, which is not free.
    ///
    /// [`compute`]: HashKeyParams::compute
    pub fn components(&self) -> KeyComponents {
        let compiler = {
            let mut m = Digest::new();
            m.update(self.compiler_digest.as_bytes());
            m.update(&[self.plusplus as u8]);
            m.update(CACHE_VERSION);
            m.update(self.language.as_str().as_bytes());
            m.finish()
        };
        let arguments = {
            let mut m = Digest::new();
            for arg in self.arguments {
                if self.basedirs.is_empty() {
                    arg.hash(&mut HashToDigest { digest: &mut m });
                } else {
                    let stripped = strip_basedirs_in_arg(arg.as_encoded_bytes(), self.basedirs);
                    m.update(&(stripped.len() as u64).to_le_bytes());
                    m.update(&stripped);
                }
            }
            m.finish()
        };
        let extra_files = {
            let mut m = Digest::new();
            for hash in self.extra_hashes {
                m.update(hash.as_bytes());
            }
            m.finish()
        };
        let env = {
            let mut m = Digest::new();
            for (var, val) in self.env_vars.iter() {
                if CACHED_ENV_VARS.contains(var.as_os_str()) {
                    var.hash(&mut HashToDigest { digest: &mut m });
                    m.update(&b"="[..]);
                    val.hash(&mut HashToDigest { digest: &mut m });
                }
            }
            m.finish()
        };
        let working_directory = {
            let mut m = Digest::new();
            if let Some(cwd) = self.working_directory {
                let stripped = strip_basedirs_in_dir(cwd, self.basedirs);
                m.update(&(stripped.len() as u64).to_le_bytes());
                m.update(&stripped);
            }
            m.finish()
        };
        let inputs = {
            let mut m = Digest::new();
            m.update(&strip_basedirs(self.preprocessor_output, self.basedirs));
            m.finish()
        };
        KeyComponents {
            compiler,
            arguments,
            extra_files,
            env,
            working_directory,
            inputs,
        }
    }
}

#[cfg(test)]
mod test {
    use std::{collections::VecDeque, sync::Mutex};

    use super::*;

    #[test]
    fn test_same_content() {
        let args = ovec!["a", "b", "c"];
        let h1 = HashKeyParams::new("abcd", Language::C, &args, b"hello world").compute();
        let h2 = HashKeyParams::new("abcd", Language::C, &args, b"hello world").compute();
        assert_eq!(h1, h2);
    }

    /// Two worktrees of one repo differ only by their root. With that root as a
    /// basedir they must produce the same key, including through arguments --
    /// which is the whole point of running several agents against one cache.
    #[test]
    fn basedirs_normalize_paths_in_arguments() {
        let a = ovec!["-I/wt/agent-a/include", "-c", "/wt/agent-a/src/x.c"];
        let b = ovec!["-I/wt/agent-b/include", "-c", "/wt/agent-b/src/x.c"];

        // Without basedirs the two are genuinely different compilations.
        let h1 = HashKeyParams::new("abcd", Language::C, &a, b"src").compute();
        let h2 = HashKeyParams::new("abcd", Language::C, &b, b"src").compute();
        assert_neq!(h1, h2);

        // With each tree's root declared, they converge.
        let ba: Vec<Vec<u8>> = vec![b"/wt/agent-a".to_vec()];
        let bb: Vec<Vec<u8>> = vec![b"/wt/agent-b".to_vec()];
        let n1 = HashKeyParams::new("abcd", Language::C, &a, b"src")
            .with_basedirs(&ba)
            .compute();
        let n2 = HashKeyParams::new("abcd", Language::C, &b, b"src")
            .with_basedirs(&bb)
            .compute();
        assert_eq!(n1, n2);
    }

    /// The same tree checked out twice reads the same files at different
    /// absolute paths. With each root declared, the result key must converge --
    /// otherwise every checkout stores its own copy of an identical object and
    /// none of them can find another's.
    #[test]
    fn dependency_keys_converge_across_checkouts() {
        let a = [
            ("digest-h".to_string(), PathBuf::from("/wt/agent-a/inc/x.h")),
            ("digest-y".to_string(), PathBuf::from("/wt/agent-a/inc/y.h")),
        ];
        let b = [
            ("digest-h".to_string(), PathBuf::from("/wt/agent-b/inc/x.h")),
            ("digest-y".to_string(), PathBuf::from("/wt/agent-b/inc/y.h")),
        ];
        let ba: Vec<Vec<u8>> = vec![b"/wt/agent-a".to_vec()];
        let bb: Vec<Vec<u8>> = vec![b"/wt/agent-b".to_vec()];

        // Without basedirs the two checkouts are genuinely different.
        assert_neq!(
            dependency_result_key("manifest", &a, &[]),
            dependency_result_key("manifest", &b, &[])
        );
        assert_eq!(
            dependency_result_key("manifest", &a, &ba),
            dependency_result_key("manifest", &b, &bb),
        );
    }

    /// A changed file must change the key, and the paths and digests are
    /// concatenated into one digest, so they need framing or a boundary can be
    /// shifted undetected.
    #[test]
    fn dependency_keys_distinguish_what_was_read() {
        let base = [("digest-h".to_string(), PathBuf::from("/r/x.h"))];
        let changed = [("digest-i".to_string(), PathBuf::from("/r/x.h"))];
        let renamed = [("digest-h".to_string(), PathBuf::from("/r/y.h"))];
        let extra = [
            ("digest-h".to_string(), PathBuf::from("/r/x.h")),
            ("digest-j".to_string(), PathBuf::from("/r/z.h")),
        ];
        let key = |i: &[(String, PathBuf)]| dependency_result_key("manifest", i, &[]);

        assert_neq!(key(&base), key(&changed));
        assert_neq!(key(&base), key(&renamed));
        assert_neq!(key(&base), key(&extra));
        assert_neq!(
            key(&base),
            dependency_result_key("other-manifest", &base, &[])
        );

        // `/r/x.h` + `digest` and `/r/x.hdigest` + `` must not collide.
        let split = [("d".to_string(), PathBuf::from("/r/x.h"))];
        let joined = [(String::new(), PathBuf::from("/r/x.hd"))];
        assert_neq!(key(&split), key(&joined));
    }

    /// Nothing else in the result key carries the directory a compilation ran
    /// in: `-EP` emits no paths at all, and a build using relative sources puts
    /// none in its arguments either. Two such builds in different directories
    /// share an entry, and the debug information in the object names whichever
    /// one produced it first.
    #[test]
    fn the_working_directory_reaches_the_key() {
        let args = ovec!["-c", "src/x.c"];
        let key_in = |dir: &str, basedirs: &[Vec<u8>]| {
            HashKeyParams::new("abcd", Language::C, &args, b"preprocessed")
                .with_basedirs(basedirs)
                .with_working_directory(Some(Path::new(dir)))
                .compute()
        };

        assert_neq!(key_in("/build/a", &[]), key_in("/build/b", &[]));

        // Declaring a checkout root is the assertion that its path does not
        // matter, and that has to hold here too or it would be the one thing
        // that keeps two checkouts apart. Both when the compilation runs at the
        // root...
        let (ba, bb) = (vec![b"/wt/a/".to_vec()], vec![b"/wt/b/".to_vec()]);
        assert_eq!(key_in("/wt/a", &ba), key_in("/wt/b", &bb));
        // ...and when it runs somewhere under it, which is the usual case.
        assert_eq!(key_in("/wt/a/build", &ba), key_in("/wt/b/build", &bb));
        // But a different directory *within* the checkout is still different.
        assert_neq!(key_in("/wt/a/build", &ba), key_in("/wt/a/other", &ba));
    }

    /// And a build that would rather have the hits can still say so, in which
    /// case the key is exactly what it was before any of this.
    #[test]
    fn the_working_directory_can_be_left_out() {
        let args = ovec!["-c", "src/x.c"];
        let without = |dir: Option<&Path>| {
            HashKeyParams::new("abcd", Language::C, &args, b"preprocessed")
                .with_working_directory(dir)
                .compute()
        };

        assert_eq!(without(None), without(None));
        assert_eq!(
            without(None),
            HashKeyParams::new("abcd", Language::C, &args, b"preprocessed").compute(),
        );
        assert_neq!(without(None), without(Some(Path::new("/build/a"))));
    }

    /// Arguments are concatenated into one digest, so they need framing or
    /// adjacent arguments can be shuffled across the boundary undetected.
    #[test]
    fn basedir_stripped_arguments_stay_distinguishable() {
        let basedirs: Vec<Vec<u8>> = vec![b"/wt/root".to_vec()];
        let split = ovec!["-Ifoo", "bar"];
        let joined = ovec!["-Ifoobar"];
        let h1 = HashKeyParams::new("abcd", Language::C, &split, b"src")
            .with_basedirs(&basedirs)
            .compute();
        let h2 = HashKeyParams::new("abcd", Language::C, &joined, b"src")
            .with_basedirs(&basedirs)
            .compute();
        assert_neq!(h1, h2);
    }

    #[test]
    fn test_plusplus_differs() {
        let args = ovec!["a", "b", "c"];
        let h1 = HashKeyParams::new("abcd", Language::C, &args, b"hello world").compute();
        let h2 = HashKeyParams::new("abcd", Language::C, &args, b"hello world")
            .with_plusplus(true)
            .compute();
        assert_neq!(h1, h2);
    }

    #[test]
    fn test_header_differs() {
        let args = ovec!["a", "b", "c"];
        let h1 = HashKeyParams::new("abcd", Language::C, &args, b"hello world").compute();
        let h2 = HashKeyParams::new("abcd", Language::CHeader, &args, b"hello world").compute();
        assert_neq!(h1, h2);
    }

    #[test]
    fn test_plusplus_header_differs() {
        let args = ovec!["a", "b", "c"];
        let h1 = HashKeyParams::new("abcd", Language::Cxx, &args, b"hello world")
            .with_plusplus(true)
            .compute();
        let h2 = HashKeyParams::new("abcd", Language::CxxHeader, &args, b"hello world")
            .with_plusplus(true)
            .compute();
        assert_neq!(h1, h2);
    }

    #[test]
    fn test_hash_key_executable_contents_differs() {
        let args = ovec!["a", "b", "c"];
        let h1 = HashKeyParams::new("abcd", Language::C, &args, b"hello world").compute();
        let h2 = HashKeyParams::new("wxyz", Language::C, &args, b"hello world").compute();
        assert_neq!(h1, h2);
    }

    #[test]
    fn test_hash_key_args_differs() {
        let abc = ovec!["a", "b", "c"];
        let xyz = ovec!["x", "y", "z"];
        let ab = ovec!["a", "b"];
        let a = ovec!["a"];

        let h_abc = HashKeyParams::new("abcd", Language::C, &abc, b"hello world").compute();
        let h_xyz = HashKeyParams::new("abcd", Language::C, &xyz, b"hello world").compute();
        let h_ab = HashKeyParams::new("abcd", Language::C, &ab, b"hello world").compute();
        let h_a = HashKeyParams::new("abcd", Language::C, &a, b"hello world").compute();

        assert_neq!(h_abc, h_xyz);
        assert_neq!(h_abc, h_ab);
        assert_neq!(h_abc, h_a);
    }

    #[test]
    fn test_hash_key_preprocessed_content_differs() {
        let args = ovec!["a", "b", "c"];
        let h1 = HashKeyParams::new("abcd", Language::C, &args, b"hello world").compute();
        let h2 = HashKeyParams::new("abcd", Language::C, &args, b"goodbye").compute();
        assert_neq!(h1, h2);
    }

    #[test]
    fn test_hash_key_env_var_differs() {
        let args = ovec!["a", "b", "c"];
        for var in CACHED_ENV_VARS.iter() {
            let h1 = HashKeyParams::new("abcd", Language::C, &args, b"hello world").compute();

            let vars1 = vec![(OsString::from(var), OsString::from("something"))];
            let h2 = HashKeyParams::new("abcd", Language::C, &args, b"hello world")
                .with_env_vars(&vars1)
                .compute();

            let vars2 = vec![(OsString::from(var), OsString::from("something else"))];
            let h3 = HashKeyParams::new("abcd", Language::C, &args, b"hello world")
                .with_env_vars(&vars2)
                .compute();

            assert_neq!(h1, h2);
            assert_neq!(h2, h3);
        }
    }

    #[test]
    fn test_extra_hash_data() {
        let args = ovec!["a", "b", "c"];
        let extra_data = stringvec!["hello", "world"];

        let h1 = HashKeyParams::new("abcd", Language::C, &args, b"hello world")
            .with_extra_hashes(&extra_data)
            .compute();
        let h2 = HashKeyParams::new("abcd", Language::C, &args, b"hello world").compute();

        assert_neq!(h1, h2);
    }

    #[test]
    fn test_hash_key_basedirs() {
        let args = ovec!["a", "b", "c"];
        let preprocessed1 = b"# 1 \"/home/user1/project/src/main.c\"\nint main() { return 0; }";
        let preprocessed2 = b"# 1 \"/home/user2/project/src/main.c\"\nint main() { return 0; }";
        let basedirs = [
            b"/home/user1/project".to_vec(),
            b"/home/user2/project".to_vec(),
        ];

        // Helper to compute with basedirs
        let hash_with_basedirs = |output: &[u8], dirs: &[Vec<u8>]| {
            HashKeyParams::new("abcd", Language::C, &args, output)
                .with_basedirs(dirs)
                .compute()
        };

        // Test 1: Same hash with different absolute paths when basedir is used
        let h1 = hash_with_basedirs(preprocessed1, &basedirs);
        let h2 = hash_with_basedirs(preprocessed2, &basedirs);
        assert_eq!(h1, h2);

        // Test 2: Same hash with single basedir that matches each
        assert_eq!(
            hash_with_basedirs(preprocessed1, &basedirs[..1]),
            hash_with_basedirs(preprocessed2, &basedirs[1..])
        );

        // Test 3: Different hashes without basedir
        assert_neq!(
            HashKeyParams::new("abcd", Language::C, &args, preprocessed1).compute(),
            HashKeyParams::new("abcd", Language::C, &args, preprocessed2).compute()
        );

        // Test 4: Works for C++ files too
        let preprocessed_cpp1 =
            b"# 1 \"/home/user1/project/src/main.cpp\"\nint main() { return 0; }";
        let preprocessed_cpp2 =
            b"# 1 \"/home/user2/project/src/main.cpp\"\nint main() { return 0; }";
        let h_cpp1 = HashKeyParams::new("abcd", Language::Cxx, &args, preprocessed_cpp1)
            .with_plusplus(true)
            .with_basedirs(&basedirs)
            .compute();
        let h_cpp2 = HashKeyParams::new("abcd", Language::Cxx, &args, preprocessed_cpp2)
            .with_plusplus(true)
            .with_basedirs(&basedirs)
            .compute();
        assert_eq!(h_cpp1, h_cpp2);

        // Test 5: Doesn't work with trailing slash in basedir, they must be normalized in config
        let basedir_slash = b"/home/user1/project/".to_vec();
        let h_slash = hash_with_basedirs(preprocessed1, std::slice::from_ref(&basedir_slash));
        assert_neq!(h1, h_slash);

        // Test 6: Multiple basedirs - longest match wins
        let multi_basedirs = vec![
            b"/home/user1".to_vec(),
            b"/home/user1/project".to_vec(), // This should match (longest)
        ];
        assert_eq!(h1, hash_with_basedirs(preprocessed1, &multi_basedirs));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn test_hash_key_basedirs_windows_escaped_backslashes() {
        // On Windows, paths in preprocessor output are C string literals with
        // escaped backslashes (e.g. MSVC: #line 1 "D:\\dir\\file.c", clang:
        // # 1 "D:\\dir\\file.c"). The same source compiled from two different
        // directories must produce the same hash when both are basedirs.
        let args = ovec!["a", "b", "c"];
        let preprocessed1 =
            b"#line 1 \"D:\\\\dev\\\\user1\\\\project\\\\src\\\\main.c\"\nint main() { return 0; }";
        let preprocessed2 =
            b"#line 1 \"E:\\\\work\\\\user2\\\\checkout\\\\src\\\\main.c\"\nint main() { return 0; }";
        // As normalized by Config: lowercase, forward slashes, trailing slash
        let basedirs = [
            b"d:/dev/user1/project/".to_vec(),
            b"e:/work/user2/checkout/".to_vec(),
        ];

        let hash_with_basedirs = |output: &[u8], dirs: &[Vec<u8>]| {
            HashKeyParams::new("abcd", Language::C, &args, output)
                .with_basedirs(dirs)
                .compute()
        };

        // Same hash with different absolute (escaped backslash) paths
        let h1 = hash_with_basedirs(preprocessed1, &basedirs);
        let h2 = hash_with_basedirs(preprocessed2, &basedirs);
        assert_eq!(h1, h2);

        // Different hashes without basedirs
        assert_neq!(
            HashKeyParams::new("abcd", Language::C, &args, preprocessed1).compute(),
            HashKeyParams::new("abcd", Language::C, &args, preprocessed2).compute()
        );
    }

    #[test]
    fn test_language_from_file_name() {
        fn t(extension: &str, expected: Language) {
            let path_str = format!("input.{}", extension);
            let path = Path::new(&path_str);
            let actual = Language::from_file_name(path);
            assert_eq!(actual, Some(expected));
        }

        t("s", Language::Assembler);
        t("S", Language::AssemblerToPreprocess);
        t("sx", Language::AssemblerToPreprocess);

        t("c", Language::C);

        t("i", Language::CPreprocessed);

        t("C", Language::Cxx);
        t("cc", Language::Cxx);
        t("cp", Language::Cxx);
        t("cpp", Language::Cxx);
        t("CPP", Language::Cxx);
        t("cxx", Language::Cxx);
        t("c++", Language::Cxx);

        t("ii", Language::CxxPreprocessed);

        t("h", Language::GenericHeader);

        t("hh", Language::CxxHeader);
        t("H", Language::CxxHeader);
        t("hp", Language::CxxHeader);
        t("hxx", Language::CxxHeader);
        t("hpp", Language::CxxHeader);
        t("HPP", Language::CxxHeader);
        t("h++", Language::CxxHeader);
        t("tcc", Language::CxxHeader);

        t("m", Language::ObjectiveC);

        t("mi", Language::ObjectiveCPreprocessed);

        t("M", Language::ObjectiveCxx);
        t("mm", Language::ObjectiveCxx);

        t("mii", Language::ObjectiveCxxPreprocessed);

        t("cu", Language::Cuda);
        t("hip", Language::Hip);
    }

    #[test]
    fn test_language_from_file_name_none() {
        fn t(extension: &str) {
            let path_str = format!("input.{}", extension);
            let path = Path::new(&path_str);
            let actual = Language::from_file_name(path);
            let expected = None;
            assert_eq!(actual, expected);
        }

        // gcc parses file-extensions as case-sensitive
        t("Cp");
        t("Cpp");
        t("Hp");
        t("Hpp");
        t("Mm");
        t("Cu");
    }

    #[test]
    fn test_process_preprocessed_file() {
        env_logger::builder()
            .is_test(true)
            .filter_level(log::LevelFilter::Debug)
            .try_init()
            .ok();
        let input_file = Path::new("tests/test.c");
        let path = Path::new(file!())
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        // This should be portable since the only headers present in this
        // output are system headers, which aren't interacted with
        // on the filesystem if configured.
        let path = path.join("tests/test.c.gcc-13.2.0-preproc");
        let mut bytes = std::fs::read(path).unwrap();
        let original_bytes = bytes.clone();
        let mut include_files = HashMap::new();

        let config = PreprocessorCacheModeConfig {
            use_preprocessor_cache_mode: true,
            skip_system_headers: true,
            ..Default::default()
        };
        let success = process_preprocessed_file(
            input_file,
            Path::new(""),
            &mut bytes,
            &mut include_files,
            config,
            std::time::SystemTime::now(),
            StandardFsAbstraction,
        )
        .unwrap();
        assert_eq!(&bytes, &original_bytes);
        assert!(success);
        assert_eq!(include_files.len(), 0);
    }

    /// `#line N "file"` linemarkers must be recognised. This is the form MSVC emits
    /// under `-E` (and HP/AIX compilers under any setting), and the detection was
    /// comparing a 4-byte slice against the 5-byte literal `b"line "`, so it never
    /// matched. The visible effect was an always-empty include list, which meant the
    /// preprocessor cache entry was never stored and direct mode could never hit.
    #[test]
    fn test_process_preprocessed_file_detects_line_directives() {
        let tempdir = tempfile::Builder::new()
            .prefix("sccache_line_directive")
            .tempdir()
            .unwrap();
        let header = tempdir.path().join("hdr.h");
        std::fs::write(&header, b"#define VALUE 0\n").unwrap();
        let input_file = tempdir.path().join("t.c");
        std::fs::write(
            &input_file,
            b"#include \"hdr.h\"\nint main(void){return VALUE;}\n",
        )
        .unwrap();

        // MSVC `-E` output shape. Backslashes are escaped in the emitted path.
        let mut bytes = format!(
            "#line 1 \"{path}\"\n\n#line 2 \"t.c\"\nint main(void){{ return 0; }}\n",
            path = header.display().to_string().replace('\\', "\\\\")
        )
        .into_bytes();

        let mut include_files = HashMap::new();
        let config = PreprocessorCacheModeConfig {
            use_preprocessor_cache_mode: true,
            skip_system_headers: true,
            ..Default::default()
        };
        let success = process_preprocessed_file(
            &input_file,
            tempdir.path(),
            &mut bytes,
            &mut include_files,
            config,
            // Well after the files were written, so `include_is_too_new` can't
            // suppress them and make this test pass for the wrong reason.
            std::time::SystemTime::now() + std::time::Duration::from_secs(600),
            StandardFsAbstraction,
        )
        .unwrap();

        assert!(success);
        assert_eq!(
            include_files.len(),
            1,
            "expected the #line header to be recorded, got {include_files:?}"
        );
        assert!(include_files.contains_key(&header));
    }

    /// A filesystem interface that only panics to test that we don't access it.
    struct PanicFs;

    impl PreprocessorFSAbstraction for PanicFs {
        fn metadata(&self, path: impl AsRef<Path>) -> io::Result<PreprocessorFileMetadata> {
            panic!("called metadata at {}", path.as_ref().display());
        }

        fn open(&self, path: impl AsRef<Path>) -> io::Result<Box<dyn std::io::Read>> {
            panic!("called open at {}", path.as_ref().display());
        }
    }

    /// A filesystem interface that gives back expected values.
    struct TestFs {
        metadata_results: Mutex<VecDeque<(PathBuf, PreprocessorFileMetadata)>>,
        open_results: Mutex<VecDeque<(PathBuf, Box<dyn std::io::Read>)>>,
    }

    impl PreprocessorFSAbstraction for TestFs {
        fn metadata(&self, path: impl AsRef<Path>) -> io::Result<PreprocessorFileMetadata> {
            let (expected_path, meta) = self
                .metadata_results
                .lock()
                .unwrap()
                .pop_front()
                .expect("not enough 'metadata' results");
            assert_eq!(expected_path, path.as_ref(), "{}", path.as_ref().display());
            Ok(meta)
        }

        fn open(&self, path: impl AsRef<Path>) -> io::Result<Box<dyn std::io::Read>> {
            let (expected_path, impls_read) = self
                .open_results
                .lock()
                .unwrap()
                .pop_front()
                .expect("not enough 'open' results");
            assert_eq!(expected_path, path.as_ref(), "{}", path.as_ref().display());
            Ok(impls_read)
        }
    }

    // Short-circuit the parameters we don't need to change during tests
    fn do_single_preprocessor_line_call(
        line: &[u8],
        include_files: &mut HashMap<PathBuf, String>,
        fs_impl: &impl PreprocessorFSAbstraction,
        skip_system_headers: bool,
    ) -> PreprocessedLineAction {
        let input_file = Path::new("tests/test.c");

        let config = PreprocessorCacheModeConfig {
            use_preprocessor_cache_mode: true,
            skip_system_headers,
            ..Default::default()
        };

        let mut bytes = line.to_vec();
        let total_len = bytes.len();
        process_preprocessor_line(
            input_file,
            Path::new(""),
            include_files,
            config,
            std::time::SystemTime::now(),
            &mut bytes,
            0,
            0,
            total_len,
            &mut HashMap::new(),
            fs_impl,
        )
        .unwrap()
    }

    /// Test cases where we don't access the filesystem
    #[test]
    fn test_process_preprocessor_line_simple() {
        env_logger::builder()
            .is_test(true)
            .filter_level(log::LevelFilter::Debug)
            .try_init()
            .ok();

        let mut include_files = HashMap::new();
        assert_eq!(
            do_single_preprocessor_line_call(
                br#"// # 0 "tests/test.c""#,
                &mut include_files,
                &PanicFs,
                true,
            ),
            ControlFlow::Continue((20, 20)),
        );
        assert_eq!(include_files.len(), 0);

        assert_eq!(
            do_single_preprocessor_line_call(
                br#"// # 0 "<built-in>""#,
                &mut include_files,
                &PanicFs,
                true,
            ),
            ControlFlow::Continue((18, 18)),
        );
        assert_eq!(include_files.len(), 0);

        assert_eq!(
            do_single_preprocessor_line_call(
                br#"// # 0 "<command-line>""#,
                &mut include_files,
                &PanicFs,
                true,
            ),
            ControlFlow::Continue((22, 22)),
        );
        assert_eq!(include_files.len(), 0);

        assert_eq!(
            do_single_preprocessor_line_call(
                br#"// # 0 "<command-line>" 2"#,
                &mut include_files,
                &PanicFs,
                true,
            ),
            ControlFlow::Continue((22, 22)),
        );
        assert_eq!(include_files.len(), 0);

        assert_eq!(
            do_single_preprocessor_line_call(
                br#"// # 1 "tests/test.c""#,
                &mut include_files,
                &PanicFs,
                true,
            ),
            ControlFlow::Continue((20, 20)),
        );
        assert_eq!(include_files.len(), 0);

        assert_eq!(
            do_single_preprocessor_line_call(
                br#"// # 1 "/usr/include/stdc-predef.h" 1 3 4"#,
                &mut include_files,
                &PanicFs,
                true,
            ),
            ControlFlow::Continue((34, 34)),
        );
        assert_eq!(include_files.len(), 0);
    }

    /// Test cases where we test our tests...
    #[test]
    fn test_test_helpers() {
        env_logger::builder()
            .is_test(true)
            .filter_level(log::LevelFilter::Debug)
            .try_init()
            .ok();

        // Test PanicFs
        let res = std::panic::catch_unwind(|| {
            let mut include_files = HashMap::new();
            assert_eq!(
                do_single_preprocessor_line_call(
                    br#"// # 1 "/usr/include/stdc-predef.h" 1 3 4"#,
                    &mut include_files,
                    &PanicFs,
                    false,
                ),
                ControlFlow::Continue((34, 34)),
            );
        });
        assert_eq!(
            res.unwrap_err().downcast_ref::<String>().unwrap(),
            "called metadata at /usr/include/stdc-predef.h"
        );

        // Test TestFs's safeguard
        let res = std::panic::catch_unwind(|| {
            let mut include_files = HashMap::new();
            let fs_impl = TestFs {
                metadata_results: Mutex::new(VecDeque::new()),
                open_results: Mutex::new(VecDeque::new()),
            };
            assert_eq!(
                do_single_preprocessor_line_call(
                    br#"// # 33 "/usr/include/x86_64-linux-gnu/bits/libc-header-start.h" 3 4"#,
                    &mut include_files,
                    &fs_impl,
                    false,
                ),
                ControlFlow::Continue((34, 34)),
            );
        });
        assert_eq!(
            res.unwrap_err().downcast_ref::<String>().unwrap(),
            "not enough 'metadata' results"
        );
    }

    /// Test cases where we test filesystem access
    #[test]
    fn test_process_preprocessor_line_fs_access() {
        env_logger::builder()
            .is_test(true)
            .filter_level(log::LevelFilter::Debug)
            .try_init()
            .ok();

        // Test "too new" include file
        let mut include_files = HashMap::new();
        let fs_impl = TestFs {
            metadata_results: Mutex::new(
                [(
                    PathBuf::from("/usr/include/x86_64-linux-gnu/bits/libc-header-start.h"),
                    PreprocessorFileMetadata {
                        is_dir: false,
                        is_file: true,
                        modified: Some(Timestamp::new(i64::MAX - 1, 0)),
                        ctime_or_creation: None,
                    },
                )]
                .into_iter()
                .collect(),
            ),
            open_results: Mutex::new(VecDeque::new()),
        };
        assert_eq!(
            do_single_preprocessor_line_call(
                br#"// # 33 "/usr/include/x86_64-linux-gnu/bits/libc-header-start.h" 3 4"#,
                &mut include_files,
                &fs_impl,
                false,
            ),
            // preprocessor cache mode is disabled
            ControlFlow::Break((63, 9, false)),
        );

        // Test invalid include file is actually a dir
        let mut include_files = HashMap::new();
        let fs_impl = TestFs {
            metadata_results: Mutex::new(
                [(
                    PathBuf::from("/usr/include/x86_64-linux-gnu/bits/libc-header-start.h"),
                    PreprocessorFileMetadata {
                        is_dir: true,
                        is_file: false,
                        modified: Some(Timestamp::new(12341234, 0)),
                        ctime_or_creation: None,
                    },
                )]
                .into_iter()
                .collect(),
            ),
            open_results: Mutex::new(VecDeque::new()),
        };
        assert_eq!(
            do_single_preprocessor_line_call(
                br#"// # 33 "/usr/include/x86_64-linux-gnu/bits/libc-header-start.h" 3 4"#,
                &mut include_files,
                &fs_impl,
                false,
            ),
            // preprocessor cache mode is *not* disabled,
            ControlFlow::Continue((63, 63)),
        );
        assert_eq!(include_files.len(), 0);

        // Test correct include file
        let mut include_files = HashMap::new();
        let fs_impl = TestFs {
            metadata_results: Mutex::new(
                [(
                    PathBuf::from("/usr/include/x86_64-linux-gnu/bits/libc-header-start.h"),
                    PreprocessorFileMetadata {
                        is_dir: false,
                        is_file: true,
                        modified: Some(Timestamp::new(12341234, 0)),
                        ctime_or_creation: None,
                    },
                )]
                .into_iter()
                .collect(),
            ),
            open_results: Mutex::new(
                [(
                    PathBuf::from("/usr/include/x86_64-linux-gnu/bits/libc-header-start.h"),
                    Box::new(&b"contents"[..]) as Box<dyn std::io::Read>,
                )]
                .into_iter()
                .collect(),
            ),
        };
        assert_eq!(
            do_single_preprocessor_line_call(
                br#"// # 33 "/usr/include/x86_64-linux-gnu/bits/libc-header-start.h" 3 4"#,
                &mut include_files,
                &fs_impl,
                false,
            ),
            ControlFlow::Continue((63, 63)),
        );
        assert_eq!(include_files.len(), 1);
        assert_eq!(
            include_files
                .get(Path::new(
                    "/usr/include/x86_64-linux-gnu/bits/libc-header-start.h",
                ))
                .unwrap(),
            // hash of `b"contents"`
            "a93900c371d997927c5bc568ea538bed59ae5c960021dcfe7b0b369da5267528",
        );
    }
}

/// Dependency keying: performing the compilation to learn what it read, instead
/// of preprocessing so the same list can be recovered from the text.
#[cfg(test)]
mod dependency_keying_test {
    use super::*;
    use crate::compiler::msvc::Msvc;
    use crate::mock_command::{ExitStatusValue, MockChild, MockCommandCreator, exit_status};
    use crate::test::mock_storage::MockStorage;
    use crate::test::utils::*;
    use std::sync::Mutex;

    fn msvc_hasher(cwd: &Path, arguments: &[OsString]) -> CCompilerHasher<Msvc> {
        let parsed_args = match crate::compiler::msvc::parse_arguments(arguments, cwd, false) {
            CompilerArguments::Ok(p) => p,
            o => panic!("Bad result from parse_arguments: {:?}", o),
        };
        CCompilerHasher {
            parsed_args,
            executable: cwd.join("cl.exe"),
            executable_digest: "compiler-digest".to_string(),
            compiler: Msvc {
                includes_prefix: String::new(),
                is_clang: false,
                version: None,
            },
        }
    }

    /// Stand in for cl.exe: write the `/sourceDependencies` report it was asked
    /// for, and record the command line so the test can see what ran.
    fn mock_compiler(
        seen: Arc<Mutex<Vec<OsString>>>,
        report: Option<Vec<PathBuf>>,
        status: ExitStatusValue,
    ) -> impl Fn(&[OsString]) -> Result<MockChild> + Send + 'static {
        let body = report.map(|includes| {
            let includes: Vec<_> = includes
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            serde_json::to_string(&serde_json::json!({
                "Version": "1.2",
                "Data": {"Source": "foo.c", "Includes": includes},
            }))
            .unwrap()
        });
        mock_compiler_raw(seen, body, status)
    }

    /// As [`mock_compiler`], writing `report` to the reported-inputs path
    /// verbatim. Writing nothing and writing something unreadable are different
    /// failures and this is what lets a test tell them apart.
    fn mock_compiler_raw(
        seen: Arc<Mutex<Vec<OsString>>>,
        report: Option<String>,
        status: ExitStatusValue,
    ) -> impl Fn(&[OsString]) -> Result<MockChild> + Send + 'static {
        move |args: &[OsString]| {
            *seen.lock().unwrap() = args.to_vec();
            if let Some(body) = &report {
                // `/sourceDependencies` takes its path joined or separated, and
                // a caller's own flag arrives in whichever form they wrote it.
                let path = args
                    .iter()
                    .enumerate()
                    .find_map(|(i, a)| {
                        let rest = a.to_string_lossy();
                        let rest = rest.strip_prefix("/sourceDependencies")?;
                        if rest.is_empty() {
                            args.get(i + 1).map(PathBuf::from)
                        } else {
                            Some(PathBuf::from(rest))
                        }
                    })
                    .expect("the compilation was never asked to report its inputs");
                std::fs::write(&path, body).unwrap();
            }
            Ok(MockChild::new(exit_status(status), "compiling foo.c", ""))
        }
    }

    struct Run {
        key: String,
        cacheable: Cacheable,
        command_line: Vec<OsString>,
        manifests: Vec<(String, PreprocessorCacheEntry)>,
    }

    /// Hash one compilation with dependency keying on, and report everything
    /// worth asserting about it.
    fn run(
        cwd: &Path,
        arguments: &[OsString],
        report: Option<Vec<PathBuf>>,
        status: ExitStatusValue,
    ) -> Run {
        run_with(cwd, arguments, report, status, Some(true))
    }

    /// A manifest sccache cannot read must be a miss, not a failure.
    ///
    /// Every entry written before a format change is one of these, so treating
    /// it as an error turns an upgrade into a build that stops until someone
    /// clears the cache by hand -- and the same goes for a single truncated
    /// file.
    #[test]
    fn a_manifest_that_cannot_be_read_is_a_miss() {
        let (_f, cwd, hdr) = fixture();
        let args = ovec!["-c", "foo.c", "-Fofoo.obj"];
        // A version byte from a format that no longer exists.
        let stale = vec![0xFEu8, 1, 2, 3];

        let runtime = single_threaded_runtime();
        let creator = new_creator();
        let seen = Arc::new(Mutex::new(Vec::new()));
        next_command_calls(&creator, mock_compiler(seen, Some(vec![hdr]), 0));
        let storage = Arc::new(
            MockStorage::new(None, true)
                .with_dependency_keying(true)
                .with_preprocessor_entry(stale),
        );

        let result = msvc_hasher(&cwd, &args)
            .generate_hash_key(
                &creator,
                cwd.clone(),
                vec![],
                false,
                runtime.handle(),
                false,
                storage.clone(),
                CacheControl::Default,
            )
            .wait();

        assert!(
            result.is_ok(),
            "an unreadable manifest must not fail the compilation: {:?}",
            result.err()
        );
        assert_eq!(
            storage.manifests().len(),
            1,
            "and the compilation should replace it"
        );
    }

    /// Nothing asks for dependency keying: not the invocation, not the project.
    /// It is on because that is the default.
    #[test]
    fn it_is_on_by_default() {
        let (_f, cwd, hdr) = fixture();
        let args = ovec!["-c", "foo.c", "-Fofoo.obj"];

        let r = run_full(&cwd, &args, Some(vec![hdr]), 0, None, true);

        assert!(
            !r.command_line.iter().any(|a| a == "-E" || a == "-EP"),
            "the default should have kept the preprocessor out of it: {:?}",
            r.command_line
        );
        assert_eq!(r.manifests.len(), 1);
        assert_eq!(r.cacheable, Cacheable::Yes);
    }

    /// And a project that wants the old behaviour can still say so.
    #[test]
    fn a_project_can_decline_the_default() {
        let (_f, cwd, hdr) = fixture();
        std::fs::write(
            cwd.join(crate::projectconf::PROJECT_CONF_NAME),
            "use_dependency_keying = false\n",
        )
        .unwrap();
        let args = ovec!["-c", "foo.c", "-Fofoo.obj"];

        let r = run_full(&cwd, &args, Some(vec![hdr]), 0, None, true);

        assert!(
            r.command_line.iter().any(|a| a == "-E" || a == "-EP"),
            "the project declined, so the preprocessor should be in charge: {:?}",
            r.command_line
        );
    }

    /// As [`run`], with the `SCCACHE_DEPEND` this invocation carries -- `None`
    /// for one that says nothing, leaving the decision to the project file and
    /// then to the machine-wide setting.
    ///
    /// The machine-wide setting is pinned *off*, so that a test asking whether
    /// something else turned keying on is answering that question and not the
    /// default's.
    fn run_with(
        cwd: &Path,
        arguments: &[OsString],
        report: Option<Vec<PathBuf>>,
        status: ExitStatusValue,
        depend: Option<bool>,
    ) -> Run {
        run_full(cwd, arguments, report, status, depend, false)
    }

    /// As [`run_with`], also pinning the machine-wide setting.
    fn run_full(
        cwd: &Path,
        arguments: &[OsString],
        report: Option<Vec<PathBuf>>,
        status: ExitStatusValue,
        depend: Option<bool>,
        machine: bool,
    ) -> Run {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let creator = new_creator();
        next_command_calls(&creator, mock_compiler(seen.clone(), report, status));
        hash_one(cwd, arguments, creator, seen, depend, machine)
    }

    /// As [`run`], with a compiler whose report is written verbatim.
    fn run_raw(
        cwd: &Path,
        arguments: &[OsString],
        report: Option<&str>,
        status: ExitStatusValue,
    ) -> Run {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let creator = new_creator();
        next_command_calls(
            &creator,
            mock_compiler_raw(seen.clone(), report.map(str::to_owned), status),
        );
        hash_one(cwd, arguments, creator, seen, Some(true), false)
    }

    /// Hash one compilation against an already-prepared compiler.
    fn hash_one(
        cwd: &Path,
        arguments: &[OsString],
        creator: Arc<Mutex<MockCommandCreator>>,
        seen: Arc<Mutex<Vec<OsString>>>,
        depend: Option<bool>,
        machine: bool,
    ) -> Run {
        let runtime = single_threaded_runtime();
        let storage = Arc::new(MockStorage::new(None, true).with_dependency_keying(machine));
        let env_vars: Vec<(OsString, OsString)> = depend
            .map(|d| {
                vec![(
                    "SCCACHE_DEPEND".into(),
                    if d { "true" } else { "false" }.into(),
                )]
            })
            .unwrap_or_default();

        let result = msvc_hasher(cwd, arguments)
            .generate_hash_key(
                &creator,
                cwd.to_path_buf(),
                env_vars,
                false,
                runtime.handle(),
                false,
                storage.clone(),
                CacheControl::Default,
            )
            .wait()
            .expect("hashing a failed compilation is not itself a failure");

        // Exactly one process ran. Two would mean a preprocessor pass survived.
        assert!(
            creator.lock().unwrap().children.is_empty(),
            "a queued command went unused, so something other than the compilation ran"
        );
        let (_, _, cacheable) =
            Compilation::<Arc<Mutex<MockCommandCreator>>>::generate_compile_commands(
                result.compilation.as_ref(),
                &mut dist::PathTransformer::new(),
                false,
            )
            .unwrap();
        let command_line = seen.lock().unwrap().clone();
        Run {
            key: result.key,
            cacheable,
            command_line,
            manifests: storage.manifests(),
        }
    }

    fn fixture() -> (TestFixture, PathBuf, PathBuf) {
        let f = TestFixture::new();
        let cwd = f.tempdir.path().to_path_buf();
        std::fs::write(cwd.join("foo.c"), "#include \"hdr.h\"\n").unwrap();
        let hdr = cwd.join("hdr.h");
        std::fs::write(&hdr, "// original").unwrap();
        (f, cwd, hdr)
    }

    /// The compilation itself is what runs, and the key follows what it
    /// reported reading.
    ///
    /// The mock returns identical output whatever the header says, so a key
    /// taken from preprocessed text cannot tell these two runs apart. That is
    /// the assertion that fails without dependency keying.
    #[test]
    fn a_changed_header_changes_the_key() {
        let (_f, cwd, hdr) = fixture();
        let args = ovec!["-c", "foo.c", "-Fofoo.obj"];

        let first = run(&cwd, &args, Some(vec![hdr.clone()]), 0);
        std::fs::write(&hdr, "// changed").unwrap();
        let second = run(&cwd, &args, Some(vec![hdr.clone()]), 0);

        assert_neq!(first.key, second.key);

        // What ran was the compilation, not a preprocessor pass.
        assert!(
            first.command_line.contains(&OsString::from("-Fofoo.obj")),
            "expected the compile command, got {:?}",
            first.command_line
        );
        assert!(
            !first.command_line.iter().any(|a| a == "-E" || a == "-EP"),
            "a preprocessor pass survived: {:?}",
            first.command_line
        );

        // And what it read was recorded, so the next compilation of this source
        // needs neither a preprocessor nor a compiler.
        assert_eq!(first.manifests.len(), 1);
        assert_eq!(first.cacheable, Cacheable::Yes);
    }

    /// A caller that already asked for a report gets its own file used. A second
    /// `/sourceDependencies` would fight over the path with theirs.
    #[test]
    fn a_callers_own_report_is_reused() {
        let (_f, cwd, hdr) = fixture();
        let args = ovec![
            "-c",
            "foo.c",
            "-Fofoo.obj",
            "/sourceDependenciesfoo.obj.json"
        ];

        let r = run(&cwd, &args, Some(vec![hdr]), 0);

        let asked = r
            .command_line
            .iter()
            .filter(|a| a.to_string_lossy().starts_with("/sourceDependencies"))
            .count();
        assert_eq!(
            asked, 1,
            "the caller's report should be the only one asked for: {:?}",
            r.command_line
        );
        // And it is theirs that was written, where they asked for it.
        assert!(cwd.join("foo.obj.json").exists());
        assert_eq!(r.cacheable, Cacheable::Yes);
    }

    /// A compilation that opens nothing is still cacheable. Qt's generated
    /// `qrc_*.cpp` is a byte array with no `#include` in it, and reading the
    /// compiler's "I opened nothing" as "I could not answer" made every
    /// resource file in a Qt build uncacheable -- measured on the reference
    /// build as 59 translation units recompiled on every warm build.
    #[test]
    fn a_compilation_that_reads_nothing_is_still_cacheable() {
        let (_f, cwd, _hdr) = fixture();
        std::fs::write(cwd.join("foo.c"), "int f(void){return 0;}\n").unwrap();
        let args = ovec!["-c", "foo.c", "-Fofoo.obj"];

        // The compiler reports an empty include list, which is the truth.
        let r = run(&cwd, &args, Some(Vec::new()), 0);

        assert_eq!(r.cacheable, Cacheable::Yes);
        assert_eq!(
            r.manifests.len(),
            1,
            "with nothing to verify, the manifest still identifies the compilation"
        );
    }

    /// A report that never arrived must stop the result being cached. Anything
    /// else records a dependency list that is quietly short, which causes a
    /// later *hit* on a stale result rather than a miss.
    #[test]
    fn a_missing_report_refuses_the_cache() {
        let (_f, cwd, _hdr) = fixture();
        let args = ovec!["-c", "foo.c", "-Fofoo.obj"];

        let r = run(&cwd, &args, None, 0);

        assert_eq!(
            r.cacheable,
            Cacheable::No(NotCacheable::DependencyReportMissing)
        );
        assert!(
            r.manifests.is_empty(),
            "nothing verifiable was read, so nothing may be recorded to verify against"
        );
    }

    /// "The report could not be parsed" and "the report said nothing was read"
    /// arrive as the same absence of a dependency list, and conflating them is
    /// what made every Qt `qrc_*.cpp` uncacheable. So the two must not reduce to
    /// one verdict: this refuses the cache, and the empty report above does not.
    #[test]
    fn an_unparseable_report_is_distinct_from_an_empty_one() {
        let (_f, cwd, _hdr) = fixture();
        let args = ovec!["-c", "foo.c", "-Fofoo.obj"];

        let r = run_raw(&cwd, &args, Some("not the JSON you are looking for"), 0);

        assert_eq!(
            r.cacheable,
            Cacheable::No(NotCacheable::DependencyReportUnreadable)
        );
        assert!(r.manifests.is_empty());
    }

    /// Whether this pays off depends on how the project is built, so a
    /// committed `.sccache.toml` can ask for it with no environment variable
    /// and no machine-wide setting -- which is what a build the whole team
    /// shares needs.
    #[test]
    fn a_project_file_can_ask_for_it() {
        let (_f, cwd, hdr) = fixture();
        std::fs::write(
            cwd.join(crate::projectconf::PROJECT_CONF_NAME),
            "use_dependency_keying = true\n",
        )
        .unwrap();
        let args = ovec!["-c", "foo.c", "-Fofoo.obj"];

        let r = run_with(&cwd, &args, Some(vec![hdr]), 0, None);

        // The compilation ran, not a preprocessor pass.
        assert!(
            !r.command_line.iter().any(|a| a == "-E" || a == "-EP"),
            "the project file was not honoured: {:?}",
            r.command_line
        );
        assert_eq!(r.manifests.len(), 1);
    }

    /// A project can extend and invalidate its own keys, and two checkouts of
    /// it agree -- the file's directory is a basedir, so nothing about where
    /// the tree happens to sit reaches the key.
    #[test]
    fn a_project_can_extend_and_bust_its_own_keys() {
        let key_for = |conf: &str, extra: &str| {
            // A fresh checkout each time: same project, different root.
            let (_f, cwd, hdr) = fixture();
            std::fs::write(cwd.join("extra.txt"), extra).unwrap();
            std::fs::write(
                cwd.join(crate::projectconf::PROJECT_CONF_NAME),
                format!("use_dependency_keying = true\nextra_hash_files = [\"extra.txt\"]\n{conf}"),
            )
            .unwrap();
            let args = ovec!["-c", "foo.c", "-Fofoo.obj"];
            run_with(&cwd, &args, Some(vec![hdr]), 0, None).key
        };

        assert_eq!(
            key_for("", "shared"),
            key_for("", "shared"),
            "two checkouts of one project must agree"
        );
        // The contents of a file the project named reach the key.
        assert_neq!(key_for("", "shared"), key_for("", "changed"));
        // And so does the buster, which is the point of having one.
        assert_neq!(
            key_for("", "shared"),
            key_for("custom_cache_buster = \"v2\"\n", "shared")
        );
    }

    /// A project that declines preprocessor cache mode gets no manifest, and
    /// therefore no dependency keying either -- it has nothing to key against.
    #[test]
    fn a_project_can_decline_preprocessor_cache_mode() {
        let (_f, cwd, hdr) = fixture();
        std::fs::write(
            cwd.join(crate::projectconf::PROJECT_CONF_NAME),
            "use_preprocessor_cache_mode = false\nuse_dependency_keying = true\n",
        )
        .unwrap();
        let args = ovec!["-c", "foo.c", "-Fofoo.obj"];

        // Nothing asks the compiler to report here, so there is no report to
        // write: without a manifest there is nothing to key a report against.
        let _ = hdr;
        let r = run_with(&cwd, &args, None, 0, None);

        // `-EP` rather than `-E`: with no manifest to reconstruct an include
        // list for, the cheaper form that emits no line markers is preferred.
        assert!(
            r.command_line.iter().any(|a| a == "-E" || a == "-EP"),
            "declining the manifest must leave the preprocessor in charge: {:?}",
            r.command_line
        );
        assert!(
            !r.command_line
                .iter()
                .any(|a| a.to_string_lossy().starts_with("/sourceDependencies")),
            "no manifest means nothing to ask about: {:?}",
            r.command_line
        );
        assert!(r.manifests.is_empty());
    }

    /// The run in front of you outranks the file. Someone bisecting a cache
    /// problem has to be able to turn it off without editing the repository.
    #[test]
    fn the_environment_outranks_the_project_file() {
        let (_f, cwd, hdr) = fixture();
        std::fs::write(
            cwd.join(crate::projectconf::PROJECT_CONF_NAME),
            "use_dependency_keying = true\n",
        )
        .unwrap();
        let args = ovec!["-c", "foo.c", "-Fofoo.obj"];

        let r = run_with(&cwd, &args, Some(vec![hdr]), 0, Some(false));

        assert!(
            r.command_line.iter().any(|a| a == "-E"),
            "SCCACHE_DEPEND=false should have left the preprocessor in charge: {:?}",
            r.command_line
        );
    }

    /// A failed compilation comes back through the normal path, so that it is
    /// reported and counted as a compile failure rather than as an sccache
    /// error -- and is not cached.
    #[test]
    fn a_failed_compilation_is_not_an_error() {
        let (_f, cwd, hdr) = fixture();
        let args = ovec!["-c", "foo.c", "-Fofoo.obj"];

        let r = run(&cwd, &args, Some(vec![hdr]), 1);

        assert_eq!(r.cacheable, Cacheable::No(NotCacheable::CompilationFailed));
        assert!(r.manifests.is_empty());
    }
}
