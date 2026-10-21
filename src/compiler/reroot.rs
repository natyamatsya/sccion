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
//! path starts with a checkout root, [`neutralize`] stores a marker in place of
//! the root, and [`reroot`] puts back the root of the checkout serving the hit,
//! in the separator style the text used. The result is what a compile in that
//! checkout would itself have printed.
//!
//! A checkout's roots are the basedirs that contain the compilation's working
//! directory ([`roots_for`]), and a marker names one by its rank among them,
//! innermost first -- never by its place in the basedir list. That list is the
//! same in every checkout when it is configured (`SCCACHE_BASEDIRS` naming
//! both trees, say), so a place in it names the *storing* checkout wherever
//! the entry is replayed. A basedir that does not contain the working
//! directory is another tree, not this one, and paths under it are stored as
//! they stand.
//!
//! A root has more than one spelling when a symlink leads to it. The working
//! directory comes resolved -- `/private/tmp/x/build` on macOS, for a checkout
//! the build reached as `/tmp/x` -- so the root found from it is spelled
//! resolved, while the build names its paths through the link. [`spellings_in`]
//! learns those other spellings from the paths a compilation's arguments name,
//! accepting one only when it resolves to the root itself. They are stripped
//! from the key like any basedir, so the two spellings key alike, and matched
//! here as the root they spell, the marker noting which spelling it replaced.
//!
//! Matching folds ASCII case only where the basedir's volume does, and only
//! ASCII, for the reason given at `util::starts_with_on`. A root that appears
//! with its spaces escaped (`\ ` in a Make-style dependency file) is not
//! matched, so such a checkout keeps absolute paths in its dependency output
//! and shares it no better than before.

use std::borrow::Cow;
use std::path::Path;

use crate::errors::*;

/// Opens a marker: `\x1fsccion-root<rank>:<style>[a]\x1f`, the `a` when the
/// text spelled the root through a symlink (see [`Root::aliases`]). The unit separator
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

