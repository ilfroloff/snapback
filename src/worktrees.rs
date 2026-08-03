//! Which project a directory belongs to.
//!
//! Two answers to that one question, and the cross-worktree scope needs BOTH:
//!
//! * The LIVE worktree set — one fail-soft `git worktree list --porcelain`
//!   shell-out, parsed into the canonicalized worktree roots that make up ONE
//!   project, plus a display label for it. Authoritative, and the only thing
//!   that can relate two folders no path rule could.
//! * The REPO ROOT of a path ([`project_root`]) — the pure prefix
//!   [`group::repo_root_of`] derives, canonicalized. Weaker, but it answers for
//!   a worktree that has been REMOVED, which git by definition cannot: git
//!   reports what exists now, so a deleted worktree's sessions match no live
//!   root and would otherwise be visible only in the all scope.
//!
//! It lives OUTSIDE `src/store/*` on purpose: the store core stays git-free and
//! pure, so the worktree set is launch context (like the launch dir itself),
//! resolved here and handed to the TUI. It is a top-level module rather than a
//! `tui` child so the dependency runs one way only — `tui` -> `worktrees` ->
//! `store::group` — with no cycle.
//!
//! **FAIL-SOFT, toward "could not resolve".** A missing `git`, a launch dir that
//! is not a repository, a non-zero exit, non-UTF-8 output, or output that parses
//! to nothing all collapse to an EMPTY [`WorktreeSet`] — never a panic. Empty is
//! a meaningful answer, not an error: the caller keeps the [`project_root`] arm,
//! which needs no git at all, so a `-p` launch outside any git repo still shows
//! the repo it is standing in rather than an empty board.
//!
//! **Terminal-safe.** The child's stdout AND stderr are captured into pipes and
//! its stdin is null, so nothing git prints (notably a "not a git repository"
//! error) can ever reach the terminal the TUI is drawing on.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::store::group;

/// The program that owns the answer. `git` is the ONLY authority on which
/// worktrees a repository has right now — the layout on disk is a convention,
/// the porcelain list is a fact.
const GIT_PROGRAM: &str = "git";

/// `-C <dir>`: run git as if it had been started in `<dir>`.
///
/// Load-bearing, and the reason this needs no `chdir`: the process CWD is the
/// user's terminal CWD and must stay untouched, so the launch dir is passed as
/// data instead. Without it git would answer about whatever directory the TUI
/// happens to be running in.
const GIT_DIR_FLAG: &str = "-C";

/// The subcommand and the machine-readable format flag, declared once so the
/// invocation cannot drift.
///
/// `--porcelain` is what makes the output parseable at all: the bare `worktree
/// list` prints an aligned human table (path, sha, `[branch]`) whose columns
/// depend on terminal-independent padding, while `--porcelain` prints one
/// `<key> <value>` line per attribute in blank-line-separated records.
const WORKTREE_LIST_ARGV: [&str; 3] = ["worktree", "list", "--porcelain"];

/// The prefix of the ONE porcelain line this module reads: `worktree <path>`.
///
/// Every record starts with it and every other line (`HEAD`, `branch`,
/// `detached`, `bare`, `locked`, `prunable`) is ignored, so an unknown attribute
/// git may add later can never break the parse.
const WORKTREE_LINE_PREFIX: &str = "worktree ";

/// The set of worktree roots that make up one project, plus its display label.
///
/// `roots` holds CANONICALIZED directories (see [`resolve_dir`]) so membership
/// compares like-for-like with a session's resolved `cwd` across symlinks and
/// `/tmp` -> `/private/tmp`. `label` is the project's repo label, derived from
/// the MAIN worktree.
///
/// **An empty `roots` means "could not resolve / not a repo"** — never "a
/// project with no worktrees", which cannot exist. Callers must read it as "no
/// signal" and fall back, not as "nothing matches".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeSet {
    /// Canonicalized worktree roots; empty means the set could not be resolved.
    roots: HashSet<PathBuf>,
    /// The project label for the header, `None` when nothing was resolved.
    label: Option<String>,
}

impl WorktreeSet {
    /// The "could not resolve / not a repo" answer every failure path returns.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Build a set from roots that are ALREADY canonicalized.
    ///
    /// The name states the precondition because it cannot be checked: passing
    /// raw paths compiles and then silently fails to match sessions whose `cwd`
    /// resolves differently. Run each path through [`resolve_dir`] first.
    #[must_use]
    pub fn from_resolved(roots: impl IntoIterator<Item = PathBuf>, label: Option<String>) -> Self {
        Self {
            roots: roots.into_iter().collect(),
            label,
        }
    }

