//! Repo/branch grouping heuristic.
//!
//! `repo_root_of` maps a cwd to the repo-root PATH it belongs to, and `repo_of`
//! LABELS that same root for the group head — ONE derivation, two consumers, so
//! the scope and the grouping can never recognize different sets of layouts.
//! Three worktree layouts collapse sibling worktrees onto one shared root (and
//! therefore onto one shared `<parent>/<base>` repo label):
//!
//! * `<repo>-worktrees/<branch>`         — sibling-suffix layout.
//! * `<repo>.worktrees/<branch>`         — sibling-suffix layout, dot variant,
//!   which also covers a `<root>/.worktrees/<branch>` container.
//! * `<repo>/.<tool>/worktrees/<branch>` — a HIDDEN container dir whose
//!   immediate child is `worktrees`. One rule, not a list of tool names: it
//!   covers `.wtp/worktrees` (the `wtp` default), any user-configured `wtp`
//!   `base_dir` such as `.agents/worktrees`, and git's own `.git/worktrees`,
//!   without hard-coding a marker per tool. A VISIBLE `worktrees/` dir is an
//!   ordinary directory and deliberately does NOT collapse.
//!
//! Otherwise the repo label is the cwd basename. `git_branch` is authoritative
//! for the branch level; a missing branch defaults to `(detached)`.

use std::path::{Path, PathBuf};

/// Branch label used when a session carries no `gitBranch`.
pub const DETACHED: &str = "(detached)";

/// Separator these heuristics split on: session `cwd` values are POSIX paths.
const PATH_SEPARATOR: char = '/';

/// Sibling-suffix marker `<repo>-worktrees/<branch>` (e.g. `web-worktrees/`).
/// Not a hidden dir, so it needs its own rule.
const WT_MARKER_DASH: &str = "-worktrees";

/// Sibling-suffix marker `<repo>.worktrees/<branch>` (e.g. `web.worktrees/`).
/// Also matches a `<repo>/.worktrees/<branch>` container, whose child dirs are
/// branches rather than a nested `worktrees` segment.
const WT_MARKER_DOT: &str = ".worktrees";

/// Container segment name shared by every tool that parks worktrees inside a
/// hidden dir (`.wtp/worktrees`, `.agents/worktrees`, `.git/worktrees`, ...).
const WORKTREES_SEGMENT: &str = "worktrees";

/// Leading char that makes a path segment a hidden (dotted) directory. The
/// hidden parent is what distinguishes a worktree container from an ordinary
/// directory that happens to be named `worktrees`.
const HIDDEN_DIR_PREFIX: char = '.';

/// The repo-root PATH a session's `cwd` belongs to.
///
/// Operates on the raw path string (not a normalized `Path`) so the substring
/// worktree detection is exact:
///
/// * `*-worktrees[/...]`      -> root is the text before the first `-worktrees`.
/// * `*.worktrees[/...]`      -> root is the text before the first `.worktrees`.
/// * `*/.<tool>/worktrees/..` -> root is the text before the hidden container.
/// * otherwise                -> a plain checkout IS its own root, returned
///   unchanged.
///
/// This is the "same project?" test [`repo_of`]'s LABEL cannot answer: the label
/// spells a plain checkout `<base>` but a worktree `<parent>/<base>`, so a repo
/// and its own worktree carry DIFFERENT labels while sharing ONE root. Compare
/// roots, never labels.
///
/// Pure and git-free like everything else here, which is the point: it answers
/// for a worktree that has been REMOVED from disk, which git cannot.
///
/// A path with no UTF-8 spelling is lossily repaired in the worktree case (the
/// root has to be sliced out of the string the markers were found in); the plain
/// case is returned byte-for-byte. Both sides of any comparison go through this
/// one function, so equal inputs stay equal either way.
#[must_use]
pub fn repo_root_of(cwd: &Path) -> PathBuf {
    let p = cwd.to_string_lossy();
    match worktree_root_len(&p) {
        Some(root_len) => PathBuf::from(&p[..root_len]),
        None => cwd.to_path_buf(),
    }
}

/// Byte length of the repo-root prefix when `p` sits inside one of the three
/// worktree layouts, or `None` for a plain checkout.
///
/// The ONE place the markers are scanned. [`repo_root_of`] slices the path here
/// and [`repo_of`] labels what it sliced, so a layout either counts for both or
/// for neither.
fn worktree_root_len(p: &str) -> Option<usize> {
    // Locate the FIRST worktree marker across all layouts: strip from that
    // occurrence to the end, keeping the prefix. Taking the minimum keeps the
    // first-occurrence rule when a path carries more than one marker.
    [
        p.find(WT_MARKER_DASH),
        p.find(WT_MARKER_DOT),
        hidden_container_root_len(p),
    ]
    .into_iter()
    .flatten()
    .min()
}