/// `text` with every one of `roots` that starts a path replaced by a marker.
pub fn neutralize<'a>(text: &'a [u8], roots: &[Root]) -> Cow<'a, [u8]> {
    if roots.is_empty() || text.is_empty() {
        return Cow::Borrowed(text);
    }
    let mut out: Option<Vec<u8>> = None;
    let mut copied = 0;
    let mut i = 0;
    while i < text.len() {
        let at_boundary = i == 0 || is_boundary(text[i - 1]);
        let found = at_boundary
            .then(|| {
                // Innermost first, so a root nested in another is matched as
                // itself.
                roots.iter().enumerate().find_map(|(rank, root)| {
                    let own = std::iter::once((&root.basedir, root.folds, false));
                    let aliases = root.aliases.iter().map(|(a, folds)| (a, *folds, true));
                    own.chain(aliases).find_map(|(spelling, folds, alias)| {
                        match_root(&text[i..], spelling, folds).map(|(n, s)| (rank, n, s, alias))
                    })
                })
            })
            .flatten();
        match found {
            Some((idx, len, style, alias)) => {
                let buf = out.get_or_insert_with(|| Vec::with_capacity(text.len()));
                buf.extend_from_slice(&text[copied..i]);
                buf.extend_from_slice(OPEN);
                buf.extend_from_slice(idx.to_string().as_bytes());
                buf.push(b':');
                buf.push(style.code());
                if alias {
                    buf.push(b'a');
                }
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

/// `text` with every marker replaced by the root of that rank in `roots`,
/// spelled in the style the marker recorded and followed by one separator. A
/// root the stored text reached through a symlink is put back through this
/// compilation's own, where it has one.
///
/// Fails on a marker it cannot resolve -- a rank with no root, which means
/// fewer basedirs contain this compilation than contained the one that stored
/// the entry -- rather than replay a path that names nothing. The caller
/// treats that as a miss.
pub fn reroot<'a>(text: &'a [u8], roots: &[Root]) -> Result<Cow<'a, [u8]>> {
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
        let (idx, style, alias) = parse_marker(&body[..end])?;
        let root = roots.get(idx).with_context(|| {
            format!(
                "the entry names checkout root {idx}, and this compilation has {}",
                roots.len()
            )
        })?;
        let root = match root.aliases.first() {
            Some((spelling, _)) if alias => spelling.strip_suffix(b"/").unwrap_or(spelling),
            _ => &root.spelled[..],
        };
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

fn parse_marker(body: &[u8]) -> Result<(usize, Style, bool)> {
    let (idx, style, alias) = match body {
        [digits @ .., b':', code, b'a'] => (digits, *code, true),
        [digits @ .., b':', code] => (digits, *code, false),
        _ => bail!("malformed checkout-root marker"),
    };
    let idx = std::str::from_utf8(idx)
        .ok()
        .and_then(|s| s.parse().ok())
        .context("malformed checkout-root rank")?;
    let style = Style::from_code(style).context("malformed checkout-root style")?;
    Ok((idx, style, alias))
}

/// A checkout root of one compilation: a basedir that contains its working
/// directory.
#[derive(Clone, Debug)]
pub struct Root {
    /// As normalized: `/` separators and a trailing `/`.
    basedir: Vec<u8>,
    folds: bool,
    /// As the working directory spells it, with `/` separators and no trailing
    /// one, so a rerooted path has the case a compile here would print.
    spelled: Vec<u8>,
    /// The other basedirs that resolve to this one -- its spellings through a
    /// symlink, normalized like `basedir` -- and whether each folds case.
    aliases: Vec<(Vec<u8>, bool)>,
}

/// The basedirs that contain `cwd`, innermost first: what [`neutralize`] takes
/// out of a compilation's output, and what [`reroot`] puts back, by rank.
///
/// Each contains `cwd`, so they nest, and innermost is longest. A compilation
/// whose working directory no basedir contains has no roots, and its output is
/// stored as it stands.
pub fn roots_for(basedirs: &[Vec<u8>], cwd: &Path) -> Vec<Root> {
    let mut cwd: Vec<u8> = cwd
        .to_string_lossy()
        .bytes()
        .map(|b| if b == b'\\' { b'/' } else { b })
        .collect();
    if !cwd.ends_with(b"/") {
        cwd.push(b'/');
    }
    let mut roots: Vec<Root> = basedirs
        .iter()
        .filter_map(|b| {
            let folds = crate::volume::basedir_folds_case(b);
            let contains_cwd = cwd.len() >= b.len()
                && cwd[..b.len()]
                    .iter()
                    .zip(b)
                    .all(|(c, b)| c == b || (folds && c.eq_ignore_ascii_case(b)));
            contains_cwd.then(|| Root {
                basedir: b.clone(),
                folds,
                spelled: cwd[..b.len()]
                    .strip_suffix(b"/")
                    .unwrap_or(&cwd[..b.len()])
                    .to_vec(),
                aliases: Vec::new(),
            })
        })
        .collect();
    roots.sort_by_key(|r| std::cmp::Reverse(r.basedir.len()));
    roots.dedup_by(|a, b| a.basedir.len() == b.basedir.len());

    // A basedir that does not contain the working directory by its text may
    // still be one of these roots, spelled through a symlink.
    let resolved: Vec<Option<Vec<u8>>> = roots.iter().map(|r| resolved_dir(&r.basedir)).collect();
    if resolved.iter().any(Option::is_some) {
        for b in basedirs {
            if roots.iter().any(|r| &r.basedir == b) {
                continue;
            }
            let Some(target) = resolved_dir(b) else {
                continue;
            };
            if let Some(i) = resolved.iter().position(|r| r.as_ref() == Some(&target)) {
                let alias = (b.clone(), crate::volume::basedir_folds_case(b));
                if !roots[i].aliases.contains(&alias) {
                    roots[i].aliases.push(alias);
                }
            }
        }
    }
    roots
}

/// `dir` with every symlink resolved, with a trailing `/`, or `None` when it
/// does not exist. Unix only: Windows resolves to `\\?\` paths, and its
/// junctions are not what puts a checkout behind a second name there.
#[cfg(unix)]
fn resolved_dir(dir: &[u8]) -> Option<Vec<u8>> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let resolved = std::fs::canonicalize(std::ffi::OsStr::from_bytes(dir)).ok()?;
    let mut bytes = resolved.into_os_string().into_vec();
    if !bytes.ends_with(b"/") {
        bytes.push(b'/');
    }
    Some(bytes)
}

#[cfg(not(unix))]
fn resolved_dir(_dir: &[u8]) -> Option<Vec<u8>> {
    None
}

/// The spellings of this compilation's checkout roots that `paths` use and
/// `basedirs` do not have: each a directory that resolves to a root, named the
/// way the build named it, with a trailing `/`.
///
/// `paths` are the compilation's arguments; a path is an argument that starts
/// with `/`, or what follows the first `/` of an option (`-I/x`,
/// `--sysroot=/x`). For each that is under no basedir as written, the nearest
/// directory of it that exists is resolved. When that lands under a root, and
/// the part below the root reads the same in both spellings, what comes before
/// it is the root's other spelling -- accepted only if it resolves to the root
/// itself, so a link to somewhere inside the checkout (`/opt/vendor` to
/// `<root>/vendor`) cannot pass `/opt/` off as the root.
#[cfg(unix)]
pub fn spellings_in<'a>(
    basedirs: &[Vec<u8>],
    cwd: &Path,
    paths: impl IntoIterator<Item = &'a std::ffi::OsStr>,
) -> Vec<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;

    let resolved_roots: Vec<Vec<u8>> = roots_for(basedirs, cwd)
        .iter()
        .filter_map(|r| resolved_dir(&r.basedir))
        .collect();
    let mut found: Vec<Vec<u8>> = Vec::new();
    if resolved_roots.is_empty() {
        return found;
    }
    for arg in paths {
        let bytes = arg.as_bytes();
        let start = match bytes {
            [b'/', ..] => 0,
            [b'-', ..] => match memchr::memchr(b'/', bytes) {
                Some(i) => i,
                None => continue,
            },
            _ => continue,
        };
        let path = &bytes[start..];
        let known = |b: &Vec<u8>| crate::util::strip_basedirs_in_arg(path, std::slice::from_ref(b));
        if basedirs
            .iter()
            .chain(&found)
            .any(|b| matches!(known(b), Cow::Owned(_)))
        {
            continue;
        }
        // The nearest directory of it that exists: an output need not yet.
        let mut dir = Path::new(std::ffi::OsStr::from_bytes(path));
        let resolved = loop {
            if let Some(r) = resolved_dir(dir.as_os_str().as_bytes()) {
                break r;
            }
            match dir.parent() {
                Some(parent) => dir = parent,
                None => break Vec::new(),
            }
        };
        let mut written = dir.as_os_str().as_bytes().to_vec();
        if !written.ends_with(b"/") {
            written.push(b'/');
        }
        for root in &resolved_roots {
            let Some(below) = resolved.strip_prefix(root.as_slice()) else {
                continue;
            };
            let Some(spelling) = written.strip_suffix(below) else {
                continue;
            };
            if spelling.ends_with(b"/")
                && spelling != root.as_slice()
                && !found.iter().any(|f| f == spelling)
                && resolved_dir(spelling).as_ref() == Some(root)
            {
                found.push(spelling.to_vec());
            }
        }
    }
    found
}