    /// Is `dir` one of this project's worktree roots?
    ///
    /// `dir` MUST already be canonicalized with [`resolve_dir`]; comparison is
    /// byte-equality on the resolved form, exactly like the exact-cwd scope.
    #[must_use]
    pub fn contains(&self, dir: &Path) -> bool {
        self.roots.contains(dir)
    }

    /// Whether the set carries no signal — see the type's docs: this is "could
    /// not resolve", not "no session matches".
    ///
    /// No longer a BRANCH anywhere in the runtime: the scope predicate reads the
    /// git set and the repo-root rule as two independent yeses, so an empty set
    /// simply admits nothing rather than switching behavior. It stays because it
    /// is the type's documented "did this resolve" question and the suite states
    /// its premises with it; hence the narrow `dead_code` allow (this crate is a
    /// BINARY, so `pub` alone does not make an item reachable).
    #[allow(dead_code)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// The project's display label, `None` when the set could not be resolved.
    #[must_use]
    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }
}

/// Canonicalize `p`, falling back to the raw path when it cannot be resolved
/// (e.g. a session whose worktree was deleted).
///
/// The ONE canonicalization both the scope predicate and the worktree resolver
/// use, so a launch dir, a session `cwd`, and a git-reported worktree root are
/// always compared in the same resolved form (symlinks, `.`/`..`, and
/// `/tmp`->`/private/tmp` collapsed). Two different notions of "the same
/// directory" would make membership silently wrong, so there is only one.
///
/// Fail-soft by design: an unresolvable path keeps its raw form rather than
/// dropping out, so a deleted worktree still compares equal to itself.
#[must_use]
pub fn resolve_dir(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// The canonicalized repo root `dir` belongs to — the project identity two paths
/// are compared on when the live worktree set cannot relate them.
///
/// **The ORDER is the contract.** The root prefix is derived from the RAW path
/// FIRST ([`group::repo_root_of`], a pure string rule) and only the PREFIX is
/// then canonicalized. Reversing the two breaks exactly the sessions this exists
/// to recover: a removed worktree's leaf does not exist, so [`resolve_dir`]
/// hands the whole path back RAW, and a symlinked prefix (`/tmp` ->
/// `/private/tmp`) would then stay unresolved on the deleted side while the live
/// launch dir resolved on the other — two spellings of one directory, compared
/// unequal. The prefix is the repo itself, which is still on disk, so
/// canonicalizing it always succeeds.
#[must_use]
pub fn project_root(dir: &Path) -> PathBuf {
    resolve_dir(&group::repo_root_of(dir))
}

/// What to CALL the project `launch_dir` belongs to, when git resolved no label:
/// the repo ROOT's own name, or its whole path when it has none (`/`), lossily
/// repaired so a path with no UTF-8 spelling still yields a name.
///
/// Declared ONCE because two surfaces take this fallback — the one group head
/// (`tui::app`'s `App::project_label`) and the header (`tui::view`'s
/// `project_name`) — and a project named two different things on one screen is
/// the bug they exist to prevent.
///
/// It names the ROOT rather than the launch dir because the project scope is
/// ROOTED there: a worktree's directory is named after its BRANCH, so heading a
/// list drawn from the whole project with it would misdescribe the list. That is
/// the same argument that makes the git label beat both fallbacks, extended to
/// the case where there is no git label. For a plain checkout the root IS the
/// launch dir, so nothing changes there.
#[must_use]
pub fn project_root_name(launch_dir: &Path) -> String {
    let root = group::repo_root_of(launch_dir);
    root.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned())
}

/// Build the argv of the worktree probe (`git -C <launch_dir> worktree list
/// --porcelain`); `argv[0]` is the program.
///
/// Pure and separate from the spawn, like [`crate::agents`]' argv builders: the
/// exact invocation is a contract with an external CLI, so it must be assertable
/// WITHOUT spawning git (which the suite never does).
#[must_use]
fn git_worktree_argv(launch_dir: &Path) -> Vec<OsString> {
    let mut argv = Vec::with_capacity(3 + WORKTREE_LIST_ARGV.len());
    argv.push(OsString::from(GIT_PROGRAM));
    argv.push(OsString::from(GIT_DIR_FLAG));
    // As an `OsString`, so a non-UTF-8 launch dir is passed through untouched.
    argv.push(launch_dir.as_os_str().to_os_string());
    argv.extend(WORKTREE_LIST_ARGV.iter().map(OsString::from));
    argv
}

