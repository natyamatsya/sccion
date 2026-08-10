// SPDX-FileCopyrightText: Copyright natyamatsya and sccache contributors
// SPDX-License-Identifier: Apache-2.0

//! Make-style dependency files, as emitted by compilers that can report what
//! they read.
//!
//! This exists to let a compilation's input list be obtained *without*
//! preprocessing it a second time. sccache's direct mode normally recovers that
//! list by scraping `#line` records out of preprocessed output, which requires
//! producing that output at all -- for MSVC an extra `cl -E` on every cold
//! compile, and for Qt's moc a step that costs more than the tool it is meant
//! to avoid running (measured: `moc -E` 249ms against 211ms for moc itself, and
//! it emits no line records to scrape anyway).
//!
//! Every tool of interest can be asked for the list directly instead: MSVC via
//! `/sourceDependencies`, moc via `--output-dep-file`, gcc and clang via `-MD`.
//! The Make-style form is the common denominator.

use std::path::PathBuf;

/// The prerequisites listed in a Make-style dependency file.
///
/// The target is discarded: callers already know what they asked to build, and
/// the interesting part is what was read to build it.
///
/// Malformed input yields whatever could be recovered rather than an error. A
/// dependency list is used to decide whether a cache entry is still valid, and
/// a short list can only cause a false *hit* if a genuine input is missing from
/// it -- so callers must treat an empty result as "cannot cache", never as "no
/// dependencies".
pub fn parse(contents: &str) -> Vec<PathBuf> {
    // Join physical lines into logical ones: a backslash immediately before a
    // newline is a continuation, not an escape.
    let joined = unfold(contents);

    let mut out = Vec::new();
    for logical in joined.lines() {
        let Some(rest) = strip_target(logical) else {
            continue;
        };
        for tok in split_unescaped(rest) {
            if tok.is_empty() {
                continue;
            }
            out.push(PathBuf::from(unescape(&tok)));
        }
    }
    out
}

