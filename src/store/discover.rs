//! Session file discovery.
//!
//! Resolves the store root from `$CLAUDE_PROJECTS_DIR` or `~/.claude/projects`
//! and enumerates ONLY `<encoded-cwd>/<session-id>.jsonl` files at exactly one
//! directory below the root. Never descends into `<session-id>/subagents/` —
//! subagent transcripts (~62% of files) masquerade as sessions and must be
//! excluded (see the Risks table). Returns candidate file paths.

use std::path::{Path, PathBuf};

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
            if is_file && path.extension().is_some_and(|e| e == "jsonl") {
                out.push(path);
            }
        }
    }

    out
}
