// SPDX-FileCopyrightText: Copyright natyamatsya and sccache contributors
// SPDX-License-Identifier: Apache-2.0

//! sccion's own settings in the environment, under either prefix.
//!
//! Everything sccion reads is spelled `SCCION_<NAME>`. `SCCACHE_<NAME>` is read
//! as well, and means exactly the same thing.
//!
//! Accepting both is not politeness about a rename. These variables are set in
//! CI configuration, in Dockerfiles, in CMake toolchain files and in shell
//! profiles that nobody edits twice a year -- places the person changing the
//! binary's name has no way to reach. A rename that silently stopped reading
//! them would not report an error; it would report a build that no longer hits
//! the cache, which is the failure this whole program exists to make visible.
//!
//! `SCCION_` wins when both are set, because the more specific spelling is the
//! one someone chose deliberately. Disagreeing values are logged rather than
//! resolved quietly: one of the two is not doing what whoever set it expects.

use std::env;
use std::ffi::{OsStr, OsString};

/// The prefix everything is spelled with.
pub const PREFIX: &str = "SCCION_";

/// The prefix inherited from sccache, still honoured.
pub const LEGACY_PREFIX: &str = "SCCACHE_";

/// The `SCCACHE_` spelling of a `SCCION_` name.
///
/// `None` for anything not under the prefix -- `MACOSX_DEPLOYMENT_TARGET` and
/// the like reach this module too, and they have no second spelling.
fn legacy_name(name: &str) -> Option<String> {
    name.strip_prefix(PREFIX)
        .map(|rest| format!("{LEGACY_PREFIX}{rest}"))
}

/// The value of `name`, falling back to its `SCCACHE_` spelling.
pub fn var_os(name: &str) -> Option<OsString> {
    let current = env::var_os(name);
    let Some(legacy) = legacy_name(name) else {
        return current;
    };
    match (current, env::var_os(&legacy)) {
        (Some(new), Some(old)) => {
            if new != old {
                warn!(
                    "{name} and {legacy} are both set and disagree; using {name}. \
                     {legacy} is the older spelling and is ignored here."
                );
            }
            Some(new)
        }
        (Some(new), None) => Some(new),
        (None, old) => old,
    }
}

/// As [`var_os`], for the many settings that are read as text.
///
/// Anything that is not valid Unicode is reported as absent rather than
/// lossily converted: a half-decoded path or size is worse than no answer,
/// because it is acted on.
pub fn var(name: &str) -> Option<String> {
    var_os(name).and_then(|v| v.into_string().ok())
}

/// The spelling that actually supplied `name`'s value.
///
/// For diagnostics. "SCCION_S3_NO_CREDENTIALS must be 'true' ..." names a
/// variable someone who wrote `SCCACHE_S3_NO_CREDENTIALS` never set, and sends
/// them looking for a second mistake they have not made. Falls back to `name`
/// when neither is set, which is the right answer for a message about
/// something absent.
pub fn name_in_use(name: &str) -> String {
    if env::var_os(name).is_some() {
        return name.to_owned();
    }
    match legacy_name(name) {
        Some(legacy) if env::var_os(&legacy).is_some() => legacy,
        _ => name.to_owned(),
    }
}

/// The spelling of `LOGGING_ENV` that is actually set.
///
/// `env_logger` takes a variable *name* and reads it itself, so the fallback
/// cannot be applied by reading the value here: the answer has to be the name
/// that is set, or a filter someone wrote in `SCCACHE_LOG` is silently ignored
/// and the only symptom is missing log output.
pub fn logging_var() -> &'static str {
    const LEGACY: &str = "SCCACHE_LOG";
    if env::var_os(crate::LOGGING_ENV).is_some() {
        crate::LOGGING_ENV
    } else {
        LEGACY
    }
}

/// Whether `key` is `name` under either spelling.
///
/// For the compile path, which is handed an explicit environment rather than
/// reading the process's own -- the compilation being cached happened
/// somewhere else, possibly on another machine.
pub fn matches(key: &OsStr, name: &str) -> bool {
    key == OsStr::new(name)
        || legacy_name(name).is_some_and(|legacy| key == OsStr::new(legacy.as_str()))
}

/// The value of `name` in an explicit environment, under either spelling.
///
/// `SCCION_` wins, as it does when reading the process's own environment; the
/// scan is done twice rather than once so that order in the list cannot decide
/// which spelling takes effect.
pub fn find<'a>(env_vars: &'a [(OsString, OsString)], name: &'a str) -> Option<&'a OsStr> {
    let by = |wanted: &str| {
        let wanted = OsString::from(wanted);
        env_vars
            .iter()
            .find(|(k, _)| *k == wanted)
            .map(|(_, v)| v.as_os_str())
    };
    by(name).or_else(|| legacy_name(name).and_then(|legacy| by(&legacy)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    /// The whole point: a setup that predates the rename keeps working. If this
    /// breaks, the symptom is not an error -- it is a build that quietly stops
    /// hitting the cache.
    #[test]
    fn the_old_spelling_still_answers() {
        let env = vars(&[("SCCACHE_DEPEND", "true")]);
        assert_eq!(find(&env, "SCCION_DEPEND"), Some(OsStr::new("true")));
        assert!(matches(OsStr::new("SCCACHE_DEPEND"), "SCCION_DEPEND"));
    }

    /// The deliberate spelling wins over the inherited one.
    #[test]
    fn the_new_spelling_outranks_the_old() {
        let env = vars(&[("SCCACHE_DEPEND", "false"), ("SCCION_DEPEND", "true")]);
        assert_eq!(find(&env, "SCCION_DEPEND"), Some(OsStr::new("true")));

        // And not because it came first in the list: whichever order the
        // environment happens to be in, the answer is the same.
        let reversed = vars(&[("SCCION_DEPEND", "true"), ("SCCACHE_DEPEND", "false")]);
        assert_eq!(find(&reversed, "SCCION_DEPEND"), Some(OsStr::new("true")));
    }

    /// Variables that are nobody's to rename must pass through untouched.
    #[test]
    fn a_name_outside_the_prefix_has_only_one_spelling() {
        assert_eq!(legacy_name("MACOSX_DEPLOYMENT_TARGET"), None);
        let env = vars(&[("MACOSX_DEPLOYMENT_TARGET", "11.0")]);
        assert_eq!(
            find(&env, "MACOSX_DEPLOYMENT_TARGET"),
            Some(OsStr::new("11.0"))
        );
        assert!(!matches(
            OsStr::new("SCCACHE_DEPLOYMENT_TARGET"),
            "SCCION_X"
        ));
    }

    #[test]
    fn nothing_set_is_nothing_answered() {
        assert_eq!(find(&vars(&[]), "SCCION_DEPEND"), None);
    }
}
