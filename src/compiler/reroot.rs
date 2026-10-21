// SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
// SPDX-License-Identifier: Apache-2.0

//! Checkout roots in what a cache entry replays, taken out on the way in and
//! put back on the way out.
//!
//! A cache key with its basedirs stripped is shared by every checkout, and the
//! object it names is too -- the build has made the object path-independent.
//! What the compiler *said* is not. MSVC's `/showIncludes` lines, a Make-style
//! dependency file, moc's dependency file: each names every header by the
//! absolute path the compiler found it at, in the checkout that filled the
//! entry. Replayed verbatim into another checkout, the build system records the
//! *first* checkout's headers as the dependencies, and an edit to a header here
//! rebuilds nothing. Measured: 1,853 objects of a second checkout depended on
//! the first one's headers rather than their own. Nothing fails; the build is
//! just stale. With the paths put back, a declaration added to a header there
//! rebuilt exactly the 16 objects that include it, and reran its moc.
//!
//! No compiler option changes this. GCC's and Clang's `-f*-prefix-map` family
//! and MSVC's `/d1trimfile` rewrite `__FILE__`, debug information and coverage
//! data, not dependency output; `/sourceDependencies` writes the same absolute
//! paths as JSON. So the rewriting happens here, as ccache does it: where a
//! path starts with a basedir, [`neutralize`] stores a marker in place of the
//! root, and [`reroot`] puts back the root of the checkout serving the hit, in
//! the separator style the text used. The result is what a compile in that
//! checkout would itself have printed.
//!
//! Matching folds ASCII case only where the basedir's volume does, and only
//! ASCII, for the reason given at `util::starts_with_on`. A root that appears
//! with its spaces escaped (`\ ` in a Make-style dependency file) is not
//! matched, so such a checkout keeps absolute paths in its dependency output
//! and shares it no better than before.

use std::borrow::Cow;
use std::path::Path;

use crate::errors::*;

/// Opens a marker: `\x1fsccion-root<index>:<style>\x1f`. The unit separator
/// does not occur in compiler output or in paths, so a marker cannot be
/// mistaken for text, nor text for a marker.
const OPEN: &[u8] = b"\x1fsccion-root";
const CLOSE: u8 = 0x1f;

/// Cache-entry members whose contents name dependencies by path: a Make-style
/// dependency file, and moc's dependency and JSON outputs.
pub const DEPENDENCY_OUTPUTS: &[&str] = &["d", "moc_dep", "moc_json"];

pub fn is_dependency_output(key: &str) -> bool {
    DEPENDENCY_OUTPUTS.contains(&key)
}

/// How the separators of a matched root were spelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Style {
    /// `D:/dev/x/`, or any Unix path.
    Slash,
    /// `D:\dev\x\`, as `/showIncludes` prints it.
    Backslash,
    /// `D:\\dev\\x\\`, as JSON and C string literals escape it.
    Doubled,
}

impl Style {
    fn code(self) -> u8 {
        match self {
            Style::Slash => b'/',
            Style::Backslash => b'\\',
            Style::Doubled => b'B',
        }
    }

    fn from_code(c: u8) -> Option<Style> {
        match c {
            b'/' => Some(Style::Slash),
            b'\\' => Some(Style::Backslash),
            b'B' => Some(Style::Doubled),
            _ => None,
        }
    }

    fn separator(self) -> &'static [u8] {
        match self {
            Style::Slash => b"/",
            Style::Backslash => b"\\",
            Style::Doubled => b"\\\\",
        }
    }
}

/// Whether a path can start right after `b`: the text before a path in
/// compiler output, a dependency file or JSON.
fn is_boundary(b: u8) -> bool {
    b.is_ascii_whitespace() || matches!(b, b'"' | b'\'' | b'<' | b'(' | b'=')
}

/// The length of `basedir` spelled at the start of `text`, and how its
/// separators were spelled, if it is there. `basedir` is normalized, with `/`
/// separators and a trailing `/`, so the root's own trailing separator is part
/// of the match and the path cannot continue a longer directory name.
fn match_root(text: &[u8], basedir: &[u8], folds: bool) -> Option<(usize, Style)> {
    let mut style = None;
    let mut i = 0;
    for &b in basedir {
        if b == b'/' {
            let found = match text.get(i..)? {
                [b'/', ..] => Style::Slash,
                [b'\\', b'\\', ..] => Style::Doubled,
                [b'\\', ..] => Style::Backslash,
                _ => return None,
            };
            match style {
                None => style = Some(found),
                Some(s) if s != found => return None,
                Some(_) => {}
            }
            i += found.separator().len();
        } else {
            let t = *text.get(i)?;
            if !(t == b || (folds && t.eq_ignore_ascii_case(&b))) {
                return None;
            }
            i += 1;
        }
    }
    style.map(|s| (i, s))
}