/// Decide what a finished worktree probe MEANS: the parsed set when git exited
/// zero with UTF-8 output, an EMPTY set when it did not.
///
/// Pure, and split out of [`resolve`] exactly as
/// [`crate::agents::agents_from_output`] is split out of its spawn: "a non-zero
/// exit is no signal" is a DECISION, and a decision left inside the impure
/// wrapper is only reachable by running git.
///
/// The status is checked BEFORE the parse, and that order is the contract: a
/// failed run's stdout is not a reading even when it happens to parse.
///
/// Non-UTF-8 stdout is rejected WHOLE rather than lossily repaired, because a
/// replacement character inside a path yields a directory that exists nowhere —
/// a root that silently matches nothing, or worse, the wrong thing. No signal is
/// the honest answer.
#[must_use]
fn set_from_output(success: bool, stdout: &[u8]) -> WorktreeSet {
    if !success {
        return WorktreeSet::empty(); // Non-zero exit -> treat as "no signal".
    }
    let Ok(text) = std::str::from_utf8(stdout) else {
        return WorktreeSet::empty();
    };
    parse_porcelain(text, resolve_dir)
}

/// Read the live worktree set of the project containing `launch_dir`.
///
/// The ONE impure step: it owns the spawn alone, and what the result means is
/// [`set_from_output`]'s pure decision. Output is CAPTURED (stdout and stderr
/// both into pipes, stdin null) so git can never write to the terminal the board
/// is drawn on, and never blocks reading from it.
///
/// Never panics: a missing binary or any other spawn failure returns
/// [`WorktreeSet::empty`], as does every other failure mode.
///
/// MUST stay a bounded one-shot (launch and reload), never a render-loop or
/// per-keystroke call — it blocks on a child process.
// Its only non-test caller is the TUI's worktree probe, which is
// `#[cfg(not(test))]`-gated so the suite never spawns git; that leaves this with
// zero callers under the `lib test` target alone, hence `dead_code` allowed
// narrowly here (rather than module- or crate-wide), as `agents::live_agents`
// does for the same reason.
#[allow(dead_code)]
#[must_use]
pub fn resolve(launch_dir: &Path) -> WorktreeSet {
    let argv = git_worktree_argv(launch_dir);
    let output = match Command::new(&argv[0]).args(&argv[1..]).output() {
        Ok(output) => output,
        Err(_) => return WorktreeSet::empty(), // `git` not on PATH, spawn failed, etc.
    };
    set_from_output(output.status.success(), &output.stdout)
}

