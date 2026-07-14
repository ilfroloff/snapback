//! Repo/branch grouping heuristic.
//!
//! `repo_of` maps a cwd to a repo-root label: `<repo>-worktrees/<branch>` and
//! `<repo>.worktrees/<branch>` cwd shapes collapse to a `<parent>/<base>` repo
//! label; otherwise the repo label is the cwd basename. `git_branch` is
//! authoritative for the branch level; a missing branch defaults to
//! `(detached)`.

use std::path::Path;

/// Branch label used when a session carries no `gitBranch`.
pub const DETACHED: &str = "(detached)";

/// Derive a repo-root label from a session's `cwd`.
///
/// Operates on the raw path string (not a normalized `Path`) so the substring
/// worktree detection is exact:
///
/// * `*-worktrees[/...]`  -> root is the text before the first `-worktrees`.
/// * `*.worktrees[/...]`  -> root is the text before the first `.worktrees`.
/// * otherwise            -> the repo label is the cwd basename.
///
/// In the worktree case the repo dir's basename alone is often ambiguous (e.g.
/// `fe`), so the label is `<parent>/<base>` when a distinct parent exists.
pub fn repo_of(cwd: &Path) -> String {
    let p = cwd.to_string_lossy();

    // Locate the first worktree marker: strip from the first occurrence to the
    // end, keeping the prefix.
    let root: &str = if let Some(idx) = p.find("-worktrees") {
        &p[..idx]
    } else if let Some(idx) = p.find(".worktrees") {
        &p[..idx]
    } else {
        // Plain repo: the path basename.
        return str_basename(&p).to_string();
    };

    // Worktree case: `root` is the repo dir. Show `<parent>/<base>` unless the
    // parent is empty or identical to the base.
    let base = str_basename(root);
    let parent = str_basename(str_dirname(root));
    if !parent.is_empty() && parent != base {
        format!("{parent}/{base}")
    } else {
        base.to_string()
    }
}

/// The text after the last `/`, or all of `s` if there is none.
fn str_basename(s: &str) -> &str {
    match s.rfind('/') {
        Some(i) => &s[i + 1..],
        None => s,
    }
}

/// The text before the last `/`, or all of `s` if there is none.
fn str_dirname(s: &str) -> &str {
    match s.rfind('/') {
        Some(i) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn repo(p: &str) -> String {
        repo_of(&PathBuf::from(p))
    }

    #[test]
    fn plain_repo_uses_basename() {
        assert_eq!(repo("/Users/me/project-alpha"), "project-alpha");
        assert_eq!(repo("/Users/me/acme/ai-to-go"), "ai-to-go");
    }

    #[test]
    fn dash_worktree_collapses_to_parent_base() {
        assert_eq!(repo("/Users/me/acme/web-worktrees/feature-x"), "acme/web");
    }

    #[test]
    fn dot_worktree_collapses_to_parent_base() {
        assert_eq!(repo("/Users/me/acme/web.worktrees/bugfix"), "acme/web");
    }

    #[test]
    fn worktree_without_parent_uses_base() {
        // root becomes `/foo` -> dirname is empty -> parent empty -> base only.
        assert_eq!(repo("/foo-worktrees/branch"), "foo");
    }

    #[test]
    fn worktree_suffix_without_trailing_branch() {
        assert_eq!(repo("/a/b-worktrees"), "a/b");
    }
}
