//! The local-file targets a preview link can name: a `[label](docs/a.md)` markdown
//! link with no scheme, and a backticked path such as `src/a/b.ts:21-40`.
//!
//! Pure string parsing — nothing here touches the disk, and nothing here decides
//! that a file exists. The click's worker does that (`crate::ide`). The input is a
//! hostile, undocumented transcript, so the rule FAILS TOWARD "not a file target":
//! a target this module cannot read is left to the existing refusal for a link
//! snapback does not open, never guessed at.
//!
//! The grammar is the ONE rule both readers share: [`markdown_target`] for a
//! `[label](target)` link and [`backtick_target`] for an inline code span, which
//! is stricter (a backticked word is prose far more often than a link is).
//! A target never OPENS with a scheme (`file:///etc/passwd`, `vscode://file/x`,
//! `mailto:a@b`); a trailing `:N`, `:N-M` or `:N:C` is a line suffix, and any other
//! `:` is part of a folder or file name.

use std::path::{Path, PathBuf};

/// The longest extension (without its dot) a `name.ext` file name may carry.
///
/// Bounds what counts as an extension so a sentence fragment such as `see/this.is.a.long.word`
/// is not read as a file: no real extension in a source tree comes close
/// (`.properties`, `.mjs`, `.tsbuildinfo` are the long ones).
const MAX_EXTENSION_LEN: usize = 12;

/// Characters a path may not contain, on top of whitespace and control
/// characters: glob and shell syntax (`src/*.rs`, `a|b.rs`, `<file>.md`), which
/// is prose about files rather than a file.
const FORBIDDEN_PATH_CHARS: &[char] = &['*', '?', '<', '>', '|', '"', '\'', '`', '{', '}', '\\'];

/// A path a preview link names, with the line it points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTarget {
    /// The path as written, with its line suffix removed: relative to some
    /// folder, or absolute. Never empty and never opening with a URL scheme, but
    /// it MAY hold a `:` inside it (a folder named after a timestamp): only a
    /// trailing `:N`, `:N-M` or `:N:C` is a line suffix, and a launcher whose own
    /// parser splits on every `:` must not be handed a line with such a path
    /// (`crate::ide::Ide::passed_line`).
    pub path: String,
    /// The 1-based line the link points at (the START of a `:N-M` range).
    pub line: Option<u32>,
}

impl LocalTarget {
    /// The absolute paths this target can be, in the order they are tried.
    ///
    /// An absolute path is itself; a relative one is joined onto each of `dirs` —
    /// the folders its link resolves against, in precedence order — that is
    /// ABSOLUTE (a relative folder names nothing without a working directory this
    /// process cannot vouch for), without repeating one. Empty when a relative
    /// path has no absolute folder to resolve against.
    pub fn candidates(&self, dirs: &[String]) -> Vec<PathBuf> {
        let path = Path::new(&self.path);
        if path.is_absolute() {
            return vec![path.to_path_buf()];
        }
        let mut out: Vec<PathBuf> = Vec::new();
        for dir in dirs.iter().map(Path::new).filter(|d| d.is_absolute()) {
            let joined = dir.join(path);
            if !out.contains(&joined) {
                out.push(joined);
            }
        }
        out
    }
}

/// Read `raw`, the target of a `[label](target)` markdown link, as a local file:
/// `None` when it is anything else — a URL of any scheme, a `#anchor`, a bare word.
///
/// The path needs a `/` or a `name.ext` final component, so `[x](u)` and
/// `[x](Makefile)` stay plain links rather than becoming file targets that refuse.
pub fn markdown_target(raw: &str) -> Option<LocalTarget> {
    parse(raw).filter(|t| has_separator(&t.path) || has_extension(file_name(&t.path)))
}

/// Read `raw`, the text of an inline code span, as a local file: `None` unless it
/// holds a `/` and ends in `name.ext`, with an optional `:N`, `:N-M`, `:N:C` or
/// `#LN` after it. A bare file name (`dataLayer.ts`) is ambiguous — which of
/// several? — so it is not a target.
pub fn backtick_target(raw: &str) -> Option<LocalTarget> {
    parse(raw).filter(|t| has_separator(&t.path) && has_extension(file_name(&t.path)))
}

fn has_separator(path: &str) -> bool {
    path.contains('/')
}

