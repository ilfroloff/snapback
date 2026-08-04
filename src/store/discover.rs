//! Session file discovery.
//!
//! Resolves the store root from `$CLAUDE_PROJECTS_DIR` or `~/.claude/projects`
//! and enumerates ONLY `<encoded-cwd>/<session-id>.jsonl` files at exactly one
//! directory below the root. Never descends into `<session-id>/subagents/` —
//! subagent transcripts (~62% of files) masquerade as sessions and must be
//! excluded (see the Risks table). Returns candidate file paths.

use std::path::{Component, Path, PathBuf};

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

/// Pure name-shape predicate: does `path` look like a consumable session file
/// relative to `root`?
///
/// True iff `path` is exactly two components below `root` and its final
/// component ends in `.jsonl`. This is intentionally a shape-only check: it
/// does NOT inspect metadata, so it can classify a path even after the file
/// has been removed. That matters for the watcher, which must decide whether a
/// deletion event is worth a reload.
///
/// This is the same rule `discover` uses; extracting it keeps the watcher and
/// discovery from drifting apart.
pub fn is_session_path(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    let mut components = relative.components();
    let (Some(Component::Normal(_)), Some(Component::Normal(file)), None) =
        (components.next(), components.next(), components.next())
    else {
        return false;
    };
    file.to_str()
        .is_some_and(|name| name.rsplit_once('.').is_some_and(|(_, ext)| ext == "jsonl"))
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
}