/// Derive a repo-root label from a session's `cwd`: [`repo_root_of`]'s answer,
/// spelled for a group head.
///
/// A plain checkout is named by its basename. In the worktree case the repo
/// dir's basename alone is often ambiguous (e.g. `fe`), so the label is
/// `<parent>/<base>` when a distinct parent exists — which is exactly why two
/// folders of one project can carry two different labels, and why membership
/// questions must be asked of [`repo_root_of`] instead.
pub fn repo_of(cwd: &Path) -> String {
    let root = repo_root_of(cwd);
    let r = root.to_string_lossy();
    let base = str_basename(&r);

    // A cwd that IS its own root is a plain checkout (`repo_root_of` returns the
    // input untouched there, and a worktree root is always a strict prefix), so
    // the basename names it.
    if root.as_os_str() == cwd.as_os_str() {
        return base.to_string();
    }

    // Worktree case: `root` is the repo dir. Show `<parent>/<base>` unless the
    // parent is empty or identical to the base.
    let parent = str_basename(str_dirname(&r));
    if !parent.is_empty() && parent != base {
        format!("{parent}/{base}")
    } else {
        base.to_string()
    }
}

/// Byte length of the repo-root prefix for the hidden-container layout
/// `<root>/.<tool>/worktrees/<branch...>`, or `None` when `p` has no such shape.
///
/// The match requires a HIDDEN segment immediately followed by a `worktrees`
/// segment, which is what separates a worktree container from an ordinary
/// `worktrees/` directory (`/Users/me/code/worktrees/thing` must not collapse).
/// A path with nothing before the hidden segment has no root to label, so it
/// yields `None` and falls back to the basename.
fn hidden_container_root_len(p: &str) -> Option<usize> {
    let mut prev: Option<(usize, &str)> = None;
    let mut start = 0usize;

    for seg in p.split(PATH_SEPARATOR) {
        if seg == WORKTREES_SEGMENT {
            if let Some((prev_start, prev_seg)) = prev {
                // `prev_start` is the hidden segment's first byte, so the byte
                // before it is the separator that terminates the repo root.
                let root_len = prev_start.saturating_sub(PATH_SEPARATOR.len_utf8());
                if prev_seg.starts_with(HIDDEN_DIR_PREFIX) && root_len > 0 {
                    return Some(root_len);
                }
            }
        }
        prev = Some((start, seg));
        start += seg.len() + PATH_SEPARATOR.len_utf8();
    }

    None
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

    fn root(p: &str) -> PathBuf {
        repo_root_of(&PathBuf::from(p))
    }

    /// The repo this repo. Used by the root tests below so the fixtures are the
    /// real layouts this heuristic was written against.
    const SNAPBACK: &str = "/Volumes/Development/ilfroloff/snapback";

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
    fn wtp_hidden_container_collapses_to_parent_base() {
        // `wtp` default layout: `<root>/.wtp/worktrees/<branch...>`.
        assert_eq!(
            repo("/Volumes/Development/ilfroloff/snapback/.wtp/worktrees/feature/delete-session"),
            "ilfroloff/snapback"
        );
    }

    #[test]
    fn agents_hidden_container_collapses_to_parent_base() {
        // This repo's real layout: a `wtp` `base_dir: .agents/worktrees`.
        assert_eq!(
            repo(
                "/Volumes/Development/ilfroloff/snapback/.agents/worktrees/feature/quick-send-reply-to-session"
            ),
            "ilfroloff/snapback"
        );
    }

    #[test]
    fn git_hidden_container_collapses_to_parent_base() {
        // git's own metadata layout: `<root>/.git/worktrees/<name>`.
        assert_eq!(
            repo("/Users/me/acme/web/.git/worktrees/feature-y"),
            "acme/web"
        );
    }

    #[test]
    fn visible_worktrees_dir_does_not_collapse() {
        // `worktrees` under a NON-hidden parent is an ordinary directory name.
        assert_eq!(repo("/Users/me/code/worktrees/thing"), "thing");
    }

    /// ACCEPTED FALSE POSITIVE, pinned so the trade-off is explicit rather than
    /// discovered later: the hidden-container rule asks only "a hidden segment
    /// whose immediate child is `worktrees`", so ANY such dir collapses — a
    /// hypothetical `<root>/.cache/worktrees/tmp` relabels to `<parent>/<root>`
    /// even though nothing there is a git worktree.
    ///
    /// It stays this way ON PURPOSE. The container dir is named by the tool, not
    /// by git (`wtp`'s `base_dir` is user-configurable, and this repo sets it to
    /// `.agents/worktrees`), so enumerating known tool names would miss real
    /// layouts — the failure this generalization was introduced to fix. The
    /// residual cost is a mislabelled group head for a directory nobody keeps
    /// sessions in; the cost of the narrow rule was missing the layouts people
    /// actually use.
    #[test]
    fn any_hidden_worktrees_container_collapses_even_when_it_holds_no_worktrees() {
        assert_eq!(repo("/Users/me/acme/web/.cache/worktrees/tmp"), "acme/web");
    }

    #[test]
    fn hidden_container_without_root_uses_basename() {
        // No text precedes the hidden container, so there is no root to label.
        assert_eq!(repo("/.wtp/worktrees/feature"), "feature");
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

    // --- repo ROOT (the "same project?" path, not the label) ---------------

    /// All three worktree layouts round-trip onto the ONE root they hang off,
    /// and a plain checkout is its own root — the four cases the cross-worktree
    /// scope compares. This is the same marker scan `repo_of` labels, asked for
    /// the path instead of the name.
    #[test]
    fn every_worktree_layout_resolves_to_the_one_repo_root() {
        assert_eq!(root(SNAPBACK), PathBuf::from(SNAPBACK), "a plain checkout");
        assert_eq!(
            root("/Users/me/acme/web-worktrees/feature-x"),
            PathBuf::from("/Users/me/acme/web"),
            "sibling-suffix layout"
        );
        assert_eq!(
            root("/Users/me/acme/web.worktrees/bugfix"),
            PathBuf::from("/Users/me/acme/web"),
            "sibling-suffix layout, dot variant"
        );
        assert_eq!(
            root(&format!("{SNAPBACK}/.wtp/worktrees/feature/delete-session")),
            PathBuf::from(SNAPBACK),
            "hidden-container layout (`wtp`'s default)"
        );
        assert_eq!(
            root(&format!("{SNAPBACK}/.agents/worktrees/feature/quick-send")),
            PathBuf::from(SNAPBACK),
            "hidden-container layout (this repo's configured `base_dir`)"
        );
    }

    /// THE TRAP this function exists to avoid, pinned as a pair: a repo and its
    /// own worktree carry DIFFERENT `repo_of` labels — the worktree branch
    /// prepends the parent dir, the plain branch does not — so comparing labels
    /// answers "different project" for two folders of one project. The ROOTS are
    /// equal, which is the answer scope membership needs.
    #[test]
    fn a_repo_and_its_worktree_share_a_root_while_their_labels_differ() {
        let worktree = format!("{SNAPBACK}/.agents/worktrees/feature/quick-send");

        assert_ne!(
            repo(SNAPBACK),
            repo(&worktree),
            "premise: the LABELS disagree ({} vs {}), which is what makes a \
             label comparison silently wrong here",
            repo(SNAPBACK),
            repo(&worktree)
        );
        assert_eq!(
            root(SNAPBACK),
            root(&worktree),
            "the roots agree, so a root comparison says `same project`"
        );
    }

    /// An unrelated repo must never share a root, or the project scope would be
    /// the all scope with extra steps.
    #[test]
    fn an_unrelated_repo_has_a_different_root() {
        assert_ne!(root(SNAPBACK), root("/Volumes/Development/ilfroloff/other"));
        assert_ne!(
            root(SNAPBACK),
            root("/Volumes/Development/ilfroloff/other/.wtp/worktrees/x"),
            "including that repo's own worktrees"
        );
    }

    /// KNOWN, DELIBERATE LIMITATION — pinned so nobody "fixes" it into a
    /// regression. A worktree parked under `<root>/target/wtp-worktrees/<branch>`
    /// resolves to `<root>/target/wtp`, NOT to `<root>`, because the FIRST marker
    /// wins and `-worktrees` occurs before the path reaches the hidden-container
    /// rule. So its sessions stay outside the project scope.
    ///
    /// That is the correct trade. The first-occurrence rule is what keeps the
    /// heuristic total and predictable; special-casing this shape would mean
    /// scoring markers against each other, and a heuristic that ranks its own
    /// rules is one nobody can reason about from a path. One directory that is
    /// itself a build artifact costs one mislabelled group; the ranking would
    /// cost every path its predictability.
    #[test]
    fn a_dash_marker_inside_the_root_wins_over_the_hidden_container_rule() {
        assert_eq!(
            root(&format!("{SNAPBACK}/target/wtp-worktrees/feat/needs-input")),
            PathBuf::from(format!("{SNAPBACK}/target/wtp")),
            "the `-worktrees` marker fires first and roots this below the repo"
        );
        assert_ne!(
            root(&format!("{SNAPBACK}/target/wtp-worktrees/feat/needs-input")),
            PathBuf::from(SNAPBACK),
            "which is exactly why it is out of the project scope"
        );
    }
}