/// The final `/`-separated component of `path` (empty for a trailing `/`).
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Whether `name` is `stem.ext` with a non-empty stem and a letter-led
/// alphanumeric extension (so `1.5` and `v2.` are not files).
fn has_extension(name: &str) -> bool {
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty()
        && !ext.is_empty()
        && ext.len() <= MAX_EXTENSION_LEN
        && ext.starts_with(|c: char| c.is_ascii_alphabetic())
        && ext.chars().all(|c| c.is_ascii_alphanumeric())
}

/// The shared grammar: `path`, then optionally `:N`, `:N-M` or `:N:C`, then
/// optionally `#LN` or `#LN-LM` (a fragment that is not a line is an anchor and is
/// dropped).
///
/// The suffix is read off the END, so a `:` INSIDE the path survives: a plan
/// folder named after its timestamp (`docs/plans/2026-11-06T05:42:19Z-x/A.md`) is a
/// path, while `file:///etc/passwd` and `mailto:a@b` open with a scheme and are not
/// ([`has_scheme`]).
fn parse(raw: &str) -> Option<LocalTarget> {
    let (before_fragment, fragment) = match raw.split_once('#') {
        Some((head, tail)) => (head, Some(tail)),
        None => (raw, None),
    };
    let (path, suffix_line) = split_line_suffix(before_fragment);
    if !is_plain_path(path) || has_scheme(path) {
        return None;
    }
    Some(LocalTarget {
        path: path.to_string(),
        line: suffix_line.or_else(|| fragment.and_then(fragment_line)),
    })
}

/// A path snapback will consider: non-empty, one token, no glob or shell syntax,
/// no leading `~` (a home shorthand it does not expand) and no leading `//`.
fn is_plain_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('~')
        && !path.starts_with("//")
        && !path
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || FORBIDDEN_PATH_CHARS.contains(&c))
}

/// Whether `path` opens with a URL scheme (`https:`, `file:`, `vscode:`, `mailto:`).
///
/// A scheme is a letter-led run of letters, digits, `+`, `.` and `-` up to the
/// FIRST `:`, so `docs/plans/05:42/a.md` (a `/` before the `:`) is not one. The
/// line suffix is split off BEFORE this is asked, which is what lets `a.ts:21`
/// (scheme-shaped, and not one) through.
fn has_scheme(path: &str) -> bool {
    path.split_once(':').is_some_and(|(head, _)| {
        head.starts_with(|c: char| c.is_ascii_alphabetic())
            && head
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
    })
}