/// `text` with every basedir that starts a path replaced by a marker.
pub fn neutralize<'a>(text: &'a [u8], basedirs: &[Vec<u8>]) -> Cow<'a, [u8]> {
    if basedirs.is_empty() || text.is_empty() {
        return Cow::Borrowed(text);
    }
    // Longest first, so a basedir nested in another is matched as itself.
    let mut order: Vec<usize> = (0..basedirs.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(basedirs[i].len()));
    let folds: Vec<bool> = basedirs
        .iter()
        .map(|b| crate::volume::basedir_folds_case(b))
        .collect();

    let mut out: Option<Vec<u8>> = None;
    let mut copied = 0;
    let mut i = 0;
    while i < text.len() {
        let at_boundary = i == 0 || is_boundary(text[i - 1]);
        let found = at_boundary
            .then(|| {
                order.iter().find_map(|&idx| {
                    match_root(&text[i..], &basedirs[idx], folds[idx]).map(|(n, s)| (idx, n, s))
                })
            })
            .flatten();
        match found {
            Some((idx, len, style)) => {
                let buf = out.get_or_insert_with(|| Vec::with_capacity(text.len()));
                buf.extend_from_slice(&text[copied..i]);
                buf.extend_from_slice(OPEN);
                buf.extend_from_slice(idx.to_string().as_bytes());
                buf.push(b':');
                buf.push(style.code());
                buf.push(CLOSE);
                i += len;
                copied = i;
            }
            None => i += 1,
        }
    }
    match out {
        Some(mut buf) => {
            buf.extend_from_slice(&text[copied..]);
            Cow::Owned(buf)
        }
        None => Cow::Borrowed(text),
    }
}

/// `text` with every marker replaced by the matching root in `roots`, spelled
/// in the style the marker recorded and followed by one separator.
///
/// Fails on a marker it cannot resolve -- an index with no root, which means
/// this checkout declares fewer basedirs than the one that stored the entry --
/// rather than replay a path that names nothing. The caller treats that as a
/// miss.
pub fn reroot<'a>(text: &'a [u8], roots: &[Vec<u8>]) -> Result<Cow<'a, [u8]>> {
    let Some(first) = memchr::memmem::find(text, OPEN) else {
        return Ok(Cow::Borrowed(text));
    };
    let mut out = Vec::with_capacity(text.len() + 64);
    out.extend_from_slice(&text[..first]);
    let mut rest = &text[first..];
    while let Some(pos) = memchr::memmem::find(rest, OPEN) {
        out.extend_from_slice(&rest[..pos]);
        let body = &rest[pos + OPEN.len()..];
        let end = memchr::memchr(CLOSE, body).context("unterminated checkout-root marker")?;
        let (idx, style) = parse_marker(&body[..end])?;
        let root = roots.get(idx).with_context(|| {
            format!(
                "the entry names checkout root {idx}, and this checkout has {}",
                roots.len()
            )
        })?;
        let path_start = out.len();
        for &b in root {
            if b == b'/' {
                out.extend_from_slice(style.separator());
            } else {
                out.push(b);
            }
        }
        out.extend_from_slice(style.separator());
        rest = &body[end + 1..];

        // The rest of the path, up to whatever ends a path in this text.
        let tail = rest
            .iter()
            .position(|&b| b.is_ascii_whitespace() || matches!(b, b'"' | b'\''))
            .unwrap_or(rest.len());
        out.extend_from_slice(&rest[..tail]);
        rest = &rest[tail..];
        if is_drive_path(root) && out.len() - path_start > MAX_PATH_CHARS {
            let collapsed = collapse_dots(&out[path_start..], style);
            out.truncate(path_start);
            out.extend_from_slice(&collapsed);
        }
    }
    out.extend_from_slice(rest);
    Ok(Cow::Owned(out))
}

/// The longest path Windows tools accept without long-path support: `MAX_PATH`
/// is 260 including the terminator. ninja's `/showIncludes` parser refuses a
/// longer one outright ("path too long") and fails the edge.
const MAX_PATH_CHARS: usize = 259;

fn is_drive_path(root: &[u8]) -> bool {
    matches!(root, [letter, b':', ..] if letter.is_ascii_alphabetic())
}