/// Parse `git worktree list --porcelain` output into a [`WorktreeSet`].
///
/// Pure — the impure half is [`resolve`] — and fail-soft in the same way the
/// JSONL reader is: it reads the ONE line shape it understands
/// ([`WORKTREE_LINE_PREFIX`]) and skips everything else, so a record with no
/// `worktree` line, an unknown attribute, a stray blank line, or output that is
/// not porcelain at all yields fewer roots (possibly none) and never an error.
///
/// `canonicalize` normalizes each path — pass [`resolve_dir`], the same one the
/// scope predicate uses, so membership compares like-for-like. It is a parameter
/// rather than a hardwired call so the parse stays hermetic under test (no
/// filesystem, no git). The LABEL comes from the FIRST record: git lists the
/// main worktree first, and the main worktree is what names the project.
#[must_use]
pub fn parse_porcelain(text: &str, canonicalize: impl Fn(&Path) -> PathBuf) -> WorktreeSet {
    let mut roots = HashSet::new();
    let mut label = None;

    for line in text.lines() {
        let Some(raw) = line.strip_prefix(WORKTREE_LINE_PREFIX) else {
            continue; // Any other attribute, a blank record separator, or noise.
        };
        // Tolerate a stray `\r` (CRLF) or trailing space rather than turning it
        // into a path that matches nothing.
        let raw = raw.trim_end();
        if raw.is_empty() {
            continue; // `worktree ` with no path -> unusable record, skip.
        }

        let root = canonicalize(Path::new(raw));
        // First record wins: git lists the MAIN worktree first, and it is what
        // names the project.
        if label.is_none() {
            label = Some(group::repo_of(&root));
        }
        roots.insert(root);
    }

    WorktreeSet::from_resolved(roots, label)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `git worktree list --porcelain` output, captured from this repo (a
    /// main worktree plus siblings under `.agents/worktrees/`), so the parser is
    /// pinned against git's actual bytes rather than a guess at them.
    ///
    /// The third record is deliberately mutilated: it carries a `detached` line
    /// and NO `worktree` line, which is what a future/unknown record shape looks
    /// like to this parser.
    const SAMPLE: &str = "\
worktree /Volumes/Development/ilfroloff/snapback
HEAD 24505baa75d87a6a4deb40fc1e6d0fef409457b0
branch refs/heads/main

worktree /Volumes/Development/ilfroloff/snapback/.agents/worktrees/cross-worktree-project-scope
HEAD 24505baa75d87a6a4deb40fc1e6d0fef409457b0
branch refs/heads/cross-worktree-project-scope

HEAD f60190a2f5d7518b479816918ec3b20a31dd84b0
detached

worktree /Volumes/Development/ilfroloff/snapback/.agents/worktrees/feature/quick-send-reply-to-session
HEAD f60190a2f5d7518b479816918ec3b20a31dd84b0
branch refs/heads/feature/quick-send-reply-to-session
";

    const MAIN: &str = "/Volumes/Development/ilfroloff/snapback";
    const WT_SCOPE: &str =
        "/Volumes/Development/ilfroloff/snapback/.agents/worktrees/cross-worktree-project-scope";
    const WT_REPLY: &str = "/Volumes/Development/ilfroloff/snapback/.agents/worktrees/feature/quick-send-reply-to-session";

    /// The identity "canonicalizer": keeps every parse test hermetic — no
    /// filesystem, no git, so the sample's paths need not exist anywhere.
    fn as_is(p: &Path) -> PathBuf {
        p.to_path_buf()
    }

    /// The whole point of the module: every worktree of one project lands in one
    /// set, and the project is named after the MAIN worktree (git lists it
    /// first), not after whichever worktree happened to launch snapback.
    #[test]
    fn multi_worktree_porcelain_parses_every_root_and_labels_from_the_main_worktree() {
        let set = parse_porcelain(SAMPLE, as_is);

        assert_eq!(
            set,
            WorktreeSet::from_resolved(
                [
                    PathBuf::from(MAIN),
                    PathBuf::from(WT_SCOPE),
                    PathBuf::from(WT_REPLY),
                ],
                Some("snapback".to_string()),
            ),
            "every `worktree <path>` line is a root of the same project"
        );
        assert!(
            set.contains(Path::new(WT_SCOPE)),
            "a sibling worktree is in"
        );
        assert!(
            !set.contains(Path::new("/Volumes/Development/ilfroloff/other-project")),
            "an unrelated repo must never be in the project's set"
        );
        assert_eq!(
            set.label(),
            Some("snapback"),
            "the label names the project, i.e. the main worktree's repo label"
        );
        assert!(!set.is_empty(), "a parsed set carries signal");
    }

    /// A record with no `worktree` line contributes nothing and does not derail
    /// the records around it — the fail-soft rule the JSONL reader lives by,
    /// applied to git's format: skip what you do not understand, keep going.
    #[test]
    fn a_record_without_a_worktree_line_is_skipped() {
        let set = parse_porcelain(SAMPLE, as_is);

        assert!(
            !set.contains(Path::new("f60190a2f5d7518b479816918ec3b20a31dd84b0")),
            "a `HEAD`/`detached` record has no path, so it adds no root"
        );
        assert!(
            set.contains(Path::new(WT_REPLY)),
            "the record AFTER the skipped one must still parse"
        );
    }

    /// Nothing parseable is not an error, it is the documented "could not
    /// resolve" answer — including the near-misses: a bare `worktree` key with
    /// no path, and a path-less `worktree ` line.
    #[test]
    fn malformed_or_empty_input_yields_an_empty_set() {
        for input in [
            "",
            "\n\n\n",
            "not porcelain at all\n",
            "worktree\n",
            "worktree \n",
        ] {
            let set = parse_porcelain(input, as_is);
            assert_eq!(
                set,
                WorktreeSet::empty(),
                "unparseable input must degrade to the empty set, not a root: {input:?}"
            );
            assert!(set.is_empty());
            assert_eq!(set.label(), None, "no main worktree -> no project label");
        }
    }

    /// The invocation is the contract with git. Pinned here so a rename, a
    /// dropped `--porcelain` (which would hand the parser a human table), or a
    /// lost `-C` (which would answer about the WRONG directory) fails loudly
    /// without anyone running git.
    #[test]
    fn argv_is_git_dash_c_launch_dir_worktree_list_porcelain() {
        let expected: Vec<OsString> = [
            "git",
            "-C",
            "/Users/me/acme/web",
            "worktree",
            "list",
            "--porcelain",
        ]
        .iter()
        .map(OsString::from)
        .collect();

        assert_eq!(git_worktree_argv(Path::new("/Users/me/acme/web")), expected);
    }

    /// A failed git run is NO SIGNAL, even when its stdout would parse: the
    /// status is checked first, so a partial dump from a crashed git can never
    /// become a project's worktree set.
    #[test]
    fn a_non_zero_git_exit_is_no_signal() {
        assert_eq!(
            set_from_output(false, SAMPLE.as_bytes()),
            WorktreeSet::empty()
        );
    }

    /// Non-UTF-8 stdout is rejected whole rather than lossily repaired — a
    /// replacement character inside a path is a directory that exists nowhere.
    ///
    /// The fixture is an OTHERWISE VALID record carrying one bad byte, which is
    /// the only shape that can tell the two readings apart: repaired lossily it
    /// would parse into a plausible-looking root that matches no session.
    #[test]
    fn non_utf8_git_output_is_no_signal() {
        assert_eq!(
            set_from_output(true, b"worktree /Users/me/acme/web\xff\n"),
            WorktreeSet::empty()
        );
    }

    // --- project root (the path-derived project identity) ------------------

    /// A unique scratch dir, never the real store — the suite's temp-dir
    /// convention.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the unix epoch")
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!("snapback-wt-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// THE CANONICALIZATION-ORDER CASE, and the reason [`project_root`] exists
    /// as its own function rather than as `repo_root_of(resolve_dir(p))` inline.
    ///
    /// The fixture is the shape this whole widening is for: a repo reached
    /// through a SYMLINKED prefix, whose worktree LEAF has been deleted. The leaf
    /// cannot canonicalize, so canonicalizing the whole path first leaves the
    /// symlinked prefix intact — and the deleted worktree then compares unequal
    /// to its own repo, which resolved. Deriving the prefix first and
    /// canonicalizing THAT resolves the symlink on both sides.
    #[cfg(unix)]
    #[test]
    fn a_deleted_worktree_behind_a_symlink_still_resolves_to_its_repo_root() {
        let base = unique_temp_dir("symlink-prefix");
        let real = base.join("real");
        let repo = real.join("acme-repo");
        std::fs::create_dir_all(&repo).expect("create the repo dir");
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).expect("create the symlink");

        // The worktree's leaf is GONE (it was never created); its path is spelled
        // through the symlink, exactly as a recorded `cwd` would be.
        let deleted_worktree = link.join("acme-repo/.wtp/worktrees/feature/gone");
        assert!(
            !deleted_worktree.exists(),
            "premise: the worktree leaf does not exist, so it cannot canonicalize"
        );

        assert_eq!(
            project_root(&deleted_worktree),
            project_root(&repo),
            "the deleted worktree and its repo must resolve to ONE root"
        );
        assert_eq!(
            project_root(&deleted_worktree),
            resolve_dir(&repo),
            "and that root is the repo's own resolved path, symlink collapsed"
        );
        assert_ne!(
            group::repo_root_of(&resolve_dir(&deleted_worktree)),
            project_root(&repo),
            "premise: the WRONG order (canonicalize, then derive) leaves the \
             symlinked prefix in place and answers `different project`"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// The name a project takes when git resolved no label: the REPO ROOT's,
    /// never the worktree's own — a worktree directory is named after its
    /// BRANCH, and heading a whole project's list with a branch name misdescribes
    /// it. A plain checkout is its own root, so the two answers coincide there.
    #[test]
    fn a_project_is_named_after_its_repo_root_not_the_worktree_launched_from() {
        assert_eq!(
            project_root_name(Path::new(
                "/Volumes/Development/ilfroloff/snapback/.agents/worktrees/feature/quick-send"
            )),
            "snapback",
            "the launch dir is called `quick-send`; the PROJECT is `snapback`"
        );
        assert_eq!(
            project_root_name(Path::new("/Volumes/Development/ilfroloff/snapback")),
            "snapback",
            "a plain checkout is its own root, so nothing changes there"
        );
        assert_eq!(
            project_root_name(Path::new("/")),
            "/",
            "no final component -> named by the whole path"
        );
    }
}