/// Split a trailing `:N`, `:N-M` or `:N:C` off `s`: the rest, and the line (the
/// leftmost number, the start of a range). `s` unchanged, and no line, when it
/// ends in neither.
fn split_line_suffix(s: &str) -> (&str, Option<u32>) {
    let Some((head, last)) = s.rsplit_once(':').filter(|(_, n)| is_line_or_range(n)) else {
        return (s, None);
    };
    // `N:C`: the number just stripped was a column, so the line is the one before.
    if let Some((path, line)) = head
        .rsplit_once(':')
        .filter(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .filter(|_| last.bytes().all(|b| b.is_ascii_digit()))
    {
        return (path, leading_number(line).map(|(n, _)| n));
    }
    (head, leading_number(last).map(|(n, _)| n))
}

/// `N` or `N-M`, each at most 9 digits.
fn is_line_or_range(s: &str) -> bool {
    let Some((_, rest)) = leading_number(s) else {
        return false;
    };
    rest.is_empty()
        || rest
            .strip_prefix('-')
            .and_then(leading_number)
            .is_some_and(|(_, tail)| tail.is_empty())
}

/// The line of a `L21` / `L21-L40` fragment; `None` for an ordinary anchor.
fn fragment_line(fragment: &str) -> Option<u32> {
    let rest = fragment.strip_prefix('L')?;
    let (line, tail) = leading_number(rest)?;
    let tail_ok = tail.is_empty()
        || tail
            .strip_prefix("-L")
            .or_else(|| tail.strip_prefix('-'))
            .and_then(leading_number)
            .is_some_and(|(_, t)| t.is_empty());
    tail_ok.then_some(line)
}

/// The decimal number `s` starts with (at most 9 digits, so it fits a `u32`) and
/// what follows it.
fn leading_number(s: &str) -> Option<(u32, &str)> {
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || digits > 9 {
        return None;
    }
    let (number, rest) = s.split_at(digits);
    number.parse().ok().map(|n| (n, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(path: &str, line: Option<u32>) -> Option<LocalTarget> {
        Some(LocalTarget {
            path: path.to_string(),
            line,
        })
    }

    #[test]
    fn a_markdown_link_to_a_relative_path_is_a_file_target() {
        assert_eq!(
            markdown_target("docs/adr/0003-separate-analytics-per-environment.md"),
            target("docs/adr/0003-separate-analytics-per-environment.md", None)
        );
        assert_eq!(markdown_target("README.md"), target("README.md", None));
        assert_eq!(markdown_target("../a/b"), target("../a/b", None));
        assert_eq!(markdown_target("/etc/hosts"), target("/etc/hosts", None));
    }

    #[test]
    fn a_line_suffix_is_read_off_every_spelling_and_the_range_keeps_its_start() {
        for (raw, line) in [
            ("src/a.ts:21", 21),
            ("src/a.ts:21-40", 21),
            ("src/a.ts:21:5", 21),
            ("src/a.ts#L21", 21),
            ("src/a.ts#L21-L40", 21),
        ] {
            assert_eq!(
                markdown_target(raw),
                target("src/a.ts", Some(line)),
                "{raw}"
            );
            assert_eq!(
                backtick_target(raw),
                target("src/a.ts", Some(line)),
                "{raw}"
            );
        }
        // An ordinary anchor is not a line: the file still opens, at its top.
        assert_eq!(
            markdown_target("docs/a.md#install"),
            target("docs/a.md", None)
        );
    }

    #[test]
    fn a_target_with_a_scheme_or_no_path_shape_is_not_a_file() {
        for raw in [
            "https://example.com/a.md",
            "file:///etc/passwd",
            "vscode://file/Users/x/a.ts:3",
            "mailto:a@b.c",
            "javascript:alert(1)",
            "#anchor",
            "u",
            "Makefile",
            "",
            "//host/share/a.md",
            "~/notes/a.md",
            "src/*.rs",
            "a b/c.md",
        ] {
            assert_eq!(markdown_target(raw), None, "{raw:?}");
        }
    }

    #[test]
    fn a_colon_inside_the_path_is_part_of_the_name_not_a_scheme() {
        let plan = "docs/plans/2026-11-06T05:42:19Z-handover/HANDOVER.md";
        assert_eq!(markdown_target(plan), target(plan, None));
        assert_eq!(backtick_target(plan), target(plan, None));
        assert_eq!(backtick_target(&format!("{plan}:7")), target(plan, Some(7)));
    }

    #[test]
    fn a_backticked_path_needs_a_slash_and_a_file_name_with_an_extension() {
        assert_eq!(
            backtick_target("frontend/apps/mono/src/dataLayer.ts:21-40"),
            target("frontend/apps/mono/src/dataLayer.ts", Some(21))
        );
        assert_eq!(
            backtick_target("/abs/dir/name.rs"),
            target("/abs/dir/name.rs", None)
        );
        // A bare name is ambiguous; a path with no extension, a version, a
        // directory and a fraction are prose.
        for raw in [
            "_document.page.tsx:28-34",
            "dataLayer.ts",
            "src/lib",
            "src/",
            "1/2.5",
            "and/or",
            "src/a.ts()",
            "https://a.b/c.md",
        ] {
            assert_eq!(backtick_target(raw), None, "{raw:?}");
        }
    }

    #[test]
    fn candidates_resolve_a_relative_path_against_each_absolute_folder_once() {
        let t = markdown_target("docs/a.md").unwrap();
        assert_eq!(
            t.candidates(&["/w".into(), "/w/sub".into(), "/w".into(), "rel".into()]),
            [
                PathBuf::from("/w/docs/a.md"),
                PathBuf::from("/w/sub/docs/a.md")
            ]
        );
        assert!(t.candidates(&[]).is_empty());
        assert!(t.candidates(&["rel".into()]).is_empty());
        let abs = markdown_target("/etc/a.md").unwrap();
        assert_eq!(abs.candidates(&["/w".into()]), [PathBuf::from("/etc/a.md")]);
    }
}