/// `path` with `.` and `..` segments resolved by text, separators spelled in
/// `style`.
///
/// cl.exe prints an include the way it was reached -- `gen\dir\../../src/x.h`
/// for a moc file including its header -- unless that would exceed `MAX_PATH`,
/// when it prints the path resolved. Its cutoff is measured against the
/// checkout that compiled, so a longer root here can push a line it printed
/// verbatim past the limit; resolving it then gives what cl would have printed
/// in this checkout. Windows resolves `..` by text too, so this names the same
/// file.
fn collapse_dots(path: &[u8], style: Style) -> Vec<u8> {
    let sep = style.separator();
    let mut segments: Vec<&[u8]> = Vec::new();
    let mut rest = path;
    while !rest.is_empty() {
        let (len, skip) = match rest.iter().position(|&b| b == b'/' || b == b'\\') {
            Some(i) => {
                let doubled = rest.get(i..i + 2) == Some(b"\\\\".as_slice());
                (i, if doubled { 2 } else { 1 })
            }
            None => (rest.len(), 0),
        };
        match &rest[..len] {
            b"" | b"." => {}
            // Never above the drive: `segments[0]` is `X:`.
            b".." if segments.len() > 1 => {
                segments.pop();
            }
            b".." => {}
            seg => segments.push(seg),
        }
        rest = &rest[len + skip..];
    }
    segments.join(sep)
}

fn parse_marker(body: &[u8]) -> Result<(usize, Style)> {
    let (idx, style) = match body {
        [digits @ .., b':', code] => (digits, *code),
        _ => bail!("malformed checkout-root marker"),
    };
    let idx = std::str::from_utf8(idx)
        .ok()
        .and_then(|s| s.parse().ok())
        .context("malformed checkout-root index")?;
    let style = Style::from_code(style).context("malformed checkout-root style")?;
    Ok((idx, style))
}

