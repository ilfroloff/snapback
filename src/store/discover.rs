//! Session file discovery.
//!
//! Resolves the store root from the Claude PROFILE
//! (`config::claude_config_dir()` — i.e. `$CLAUDE_CONFIG_DIR`, else `~/.claude`)
//! joined with `projects`, unless `config::claude_projects_override()` (i.e.
//! `$CLAUDE_PROJECTS_DIR`) is set and non-empty, in which case IT wins — a
//! FIXTURES/DEMO-only override of the store view that does NOT change the
//! profile any spawned `claude` child uses (see `store_root_from`'s doc comment
//! for why it stays highest-precedence). A relocated profile uses the IDENTICAL
//! `<encoded-cwd>/<id>.jsonl` on-disk layout, so a moved root changes only WHERE
//! this module looks, never HOW it looks — the two-level-deep scan below is
//! unaffected either way.
//!
//! This module reads NO environment variables itself: both `config_dir` and
//! `projects_override` are composed from `config`, the single env reader (see
//! its module doc), so the store view and the child `claude`'s profile can
//! never independently disagree from this side.
//!
//! Enumerates ONLY `<encoded-cwd>/<session-id>.jsonl` files at exactly one
//! directory below the root. Never descends into `<session-id>/subagents/` —
//! subagent transcripts (~62% of files) masquerade as sessions and must be
//! excluded (see the Risks table). Returns candidate file paths.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use crate::config;

/// Subdirectory of the Claude profile holding transcripts. Kept as a literal
/// here — the module that joins it — rather than in `config`, per the "one
/// literal, one home" convention recorded at `config::CLAUDE_DIR_NAME`'s doc
/// comment; do not duplicate it.
const PROJECTS_SUBDIR: &str = "projects";

/// Pure precedence decision behind [`store_root`]: a set, non-empty
/// `projects_override` (i.e. `$CLAUDE_PROJECTS_DIR`) wins over
/// `config_dir.join("projects")`; an empty override is treated as unset,
/// matching `config`'s existing rule for every other override in this crate.
///
/// The override stays HIGHEST precedence — outranking the profile itself —
/// because it is load-bearing for three consumers that must stay deterministic
/// regardless of the ambient Claude profile: the board tests in
/// `src/tui/update.rs` that set `CLAUDE_PROJECTS_DIR`, the release workflow's
/// `sb`/`snapback` parity check (`.github/workflows/npm-release.yml`), and the
/// demo-GIF recording procedure (the variable's documented contract, those
/// consumers included, is `docs/agents/OPERATIONS.md#environment`). Each wants
/// "point the board at THIS directory of fixtures and nothing else"; a
/// profile-derived root that could override it would make all three depend on
/// the runner's or developer's ambient profile.
pub fn store_root_from(projects_override: Option<&str>, config_dir: &Path) -> PathBuf {
    if let Some(dir) = projects_override {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    config_dir.join(PROJECTS_SUBDIR)
}

/// Resolve the store root: the thin, impure composition of `config`'s two
/// readers over the pure [`store_root_from`]. See that function's doc comment
/// for the precedence rule and why the override outranks the profile.
pub fn store_root() -> PathBuf {
    store_root_from(
        config::claude_projects_override().as_deref(),
        &config::claude_config_dir(),
    )
}

/// D2: make the one remaining way the store view and the child `claude`'s
/// profile can disagree VISIBLE rather than silent.
///
/// `None` unless `projects_override` (`$CLAUDE_PROJECTS_DIR`) is set, non-empty,
/// AND its path components differ from `config_dir.join("projects")`'s —
/// compared via [`Path::components`] equality so a trailing slash or a `./`
/// segment is never falsely reported as a mismatch. Does **no filesystem
/// access** (no `canonicalize`: it fails on a path that does not exist yet, and
/// would have to fail soft regardless), so the check costs one component
/// compare and never blocks startup.
///
/// The returned message names BOTH paths — the override actually in effect and
/// the profile's own store root — so a one-time board status can say plainly
/// that the board is showing the override, not the profile it would otherwise
/// agree with.
///
/// `lib::run` calls this once, before the board loop starts, and turns `Some`
/// into a one-time board status via `App::set_status`.
pub fn store_override_note(projects_override: Option<&str>, config_dir: &Path) -> Option<String> {
    let dir = projects_override?;
    if dir.is_empty() {
        return None;
    }
    let override_path = Path::new(dir);
    let profile_root = config_dir.join(PROJECTS_SUBDIR);
    if override_path.components().eq(profile_root.components()) {
        return None;
    }
    Some(format!(
        "showing CLAUDE_PROJECTS_DIR override {} instead of the profile's store at {}",
        override_path.display(),
        profile_root.display()
    ))
}

/// Where a path sits in the store's ONE consumable shape,
/// `<root>/<encoded-cwd>/<session-id>.jsonl`.
///
/// This enum — and [`depth_of`], the single `match` behind it — is the ONLY
/// place the store's depth arithmetic is written down. The SUBAGENT EXCLUSION
/// BY DEPTH rule in `AGENTS.md` says the shape rule is never duplicated, and
/// [`is_session_path`] alone could not carry that: it answers one yes/no
/// question, while the watcher has to react to each LEVEL separately (a change
/// at the root can reshape the tree; a change below the session level provably
/// cannot matter). Both consumers therefore classify through this enum rather
/// than re-deriving a component count, so moving the consumable shape means
/// editing one `match`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreDepth {
    /// Not under `root` at all, so the store's shape rules nothing out.
    Outside,
    /// `root` itself.
    Root,
    /// One component below the root: an `<encoded-cwd>` entry. The level that
    /// HOLDS sessions, never one itself.
    CwdLevel,
    /// Two components below the root: the ONLY level a consumable session can
    /// live at.
    SessionLevel,
    /// Three or more components below the root — `<id>/subagents/agent-*.jsonl`
    /// and anything else deeper. Never consumable.
    BelowSession,
}