fn unfold(contents: &str) -> String {
    let mut out = String::with_capacity(contents.len());
    let mut chars = contents.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                // Continuation: swallow the backslash and the newline.
                Some('\n') => {
                    chars.next();
                    out.push(' ');
                }
                Some('\r') => {
                    chars.next();
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    out.push(' ');
                }
                // Any other backslash is kept: it may be a Windows separator or
                // an escape, and `unescape` decides which later.
                _ => out.push(c),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Drop `target:` from the front of a rule line, returning the prerequisites.
///
/// The colon cannot simply be searched for. On Windows the target is routinely
/// an absolute path, so the first colon in `C:\...\moc_x.cpp: dep.h` belongs to
/// the drive letter -- splitting there would take the target to be `C` and turn
/// the rest of its own path into a dependency.
fn strip_target(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2, // escaped character, skip both
            b':' => {
                // A drive letter is a single alphabetic character preceded only
                // by whitespace, and followed by a separator.
                let is_drive = i >= 1
                    && (bytes[i - 1] as char).is_ascii_alphabetic()
                    && line[..i - 1].trim().is_empty()
                    && matches!(bytes.get(i + 1), Some(b'\\') | Some(b'/'));
                if is_drive {
                    i += 1;
                } else {
                    return Some(&line[i + 1..]);
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// Split on whitespace that is not backslash-escaped.
///
/// Paths with spaces are ubiquitous on Windows -- `C:/Program\ Files\ (x86)/...`
/// appears in essentially every dep file MSVC or moc produces there -- and
/// splitting on raw whitespace would shred them into nonexistent fragments.
fn split_unescaped(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                cur.push('\\');
                if let Some(&n) = chars.peek() {
                    cur.push(n);
                    chars.next();
                }
            }
            c if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Undo Make's escaping, leaving Windows separators alone.
///
/// `\ ` and `\#` are escapes; `\d` in `C:\dev` is a path separator. The rule is
/// that a backslash only escapes a character that would otherwise be
/// syntactically significant.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some(&n) if n == ' ' || n == '#' || n == ':' => {
                    out.push(n);
                    chars.next();
                }
                _ => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The includes listed by MSVC's `/sourceDependencies`.
///
/// MSVC reports in JSON rather than Make format, so it needs its own reader,
/// but everything downstream is shared. The shape is
/// `{"Version":"1.2","Data":{"Source":"...","Includes":["...", ...]}}`; only
/// `Data.Includes` is of interest, since the source is already known.
///
/// As with [`parse`], anything less than a clean read yields an empty list, and
/// callers must treat that as "cannot cache" rather than "no includes".
pub fn parse_source_dependencies(contents: &str) -> Vec<PathBuf> {
    read_source_dependencies(contents).unwrap_or_default()
}

/// As [`parse_source_dependencies`], separating a report that could not be read
/// from one that says the compilation opened nothing.
///
/// `None` is "cannot cache". An empty `Some` is a complete answer: a source
/// file that includes nothing has nothing to verify later, and generated
/// resource and lookup-table files are like that by construction -- Qt's
/// `qrc_*.cpp` is a byte array with no `#include` in it at all.
///
/// A non-string entry fails the whole read rather than being skipped, for the
/// usual reason: a list that is quietly short causes a stale *hit*.
pub fn read_source_dependencies(contents: &str) -> Option<Vec<PathBuf>> {
    let v = serde_json::from_str::<serde_json::Value>(contents).ok()?;
    let entries = v.get("Data")?.get("Includes")?.as_array()?;
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        out.push(PathBuf::from(entry.as_str()?));
    }
    Some(out)
}

/// The shape a tool reports its inputs in.
///
/// Which one a compiler speaks is a property of the compiler, so it travels
/// with the request for a report rather than being guessed from the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepFileFormat {
    /// Make rules, as `-MD`/`-MF` and moc's `--output-dep-file` produce.
    Make,
    /// MSVC's `/sourceDependencies` JSON.
    MsvcJson,
}

impl DepFileFormat {
    /// Read a report in this format.
    ///
    /// `None` means the report could not be read, and the compilation must not
    /// be cached. An empty `Some` means the tool said it opened nothing, which
    /// is an answer rather than a missing one -- see
    /// [`read_source_dependencies`].
    pub fn read(self, contents: &str) -> Option<Vec<PathBuf>> {
        match self {
            // Make cannot tell the two apart: a rule with no prerequisites and
            // a file that failed to parse both arrive as an empty list. So this
            // format keeps the conservative reading.
            Self::Make => {
                let deps = parse(contents);
                (!deps.is_empty()).then_some(deps)
            }
            Self::MsvcJson => read_source_dependencies(contents),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source file that includes nothing is perfectly cacheable, and Qt's
    /// generated `qrc_*.cpp` -- a byte array with no `#include` in it -- is
    /// exactly that. Reading "opened nothing" as "could not be read" made every
    /// resource file in a Qt build uncacheable, and recompiled on every build.
    #[test]
    fn a_report_of_no_includes_is_an_answer_not_a_failure() {
        let empty = r#"{"Version":"1.2","Data":{"Source":"qrc_x.cpp","Includes":[]}}"#;
        assert_eq!(
            DepFileFormat::MsvcJson.read(empty),
            Some(Vec::new()),
            "an explicit empty list is a complete answer"
        );

        for unusable in ["", "not json", "{}", r#"{"Data":{}}"#] {
            assert_eq!(
                DepFileFormat::MsvcJson.read(unusable),
                None,
                "{unusable:?} cannot be read and must refuse the cache"
            );
        }
    }

    /// A non-string entry means the list cannot be trusted, and a list that is
    /// quietly short causes a stale hit rather than a miss.
    #[test]
    fn a_report_that_is_not_all_paths_is_refused_whole() {
        let mixed = r#"{"Data":{"Includes":["a.h", 42, "b.h"]}}"#;
        assert_eq!(DepFileFormat::MsvcJson.read(mixed), None);
    }

    /// Make cannot distinguish the two cases, so it keeps refusing an empty
    /// list -- moc's dependency files come through here.
    #[test]
    fn make_keeps_the_conservative_reading() {
        assert_eq!(DepFileFormat::Make.read("out.o:\n"), None);
        assert_eq!(
            DepFileFormat::Make.read("out.o: a.h\n"),
            Some(vec![PathBuf::from("a.h")])
        );
    }

    #[test]
    fn windows_drive_letter_is_not_the_rule_separator() {
        // The target's own colon comes first; taking it as the separator would
        // make "C" the target and feed the rest of the path back as a
        // dependency.
        let d = "C:\\build\\moc_x.cpp: D:/src/x.h\n";
        assert_eq!(parse(d), vec![PathBuf::from("D:/src/x.h")]);
    }

    #[test]
    fn escaped_spaces_survive() {
        // Straight from a real moc dep file. Splitting on raw whitespace turns
        // this into three paths that do not exist.
        let d = "out.cpp: C:/Program\\ Files\\ (x86)/Windows\\ Kits/10/ucrt/assert.h\n";
        assert_eq!(
            parse(d),
            vec![PathBuf::from(
                "C:/Program Files (x86)/Windows Kits/10/ucrt/assert.h"
            )]
        );
    }

    #[test]
    fn line_continuations_are_joined() {
        let d = "out.cpp: a.h \\\n  b.h \\\n  c.h\n";
        assert_eq!(
            parse(d),
            vec![
                PathBuf::from("a.h"),
                PathBuf::from("b.h"),
                PathBuf::from("c.h")
            ]
        );
    }

    #[test]
    fn crlf_continuations_are_joined() {
        // Dep files written on Windows routinely use CRLF; missing the \r
        // leaves it glued to the last path of every line.
        let d = "out.cpp: a.h \\\r\n  b.h\r\n";
        assert_eq!(parse(d), vec![PathBuf::from("a.h"), PathBuf::from("b.h")]);
    }

    #[test]
    fn backslash_separators_are_not_treated_as_escapes() {
        let d = "out.cpp: D:\\dev\\project\\header.h\n";
        assert_eq!(parse(d), vec![PathBuf::from("D:\\dev\\project\\header.h")]);
    }

    #[test]
    fn a_line_with_no_rule_is_ignored_rather_than_guessed_at() {
        assert!(parse("this line has no colon\n").is_empty());
        assert!(parse("").is_empty());
    }

    #[test]
    fn msvc_source_dependencies_json() {
        let j = r#"{"Version":"1.2","Data":{"Source":"D:\\src\\a.cpp",
            "ProvidedModule":"","Includes":["D:\\src\\a.h","C:\\Program Files\\x\\vector"]}}"#;
        assert_eq!(
            parse_source_dependencies(j),
            vec![
                PathBuf::from("D:\\src\\a.h"),
                PathBuf::from("C:\\Program Files\\x\\vector")
            ],
            "spaces need no escaping in JSON, unlike the Make form"
        );
    }

    #[test]
    fn malformed_source_dependencies_yield_nothing_to_cache_on() {
        // Same rule as the Make parser: an unreadable answer must reach the
        // caller as "cannot cache", never as "this compilation had no inputs".
        for j in [
            "",
            "not json",
            "{}",
            r#"{"Data":{}}"#,
            r#"{"Data":{"Includes":42}}"#,
        ] {
            assert!(parse_source_dependencies(j).is_empty(), "{j:?}");
        }
    }

    /// Check the parser against a dep file a real tool actually wrote.
    ///
    /// `SCCACHE_TEST_DEPFILE=<path>`. Every prerequisite must exist on disk,
    /// which is a stronger assertion than it looks: mishandling an escaped
    /// space or a drive letter does not produce an error, it produces confident
    /// paths to files that are not there. Those would then be digested as
    /// missing and quietly make every lookup a miss.
    #[test]
    fn real_dep_file_prerequisites_all_exist() {
        let Some(path) = std::env::var_os("SCCACHE_TEST_DEPFILE") else {
            eprintln!("skipping: set SCCACHE_TEST_DEPFILE to check a real dep file");
            return;
        };
        let contents = std::fs::read_to_string(&path).expect("read dep file");
        let deps = parse(&contents);
        assert!(!deps.is_empty(), "parsed no prerequisites from {path:?}");
        let missing: Vec<_> = deps.iter().filter(|d| !d.exists()).collect();
        assert!(
            missing.is_empty(),
            "{} of {} prerequisites do not exist, so the parse is wrong; first few: {:?}",
            missing.len(),
            deps.len(),
            missing.iter().take(5).collect::<Vec<_>>()
        );
        eprintln!("verified {} prerequisites from {path:?}", deps.len());
    }

    #[test]
    fn real_moc_dep_file_shape() {
        // Abbreviated from qtmoc --output-dep-file: backslash-separated target,
        // forward-slash prerequisites, escaped spaces, continuations.
        let d = "C:\\Temp\\moc_probe.cpp: D:/dev/proj/Amplifiers_global.h \\\r\n\
                 \x20 C:/Program\\ Files\\ (x86)/Windows\\ Kits/10/include/ucrt/assert.h \\\r\n\
                 \x20 D:/dev/proj/other.h\r\n";
        let got = parse(d);
        assert_eq!(got.len(), 3, "got {got:?}");
        assert_eq!(got[0], PathBuf::from("D:/dev/proj/Amplifiers_global.h"));
        assert_eq!(
            got[1],
            PathBuf::from("C:/Program Files (x86)/Windows Kits/10/include/ucrt/assert.h")
        );
        assert_eq!(got[2], PathBuf::from("D:/dev/proj/other.h"));
    }
}