/// The root each basedir stands for in this checkout, with `/` separators and
/// no trailing one: spelled as the working directory spells it where the
/// basedir contains it, so a rerooted path has the case a compile here would
/// print, and as the normalized basedir otherwise.
pub fn roots_for(basedirs: &[Vec<u8>], cwd: &Path) -> Vec<Vec<u8>> {
    let mut cwd: Vec<u8> = cwd
        .to_string_lossy()
        .bytes()
        .map(|b| if b == b'\\' { b'/' } else { b })
        .collect();
    if !cwd.ends_with(b"/") {
        cwd.push(b'/');
    }
    basedirs
        .iter()
        .map(|b| {
            let folds = crate::volume::basedir_folds_case(b);
            let contains_cwd = cwd.len() >= b.len()
                && cwd[..b.len()]
                    .iter()
                    .zip(b)
                    .all(|(c, b)| c == b || (folds && c.eq_ignore_ascii_case(b)));
            let spelled = if contains_cwd {
                &cwd[..b.len()]
            } else {
                &b[..]
            };
            spelled.strip_suffix(b"/").unwrap_or(spelled).to_vec()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn here_and_there() -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
        // Stored in checkout `a`, served in checkout `b`. Lowercase, as basedirs
        // are normalized; the text below uses mixed case where the volume folds.
        (vec![b"/w/a/".to_vec()], vec![b"/w/b".to_vec()])
    }

    fn round_trip(text: &str) -> String {
        let (stored_in, served_in) = here_and_there();
        let neutral = neutralize(text.as_bytes(), &stored_in);
        assert!(
            !String::from_utf8_lossy(&neutral).contains("/w/a/"),
            "{neutral:?}"
        );
        String::from_utf8(reroot(&neutral, &served_in).unwrap().into_owned()).unwrap()
    }

    #[test]
    fn a_make_dependency_file_names_the_serving_checkout() {
        assert_eq!(
            round_trip("/w/a/build/x.o: /w/a/src/x.c \\\n  /w/a/src/x.h /usr/include/stdio.h\n"),
            "/w/b/build/x.o: /w/b/src/x.c \\\n  /w/b/src/x.h /usr/include/stdio.h\n"
        );
    }

    #[test]
    fn a_root_that_only_prefixes_a_longer_name_is_left_alone() {
        assert_eq!(round_trip("/w/a/x.h /w/ab/y.h"), "/w/b/x.h /w/ab/y.h");
    }

    #[test]
    fn a_root_inside_a_word_is_left_alone() {
        let (stored_in, _) = here_and_there();
        assert!(matches!(
            neutralize(b"see:/w/a/x.h", &stored_in),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn each_separator_style_is_kept() {
        let stored_in = vec![b"d:/w/a/".to_vec()];
        let served_in = vec![b"D:/w/b".to_vec()];
        let text = "Note: including file:  D:\\w\\a\\src\\x.h\n\
                    {\"Includes\":[\"d:\\\\w\\\\a\\\\src\\\\y.h\"]}\n\
                    D:/w/a/build/moc_z.cpp: D:/w/a/src/z.h\n";
        let neutral = neutralize(text.as_bytes(), &stored_in);
        let back = String::from_utf8(reroot(&neutral, &served_in).unwrap().into_owned()).unwrap();
        #[cfg(windows)]
        assert_eq!(
            back,
            "Note: including file:  D:\\w\\b\\src\\x.h\n\
             {\"Includes\":[\"D:\\\\w\\\\b\\\\src\\\\y.h\"]}\n\
             D:/w/b/build/moc_z.cpp: D:/w/b/src/z.h\n"
        );
        // Off Windows `d:/w/a/` does not fold case, so only the lowercase JSON
        // path matches -- which still shows the doubled style surviving.
        #[cfg(not(windows))]
        assert!(back.contains("d:\\\\w\\\\a") || back.contains("D:\\\\w\\\\b"));
    }

    #[test]
    fn mixed_separators_within_one_root_are_not_a_root() {
        let stored_in = vec![b"/w/a/".to_vec()];
        assert_eq!(
            neutralize(b"\\w/a\\x.h", &stored_in).as_ref(),
            b"\\w/a\\x.h".as_ref()
        );
    }

    #[test]
    fn the_longest_basedir_wins() {
        let stored_in = vec![b"/w/".to_vec(), b"/w/a/".to_vec()];
        let served_in = vec![b"/v".to_vec(), b"/w/b".to_vec()];
        let neutral = neutralize(b"/w/a/x.h /w/c/y.h", &stored_in);
        assert_eq!(
            reroot(&neutral, &served_in).unwrap().as_ref(),
            b"/w/b/x.h /v/c/y.h".as_ref()
        );
    }

    #[test]
    fn text_without_a_root_is_not_copied() {
        let (stored_in, served_in) = here_and_there();
        assert!(matches!(
            neutralize(b"warning: none here", &stored_in),
            Cow::Borrowed(_)
        ));
        assert!(matches!(
            reroot(b"warning: none here", &served_in).unwrap(),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn a_marker_this_checkout_cannot_resolve_fails() {
        let stored_in = vec![b"/x/".to_vec(), b"/w/a/".to_vec()];
        let neutral = neutralize(b"/w/a/x.h", &stored_in);
        let err = reroot(&neutral, &[b"/w/b".to_vec()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("checkout root 1"), "{err}");
    }

    #[test]
    fn a_path_a_longer_root_pushes_past_max_path_is_resolved() {
        // Measured: cl.exe printed this 259-character include in the checkout
        // that stored it, and two characters more of root made it 261 here,
        // which ninja refuses ("path too long").
        let stored_in = vec![b"d:/dev/ac/".to_vec()];
        let served_in = vec![b"D:/dev/ac-longer-checkout-name".to_vec()];
        let deep = "build\\rel\\gen\\TLDSB7EF3Q\\../../../../";
        let filler = "x".repeat(200);
        let text = format!("Note: including file:  D:\\dev\\ac\\{deep}src/{filler}/w.h\r\n");
        let neutral = neutralize(text.as_bytes(), &stored_in);
        let back = String::from_utf8(reroot(&neutral, &served_in).unwrap().into_owned()).unwrap();
        #[cfg(windows)]
        assert_eq!(
            back,
            format!(
                "Note: including file:  D:\\dev\\ac-longer-checkout-name\\src\\{filler}\\w.h\r\n"
            )
        );
        #[cfg(not(windows))]
        let _ = back;
    }

    #[test]
    fn a_path_that_fits_keeps_the_spelling_cl_gave_it() {
        let stored_in = vec![b"d:/dev/ac/".to_vec()];
        let served_in = vec![b"D:/dev/ac-b".to_vec()];
        let text = "Note: including file:  D:\\dev\\ac\\gen\\TL\\../../src/w.h\n";
        let neutral = neutralize(text.as_bytes(), &stored_in);
        let back = String::from_utf8(reroot(&neutral, &served_in).unwrap().into_owned()).unwrap();
        #[cfg(windows)]
        assert_eq!(
            back,
            "Note: including file:  D:\\dev\\ac-b\\gen\\TL\\../../src/w.h\n"
        );
        #[cfg(not(windows))]
        let _ = back;
    }

    #[test]
    fn dots_collapse_without_climbing_above_the_drive() {
        assert_eq!(
            collapse_dots(b"D:\\a\\b\\..\\..\\..\\c/./d.h", Style::Backslash),
            b"D:\\c\\d.h".to_vec()
        );
        assert_eq!(
            collapse_dots(b"D:\\\\a\\\\b\\\\..\\\\c.h", Style::Doubled),
            b"D:\\\\a\\\\c.h".to_vec()
        );
    }

    #[test]
    fn roots_are_spelled_the_way_the_working_directory_is() {
        let basedirs = vec![b"d:/dev/proj/".to_vec(), b"/elsewhere/".to_vec()];
        let roots = roots_for(&basedirs, Path::new("D:\\Dev\\Proj\\build\\rel"));
        #[cfg(windows)]
        assert_eq!(roots[0], b"D:/Dev/Proj".to_vec());
        assert_eq!(roots[1], b"/elsewhere".to_vec());
    }
}