/// Classify where `path` sits relative to `root` in the store's shape.
///
/// Pure and metadata-free, like [`is_session_path`]: it answers for a path that
/// has already been deleted, which is what lets the watcher judge a removal.
pub fn store_depth(root: &Path, path: &Path) -> StoreDepth {
    match store_relative(root, path) {
        Some(relative) => depth_of(relative),
        None => StoreDepth::Outside,
    }
}

/// `path` expressed relative to `root`, or `None` when it is not under it.
fn store_relative<'a>(root: &Path, path: &'a Path) -> Option<&'a Path> {
    path.strip_prefix(root).ok()
}

/// The one place the level literals live. Everything else names a variant.
fn depth_of(relative: &Path) -> StoreDepth {
    match relative.components().count() {
        0 => StoreDepth::Root,
        1 => StoreDepth::CwdLevel,
        2 => StoreDepth::SessionLevel,
        _ => StoreDepth::BelowSession,
    }
}

/// Pure name-shape predicate: does `path` look like a consumable session file
/// relative to `root`?
///
/// True iff `path` sits at [`StoreDepth::SessionLevel`] under two ordinary name
/// components and carries the `.jsonl` extension. This is intentionally a
/// shape-only check: it does NOT inspect metadata, so it can classify a path
/// even after the file has been removed. That matters for the watcher, which
/// must decide whether a deletion event is worth a reload.
///
/// This is the same rule `discover` uses; extracting it keeps the watcher and
/// discovery from drifting apart.
pub fn is_session_path(root: &Path, path: &Path) -> bool {
    let Some(relative) = store_relative(root, path) else {
        return false;
    };
    if depth_of(relative) != StoreDepth::SessionLevel {
        return false;
    }
    let mut components = relative.components();
    let (Some(Component::Normal(_)), Some(Component::Normal(file))) =
        (components.next(), components.next())
    else {
        return false;
    };
    has_jsonl_extension(file)
}

/// Does this file NAME carry the `.jsonl` extension?
///
/// Asked of the raw [`OsStr`] through `Path`'s own extension split, never of a
/// `to_str()` view, and both halves of that are load-bearing:
///
/// * A filename the platform accepts but UTF-8 does not is still a session file
///   on disk. Answering `false` for it would NARROW discovery — the one
///   direction the parse-cache rule in `AGENTS.md` forbids, since a row may be
///   briefly stale but must never go missing.
/// * `Path`'s split keeps a bare `.jsonl` OUT: a leading dot with nothing before
///   it is a hidden file with no extension, not a session whose id is empty.
///   A hand-rolled "text after the last dot" test admits it.
fn has_jsonl_extension(file: &OsStr) -> bool {
    Path::new(file)
        .extension()
        .is_some_and(|ext| ext == "jsonl")
}

