//! Session file discovery.
//!
//! Resolves the store root from `$CLAUDE_PROJECTS_DIR` or `~/.claude/projects`
//! and enumerates ONLY `<encoded-cwd>/<session-id>.jsonl` files at exactly one
//! directory below the root. Never descends into `<session-id>/subagents/` —
//! subagent transcripts (~62% of files) masquerade as sessions and must be
//! excluded (see the Risks table). Returns candidate file paths.
//!
//! It also owns the one shape check for a subagent TRANSCRIPT's path,
//! [`subagent_transcript_at`], which the watcher alone asks. Discovery never
//! does: a subagent is never a session.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use super::subagents;

/// Resolve the store root: `$CLAUDE_PROJECTS_DIR` if set and non-empty, else
/// `~/.claude/projects`.
pub fn store_root() -> PathBuf {
    if let Ok(dir) = std::env::var("CLAUDE_PROJECTS_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Some(home) = dirs::home_dir() {
        return home.join(".claude").join("projects");
    }
    // Last resort if the home directory cannot be resolved.
    PathBuf::from(".claude").join("projects")
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
        .is_some_and(|ext| ext == SESSION_EXTENSION)
}

/// The extension every consumable session file carries, `<session-id>.jsonl`.
const SESSION_EXTENSION: &str = "jsonl";

/// A subagent's transcript, named the way the board names the view showing it:
/// by the session that launched it and the subagent's own id. What
/// [`subagent_transcript_at`] reads off a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentTranscript {
    /// The launching session's transcript, `<root>/<encoded-cwd>/<id>.jsonl`,
    /// under the same `root` the path was given under: the file whose
    /// `subagents::subagents_dir` holds this transcript.
    pub session_file: PathBuf,
    /// The subagent's id, from the transcript's name `agent-<agent_id>.jsonl`.
    pub agent_id: String,
}

/// Pure name-shape check, for the WATCHER only: is `path` a subagent's
/// transcript, `<root>/<encoded-cwd>/<id>/subagents/agent-<agentId>.jsonl`?
///
/// `Some` names it; `None` answers everything else — a session, a meta file
/// (`agent-<id>.meta.json`), a forked skill's `.json` files, any other name, a
/// path one level shallower or deeper, one outside `root`, and a name that is not
/// UTF-8 (no meta file's name is, so no subagent view can show it). Like
/// [`is_session_path`] it reads no metadata, so it answers for any path a watcher
/// reports.
///
/// It counts no level of its own. The launching session's file must pass
/// [`is_session_path`], the one depth rule, and the transcript must sit in THAT
/// file's `subagents::subagents_dir` under a name `subagents::transcript_agent_id`
/// reads: the two derivations the subagent view opens a transcript through, so
/// the watcher can never name a file the view would not read. Discovery never
/// asks, because a subagent is never a session (AGENTS.md SUBAGENT EXCLUSION BY
/// DEPTH).
pub fn subagent_transcript_at(root: &Path, path: &Path) -> Option<SubagentTranscript> {
    let agent_id = subagents::transcript_agent_id(path.file_name()?.to_str()?)?;
    let dir = path.parent()?;
    // `<id>` + `.jsonl`, appended rather than set: an id holding a dot keeps it.
    let mut session_file = dir.parent()?.to_path_buf();
    session_file.add_extension(SESSION_EXTENSION);
    let in_its_own_dir = subagents::subagents_dir(&session_file).is_some_and(|own| own == dir);
    (in_its_own_dir && is_session_path(root, &session_file)).then(|| SubagentTranscript {
        session_file,
        agent_id: agent_id.to_owned(),
    })
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

    // --- subagent_transcript_at: the watcher's subagent shape ---------------

    /// The one shape it names, with the launching session's file and the id.
    #[test]
    fn a_subagent_transcript_names_its_session_file_and_agent_id() {
        let root = Path::new("/store");
        let path = root
            .join("cwd")
            .join("sess")
            .join("subagents")
            .join("agent-a1b2.jsonl");
        assert_eq!(
            subagent_transcript_at(root, &path),
            Some(SubagentTranscript {
                session_file: root.join("cwd").join("sess.jsonl"),
                agent_id: "a1b2".to_string(),
            })
        );
    }

    /// It reads back exactly what the subagent view's own derivations build from
    /// a session file — an id holding a dot included, which a `with_extension`
    /// would have cut short — so the watcher names the file the view opens.
    #[test]
    fn a_subagent_transcript_reads_back_the_views_own_derivation() {
        let root = Path::new("/store");
        for session_file in [
            root.join("-Users-me-p").join("sess-1.jsonl"),
            root.join("-Users-me-p").join("sess.v2.jsonl"),
        ] {
            let dir = subagents::subagents_dir(&session_file).expect("a session has a dir");
            let path = subagents::transcript_path(&dir, "a1b2");
            assert_eq!(
                subagent_transcript_at(root, &path),
                Some(SubagentTranscript {
                    session_file: session_file.clone(),
                    agent_id: "a1b2".to_string(),
                }),
                "{path:?}"
            );
        }
    }

    /// Every other file the store holds near one is refused: a session, the meta
    /// and forked-skill files beside the transcript, any other name, a level
    /// shallower or deeper, another dir under the session's, and a path outside
    /// the root.
    #[test]
    fn subagent_transcript_at_refuses_every_other_shape() {
        let root = Path::new("/store");
        let sess = root.join("cwd").join("sess");
        let subagents_dir = sess.join("subagents");
        for path in [
            root.join("cwd").join("sess.jsonl"),
            subagents_dir.join("agent-a1b2.meta.json"),
            subagents_dir.join("agent-a1b2.forked-skill.json"),
            subagents_dir.join("agent-a1b2.forked-skill.marker.json"),
            subagents_dir.join("notes.txt"),
            subagents_dir.join("a1b2.jsonl"),
            subagents_dir.clone(),
            sess.join("agent-a1b2.jsonl"),
            subagents_dir.join("nested").join("agent-a1b2.jsonl"),
            sess.join("tool-results").join("agent-a1b2.jsonl"),
            root.join("sess").join("subagents").join("agent-a1b2.jsonl"),
            PathBuf::from("/other/cwd/sess/subagents/agent-a1b2.jsonl"),
        ] {
            assert_eq!(subagent_transcript_at(root, &path), None, "{path:?}");
        }
    }
}