#[cfg(not(unix))]
pub fn spellings_in<'a>(
    _basedirs: &[Vec<u8>],
    _cwd: &Path,
    _paths: impl IntoIterator<Item = &'a std::ffi::OsStr>,
) -> Vec<Vec<u8>> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The roots of a compilation in `cwd`, `basedirs` normalized.
    fn roots(basedirs: &[&str], cwd: &str) -> Vec<Root> {
        let basedirs: Vec<Vec<u8>> = basedirs.iter().map(|b| b.as_bytes().to_vec()).collect();
        roots_for(&basedirs, Path::new(cwd))
    }

    fn here_and_there() -> (Vec<Root>, Vec<Root>) {
        // Stored in checkout `a`, served in checkout `b`.
        (
            roots(&["/w/a/"], "/w/a/build"),
            roots(&["/w/b/"], "/w/b/build"),
        )
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
        let stored_in = roots(&["d:/w/a/"], "d:/w/a/build");
        let served_in = roots(&["D:/w/b/"], "D:/w/b/build");
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
        let stored_in = roots(&["/w/a/"], "/w/a");
        assert_eq!(
            neutralize(b"\\w/a\\x.h", &stored_in).as_ref(),
            b"\\w/a\\x.h".as_ref()
        );
    }

    #[test]
    fn the_innermost_root_wins() {
        let stored_in = roots(&["/w/", "/w/a/"], "/w/a/build");
        let served_in = roots(&["/v/b/", "/v/"], "/v/b/build");
        let neutral = neutralize(b"/w/a/x.h /w/c/y.h", &stored_in);
        assert_eq!(
            reroot(&neutral, &served_in).unwrap().as_ref(),
            b"/v/b/x.h /v/c/y.h".as_ref()
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
        let stored_in = roots(&["/w/", "/w/a/"], "/w/a/build");
        let neutral = neutralize(b"/w/c/x.h", &stored_in);
        let err = reroot(&neutral, &roots(&["/v/b/"], "/v/b/build"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("checkout root 1"), "{err}");
    }

    #[test]
    fn a_path_a_longer_root_pushes_past_max_path_is_resolved() {
        // Measured: cl.exe printed this 259-character include in the checkout
        // that stored it, and two characters more of root made it 261 here,
        // which ninja refuses ("path too long").
        let stored_in = roots(&["d:/dev/ac/"], "d:/dev/ac/build");
        let served_in = roots(
            &["D:/dev/ac-longer-checkout-name/"],
            "D:/dev/ac-longer-checkout-name/build",
        );
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
        let stored_in = roots(&["d:/dev/ac/"], "d:/dev/ac/build");
        let served_in = roots(&["D:/dev/ac-b/"], "D:/dev/ac-b/build");
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
        let roots = roots(&["d:/dev/proj/"], "D:\\Dev\\Proj\\build\\rel");
        #[cfg(windows)]
        assert_eq!(roots[0].spelled, b"D:/Dev/Proj".to_vec());
        #[cfg(not(windows))]
        let _ = roots;
    }

    /// Only the basedirs containing the working directory are its roots,
    /// innermost first, whatever order they were declared in.
    #[test]
    fn roots_are_the_basedirs_containing_the_working_directory() {
        let roots = roots(&["/w/", "/elsewhere/", "/w/a/", "/w/ab/"], "/w/a/build");
        let spelled: Vec<_> = roots.iter().map(|r| r.spelled.clone()).collect();
        assert_eq!(spelled, [b"/w/a".to_vec(), b"/w".to_vec()]);
    }

    /// `SCCACHE_BASEDIRS` naming both checkouts is the same list in each, so a
    /// root's place in it names the checkout that stored the entry. Its rank
    /// among the roots containing the compilation names the one serving it.
    #[test]
    fn a_configured_list_of_both_checkouts_reroots_to_the_serving_one() {
        let basedirs = ["/build/dir1/", "/build/dir2/"];
        let stored_in = roots(&basedirs, "/build/dir1");
        let served_in = roots(&basedirs, "/build/dir2");
        let neutral = neutralize(b"x.o: /build/dir1/x.c /build/dir1/x.h\n", &stored_in);
        assert_eq!(
            reroot(&neutral, &served_in).unwrap().as_ref(),
            b"x.o: /build/dir2/x.c /build/dir2/x.h\n".as_ref()
        );
    }

    /// A checkout in `real/<name>`, reached through `link` -> `real`, with
    /// `inc/` and `build/` in it: the root spelled resolved, as a working
    /// directory reports it, and through the link, as a build names it.
    #[cfg(unix)]
    struct Linked {
        resolved: Vec<u8>,
        linked: Vec<u8>,
        cwd: std::path::PathBuf,
    }

    #[cfg(unix)]
    fn linked(dir: &tempfile::TempDir, name: &str) -> Linked {
        use std::os::unix::ffi::OsStrExt;
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("real").join(name);
        std::fs::create_dir_all(root.join("inc")).unwrap();
        std::fs::create_dir_all(root.join("build")).unwrap();
        if !base.join("link").exists() {
            std::os::unix::fs::symlink(base.join("real"), base.join("link")).unwrap();
        }
        let slash = |p: &Path| [p.as_os_str().as_bytes(), b"/"].concat();
        Linked {
            resolved: slash(&root),
            linked: slash(&base.join("link").join(name)),
            cwd: root.join("build"),
        }
    }

    #[cfg(unix)]
    fn os(text: &[u8]) -> std::ffi::OsString {
        use std::os::unix::ffi::OsStrExt;
        std::ffi::OsStr::from_bytes(text).to_owned()
    }

    /// A build that names its headers through a symlink to the checkout names
    /// them under no basedir: the root is found from the resolved working
    /// directory. Its spelling through the link is learned from those paths.
    #[cfg(unix)]
    #[test]
    fn a_root_reached_through_a_symlink_is_learned_from_the_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let a = linked(&dir, "a");
        let include = os(&[b"-I".as_slice(), &a.linked, b"inc"].concat());
        let output = os(&[a.linked.as_slice(), b"build/not-yet.o"].concat());
        let basedirs = vec![a.resolved.clone()];

        let spellings = spellings_in(
            &basedirs,
            &a.cwd,
            [
                include.as_os_str(),
                output.as_os_str(),
                std::ffi::OsStr::new("-c"),
            ],
        );
        assert_eq!(spellings, std::slice::from_ref(&a.linked));
    }

    /// A link from outside into the checkout is not the checkout: `/opt/vendor`
    /// -> `<root>/vendor` must not make `/opt/` a spelling of the root, or every
    /// path under `/opt/` would key as if it were in the checkout.
    #[cfg(unix)]
    #[test]
    fn a_link_into_the_checkout_is_not_a_spelling_of_its_root() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let a = linked(&dir, "a");
        let root = Path::new(std::ffi::OsStr::from_bytes(&a.resolved));
        std::fs::create_dir_all(root.join("vendor").join("inc")).unwrap();
        let opt = dir.path().canonicalize().unwrap().join("opt");
        std::fs::create_dir_all(&opt).unwrap();
        std::os::unix::fs::symlink(root.join("vendor"), opt.join("vendor")).unwrap();
        let include = os(&[
            b"-I".as_slice(),
            opt.join("vendor/inc").as_os_str().as_bytes(),
        ]
        .concat());

        let spellings = spellings_in(
            std::slice::from_ref(&a.resolved),
            &a.cwd,
            [include.as_os_str()],
        );
        assert!(spellings.is_empty(), "{spellings:?}");
    }

    /// Output that spells the root through the link is put back through the
    /// serving checkout's own link, and resolved where it has none.
    #[cfg(unix)]
    #[test]
    fn a_root_spelled_through_a_symlink_is_put_back_the_same_way() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (linked(&dir, "a"), linked(&dir, "b"));
        let stored_in = roots_for(&[a.resolved.clone(), a.linked.clone()], &a.cwd);
        let text = [
            b"x.o: ".as_slice(),
            &a.linked,
            b"inc/x.h ",
            &a.resolved,
            b"inc/y.h\n",
        ]
        .concat();
        let neutral = neutralize(&text, &stored_in);
        assert!(
            !neutral
                .windows(a.linked.len())
                .any(|w| w == a.linked.as_slice())
        );

        let through_link = roots_for(&[b.resolved.clone(), b.linked.clone()], &b.cwd);
        assert_eq!(
            reroot(&neutral, &through_link).unwrap().as_ref(),
            [
                b"x.o: ".as_slice(),
                &b.linked,
                b"inc/x.h ",
                &b.resolved,
                b"inc/y.h\n"
            ]
            .concat()
        );
        let resolved_only = roots_for(std::slice::from_ref(&b.resolved), &b.cwd);
        assert_eq!(
            reroot(&neutral, &resolved_only).unwrap().as_ref(),
            [
                b"x.o: ".as_slice(),
                &b.resolved,
                b"inc/x.h ",
                &b.resolved,
                b"inc/y.h\n"
            ]
            .concat()
        );
    }

    /// A basedir that is not this checkout's is another tree, and what names
    /// it is stored as it stands.
    #[test]
    fn a_basedir_not_containing_the_compilation_is_left_alone() {
        let stored_in = roots(&["/build/dir1/", "/build/dir2/"], "/build/dir1");
        assert!(matches!(
            neutralize(b"/build/dir2/x.h", &stored_in),
            Cow::Borrowed(_)
        ));
    }
}