/// Enumerate resumable session files: `<root>/<encoded-cwd>/<session-id>.jsonl`.
///
/// This is the load-bearing subagent-exclusion rule. The store lays subagent
/// transcripts out at `<encoded-cwd>/<session-id>/subagents/agent-*.jsonl`,
/// which is deeper than the single directory level scanned here. Because those
/// files carry the PARENT's `cwd` + `sessionId`, listing them would surface
/// phantom, wrong-target "sessions" (~62% of the store). Depth is therefore
/// pinned to exactly two levels: direct child directories of the root, and the
/// `.jsonl` files directly inside them. Nested `<session-id>/` directories are
/// skipped (they are directories, not `.jsonl` files) and never descended into.
///
/// Fail-soft: an unreadable root yields an empty list; an unreadable child
/// directory is skipped rather than aborting the scan.
pub fn discover(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();

    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return out,
    };

    for entry in entries.flatten() {
        // Depth 1 must be a directory (an encoded-cwd folder).
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if !is_dir {
            continue;
        }
        let child = entry.path();
        let inner = match std::fs::read_dir(&child) {
            Ok(inner) => inner,
            Err(_) => continue,
        };
        for file in inner.flatten() {
            // Depth 2 must be a regular `.jsonl` FILE. Skipping the nested
            // `<session-id>/` directories here is what excludes subagents.
            let is_file = file.file_type().map(|t| t.is_file()).unwrap_or(false);
            let path = file.path();
            if is_file && is_session_path(root, &path) {
                out.push(path);
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_session_path_depth_two_jsonl() {
        let root = Path::new("/store");
        assert!(is_session_path(root, &root.join("cwd").join("sess.jsonl")));
    }

    #[test]
    fn is_session_path_depth_one_false() {
        let root = Path::new("/store");
        assert!(!is_session_path(root, &root.join("sess.jsonl")));
    }

    #[test]
    fn is_session_path_depth_three_subagent_false() {
        let root = Path::new("/store");
        assert!(!is_session_path(
            root,
            &root
                .join("cwd")
                .join("sess")
                .join("subagents")
                .join("agent-1.jsonl")
        ));
    }

    #[test]
    fn is_session_path_depth_two_txt_false() {
        let root = Path::new("/store");
        assert!(!is_session_path(root, &root.join("cwd").join("notes.txt")));
    }

    #[test]
    fn is_session_path_depth_two_json_false() {
        let root = Path::new("/store");
        assert!(!is_session_path(root, &root.join("cwd").join("data.json")));
    }

    #[test]
    fn is_session_path_outside_root_false() {
        let root = Path::new("/store");
        assert!(!is_session_path(root, Path::new("/other/cwd/sess.jsonl")));
    }

    /// A file named exactly `.jsonl` has no stem, so it is a HIDDEN file with no
    /// extension rather than a session whose id is the empty string. A
    /// hand-rolled "text after the last dot" test admits it; `Path`'s own split
    /// does not, and this pins that.
    #[test]
    fn is_session_path_bare_dot_jsonl_false() {
        let root = Path::new("/store");
        assert!(!is_session_path(root, &root.join("cwd").join(".jsonl")));
    }

    /// A `.jsonl` whose NAME is not valid UTF-8 is still a session file on disk,
    /// so the predicate must still say yes. Excluding it would NARROW discovery
    /// — a session present on disk that never reaches the board — which is the
    /// one direction `AGENTS.md`'s parse-cache rule rules out. Unix-only because
    /// only there can an arbitrary byte string be an `OsStr` filename.
    #[cfg(unix)]
    #[test]
    fn is_session_path_non_utf8_name_is_still_a_session() {
        use std::os::unix::ffi::OsStrExt;

        let root = Path::new("/store");
        // 0xFF/0xFE are not valid UTF-8 in any position.
        let name = OsStr::from_bytes(b"sess-\xff\xfe.jsonl");
        assert!(
            std::str::from_utf8(name.as_bytes()).is_err(),
            "the fixture must really be non-UTF-8, or this test proves nothing"
        );
        assert!(is_session_path(root, &root.join("cwd").join(name)));
    }

    /// The end-to-end counterpart of the predicate test above: a non-UTF-8-named
    /// `.jsonl` at depth 2 must come back from a real `discover()` scan, not just
    /// from the predicate in isolation.
    ///
    /// Whether such a name can EXIST is the filesystem's call, not the
    /// platform's: ext4 stores a filename as opaque bytes, while APFS and HFS+
    /// reject anything that is not valid UTF-8 (`EILSEQ`). So the fixture is
    /// attempted and the test bows out if the store's filesystem refuses it —
    /// there is no bug to catch on a filesystem that cannot hold the input. CI
    /// runs on Linux, where the write succeeds and this really executes.
    #[cfg(unix)]
    #[test]
    fn discover_finds_a_non_utf8_named_jsonl() {
        use std::os::unix::ffi::OsStrExt;
        use std::time::{SystemTime, UNIX_EPOCH};

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut root = std::env::temp_dir();
        root.push(format!(
            "snapback-discover-nonutf8-{}-{nanos}",
            std::process::id()
        ));
        let cwd = root.join("encoded-cwd");
        std::fs::create_dir_all(&cwd).expect("create encoded-cwd dir");
        let path = cwd.join(OsStr::from_bytes(b"sess-\xff\xfe.jsonl"));
        if std::fs::write(&path, b"{}\n").is_err() {
            let _ = std::fs::remove_dir_all(&root);
            return; // This filesystem cannot name the file; nothing to assert.
        }

        let found = discover(&root);

        assert_eq!(
            found,
            vec![path],
            "a non-UTF-8-named .jsonl at depth 2 must still be discovered"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // --- store_depth: the ONE place the level literals live -----------------

    #[test]
    fn store_depth_root_itself() {
        let root = Path::new("/store");
        assert_eq!(store_depth(root, root), StoreDepth::Root);
    }

    #[test]
    fn store_depth_encoded_cwd_is_the_cwd_level() {
        let root = Path::new("/store");
        assert_eq!(store_depth(root, &root.join("cwd")), StoreDepth::CwdLevel);
    }

    #[test]
    fn store_depth_two_below_is_the_session_level() {
        let root = Path::new("/store");
        assert_eq!(
            store_depth(root, &root.join("cwd").join("sess.jsonl")),
            StoreDepth::SessionLevel
        );
        // Shape, not name: the level is the same for a non-session name.
        assert_eq!(
            store_depth(root, &root.join("cwd").join("notes.txt")),
            StoreDepth::SessionLevel
        );
    }

    #[test]
    fn store_depth_subagent_is_below_the_session_level() {
        let root = Path::new("/store");
        assert_eq!(
            store_depth(root, &root.join("cwd").join("sess").join("subagents")),
            StoreDepth::BelowSession
        );
        assert_eq!(
            store_depth(
                root,
                &root
                    .join("cwd")
                    .join("sess")
                    .join("subagents")
                    .join("agent-1.jsonl")
            ),
            StoreDepth::BelowSession
        );
    }

    #[test]
    fn store_depth_outside_root() {
        let root = Path::new("/store");
        assert_eq!(
            store_depth(root, Path::new("/other/cwd/sess.jsonl")),
            StoreDepth::Outside
        );
    }

    // `store_root_from` and `store_override_note` are pure functions of their
    // arguments — no env mutation anywhere below, matching AGENTS.md PURE +
    // TESTED. Both are tested with constructed paths, never `std::env::set_var`.
    // The `projects` segment is asserted as a LITERAL string throughout (never
    // via `PROJECTS_SUBDIR`), matching the repo's convention at
    // `config.rs:118-123` of never asserting through the const under test.

    // --- store_root_from -----------------------------------------------------

    #[test]
    fn store_root_from_prefers_a_set_non_empty_override() {
        let config_dir = Path::new("/p/.claude-work");
        assert_eq!(
            store_root_from(Some("/tmp/fix"), config_dir),
            PathBuf::from("/tmp/fix"),
            "a set, non-empty CLAUDE_PROJECTS_DIR override must win over the \
             profile-derived root"
        );
    }

    #[test]
    fn store_root_from_joins_the_literal_projects_segment_onto_the_profile_when_unset() {
        let config_dir = Path::new("/p/.claude-work");
        let root = store_root_from(None, config_dir);
        assert_eq!(root, PathBuf::from("/p/.claude-work/projects"));
        assert_eq!(
            root.file_name().and_then(|n| n.to_str()),
            Some("projects"),
            "the store root's final component must be the literal `projects` segment"
        );
    }

    #[test]
    fn store_root_from_treats_an_empty_override_as_unset() {
        let config_dir = Path::new("/p/.claude-work");
        assert_eq!(
            store_root_from(Some(""), config_dir),
            store_root_from(None, config_dir),
            "an empty CLAUDE_PROJECTS_DIR must fall through to the profile-derived \
             root, matching config's rule for every other override"
        );
    }

    // --- store_override_note --------------------------------------------------

    #[test]
    fn store_override_note_is_none_when_the_override_is_absent() {
        let config_dir = Path::new("/p/.claude-work");
        assert_eq!(
            store_override_note(None, config_dir),
            None,
            "no override set means nothing to disagree about"
        );
    }

    #[test]
    fn store_override_note_is_none_when_the_override_matches_the_profile_root() {
        let config_dir = Path::new("/p/.claude-work");
        assert_eq!(
            store_override_note(Some("/p/.claude-work/projects"), config_dir),
            None,
            "an override naming the SAME directory as the profile's store is not a \
             mismatch"
        );
    }

    #[test]
    fn store_override_note_ignores_a_trailing_slash() {
        let config_dir = Path::new("/p/.claude-work");
        assert_eq!(
            store_override_note(Some("/p/.claude-work/projects/"), config_dir),
            None,
            "Path::components() equality must not be fooled by a trailing slash"
        );
    }

    #[test]
    fn store_override_note_reports_a_genuinely_different_directory() {
        let config_dir = Path::new("/p/.claude-work");
        let note = store_override_note(Some("/tmp/fixtures"), config_dir)
            .expect("a genuinely different override must produce a note");
        assert!(
            note.contains("/tmp/fixtures"),
            "the note must name the override path actually in effect: {note}"
        );
        assert!(
            note.contains("/p/.claude-work/projects"),
            "the note must also name the profile's own store path: {note}"
        );
    }
}
