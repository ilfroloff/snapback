//! Command-line argument parsing.
//!
//! Parses the launch flags for `snapback` (short alias `sb`):
//! - `--all` / `-a`: show every session grouped by folder (instead of the
//!   default current-folder scope), AND make that scope the third stop of the
//!   `Ctrl-A` cycle — it is reachable no other way.
//! - `--project` / `-p`: show every session from this project — the launch
//!   repo and all of its git worktrees — grouped by branch under one project
//!   head.
//! - `--help` / `-h`: print usage and exit.
//!
//! Also resolves the launch directory (`std::env::current_dir`) used by the
//! default folder-scoping predicate in `tui::update` (Task 5.4).

use std::path::PathBuf;

use crate::tui::Scope;

/// One-line usage banner.
const USAGE: &str = "\
snapback — browse, search, and resume Claude Code sessions
(short alias: sb — an installed binary that runs the same program)

USAGE:
    snapback [OPTIONS]
    sb [OPTIONS]

OPTIONS:
    -a, --all      Show every session grouped by folder, and make that scope the
                   third stop of the Ctrl-A cycle — without this flag Ctrl-A
                   flips between the current folder and the project (default:
                   only the current folder's sessions)
    -p, --project  Show every session from this project — the repo you launched
                   in and all of its git worktrees — grouped by branch under one
                   project head
    -h, --help     Print this help and exit

KEYS:
    ↑/↓, j/k      move          Enter        resume (returns to the board on exit)
    ←/→           fold / expand a fork lineage (a row marked (+N) stands for more)
    Ctrl-F        fork          Ctrl-/       toggle preview
    Ctrl-A        flip scope: current folder ↔ project (the repo you launched in
                  and all of its git worktrees). Launched with -a it is a
                  three-stop cycle instead: current folder → project → all
                  folders
    Ctrl-N        new session in the launch dir: pick an agent when any are
                  defined, then draft the session's first message — Enter starts
                  it as a BACKGROUND agent without leaving the board, Ctrl-O runs
                  it interactively instead, Ctrl-J or Alt+Enter newline, Esc
                  cancels. The message is sent as the first turn either way
    Ctrl-O        in the agent picker: start that agent interactively at once,
                  skipping the draft — the same verb Ctrl-O has inside the draft
    Ctrl-X        leader chord: x hide · d delete (this row or its lineage)
                  · h show/hide hidden · r re-read every transcript from disk
                  (the board already autorefreshes and reuses unchanged files;
                  r is the force, for a row that looks stale)
    Ctrl-R        quick reply — send a one-shot message to the selected session
                  without leaving the board. An agent whose run is over (done,
                  stopped, failed) is stopped first so the reply lands in place;
                  a waiting one (needs input) confirms first; a working, idle,
                  interrupted or unrecognized agent is refused — Attach or Fork
                  instead (Enter sends, Ctrl-J or Alt+Enter newline, Esc cancels)
    Ctrl-K        stop / interrupt the selected session's live background agent
                  (claude stop); an agent whose run is over (done, stopped,
                  failed) stops at once, every other live agent confirms first;
                  a session claude isn't holding, or one running interactively,
                  has no job to stop (Enter stops, Esc cancels)
    Tab           toggle name / name+content search. Widening to content also
                  opens the preview on the most recent match, as typing does
    PgUp/PgDn     preview page  Ctrl-U/Ctrl-D  preview quarter-page
    Home/End      preview top / bottom
    Shift+↑/↓     jump the preview to the previous / next line the query marks.
                  Only while something is marked there — with nothing marked they
                  stay plain move, so they never take a key away from you. One
                  stop per marked line, not per repeated word
    wheel         scroll preview / list (mouse mode on; hold Shift/Option to select)
    paste         your terminal's own paste (Cmd/Ctrl-V, middle-click) is inserted
                  as TEXT: into a compose draft at the cursor, newlines and all, or
                  appended to the search query with newlines flattened to spaces.
                  It never sends, resumes, or answers a confirmation
    q, Esc        quit          (type to search)

BACK TO THE BOARD (typed inside a resumed Claude session, not a snapback key):
    /bg           detach the session — it keeps running as a bg agent — and snap
                  back to the board; /exit ends it. Prefer these over Ctrl-Z, which
                  only detaches cleanly when you're attached to a background agent.";

/// Parsed launch options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Args {
    /// The initial scope: current-folder unless `--all`/`-a` (every folder) or
    /// `--project`/`-p` (this project's git worktrees) was given.
    pub scope: Scope,
    /// Whether [`Scope::All`] exists for this run at all.
    ///
    /// `--all`/`-a` means TWO things, which is why this cannot be read off
    /// [`scope`](Self::scope): it starts the board wide AND it keeps the all
    /// scope as the third stop of the `Ctrl-A` cycle. Without the flag that key
    /// is a two-state flip (current folder <-> project) and the whole store is
    /// unreachable from inside the board — deliberately, because it is the
    /// widest, least-often-wanted answer and it used to sit mid-cycle where a
    /// stray keypress landed on it.
    ///
    /// The two meanings come apart whenever a trailing `-p` wins the initial
    /// scope; see [`parse_from`] for that precedence rule.
    pub all_scope_enabled: bool,
    /// Hidden non-interactive dump mode (`--print-list`): load the store and
    /// print one line per resumable session plus counts/grouping, WITHOUT
    /// starting the TUI. Deliberately omitted from [`USAGE`] — it is a
    /// debug/scripting aid for inspecting what the data core discovers.
    pub print_list: bool,
}

/// Parse `std::env::args`. Exits the process (code 0) on `--help`/`-h`.
#[must_use]
pub fn parse() -> Args {
    parse_from(std::env::args().skip(1))
}

/// Parse an explicit argument iterator (testable; no process access).
///
/// `--help`/`-h` prints usage and exits; every other unrecognized flag is
/// ignored (this is a personal tool with a tiny surface).
///
/// The scope flags are mutually exclusive in meaning but not in syntax: the
/// LAST one on the command line wins, which is the plain single-pass
/// assignment below and what a repeated option does everywhere else, so a
/// shell alias carrying `-p` stays overridable by a trailing `-a`.
///
/// [`Args::all_scope_enabled`] is ORTHOGONAL to that rule and does not
/// participate in it: `-a`/`--all` seen ANYWHERE enables the all scope's cycle
/// stop, even where a trailing `-p` takes the initial scope away from it. So
/// `sb -a -p` starts in the project scope and can still reach all folders,
/// while `sb -p` starts in the same place and cannot. The flag is never
/// unset — asking for a scope cannot be undone by then asking to start in a
/// different one.
pub fn parse_from<I, S>(args: I) -> Args
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut scope = Scope::CurrentFolder;
    let mut all_scope_enabled = false;
    let mut print_list = false;
    for arg in args {
        match arg.as_ref() {
            "--all" | "-a" => {
                scope = Scope::All;
                all_scope_enabled = true;
            }
            "--project" | "-p" => scope = Scope::Project,
            // Hidden debug/dump flag (not advertised in USAGE); see `Args::print_list`.
            "--print-list" => print_list = true,
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            _ => {}
        }
    }
    Args {
        scope,
        all_scope_enabled,
        print_list,
    }
}

/// The canonicalized launch directory, used by the current-folder scope
/// predicate. Falls back to the raw cwd (then `.`) if canonicalization fails.
#[must_use]
pub fn launch_dir() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    std::fs::canonicalize(&cwd).unwrap_or(cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_current_folder_scope() {
        let args = parse_from(Vec::<String>::new());
        assert_eq!(args.scope, Scope::CurrentFolder);
    }

    #[test]
    fn all_flag_selects_all_scope() {
        assert_eq!(parse_from(["--all"]).scope, Scope::All);
        assert_eq!(parse_from(["-a"]).scope, Scope::All);
    }

    #[test]
    fn project_flag_selects_project_scope() {
        assert_eq!(parse_from(["--project"]).scope, Scope::Project);
        assert_eq!(parse_from(["-p"]).scope, Scope::Project);
        // Orthogonal to the hidden dump flag, exactly as `--all` is: the scope
        // says WHICH sessions, `--print-list` says WHERE they are printed.
        let both = parse_from(["--project", "--print-list"]);
        assert_eq!(both.scope, Scope::Project);
        assert!(both.print_list);
    }

    /// Two scope flags on one command line is a contradiction, and the rule is
    /// LAST ONE WINS — the plain single-pass assignment, and what a shell user
    /// expects from a repeated option (an alias appending `-p` stays
    /// overridable by a trailing `-a`). Pinned so nobody "fixes" the loop into
    /// a widest-wins or first-wins precedence by accident.
    #[test]
    fn the_last_scope_flag_wins() {
        assert_eq!(parse_from(["--all", "--project"]).scope, Scope::Project);
        assert_eq!(parse_from(["--project", "--all"]).scope, Scope::All);
    }

    /// The OTHER half of `-a`, and the half the last-flag-wins rule above does
    /// NOT decide: the flag also says the all scope exists as a `Ctrl-A` stop,
    /// and that half survives a trailing `-p` taking the initial scope away.
    ///
    /// Both directions are pinned, because the interesting case is the one
    /// where the two meanings disagree: `-a -p` starts in the project scope and
    /// can still cycle to all folders, while a bare `-p` starts in exactly the
    /// same scope and cannot.
    #[test]
    fn the_all_flag_enables_the_cycle_stop_even_when_project_wins_the_scope() {
        let both = parse_from(["-a", "-p"]);
        assert_eq!(
            both.scope,
            Scope::Project,
            "the trailing flag still decides where the board STARTS"
        );
        assert!(
            both.all_scope_enabled,
            "but `-a` was asked for, so the all scope stays reachable by key"
        );

        let project_only = parse_from(["-p"]);
        assert_eq!(project_only.scope, Scope::Project);
        assert!(
            !project_only.all_scope_enabled,
            "the same starting scope WITHOUT `-a` leaves the whole store \
             unreachable from the board"
        );

        assert!(parse_from(["--project", "--all"]).all_scope_enabled);
        assert!(!parse_from(Vec::<String>::new()).all_scope_enabled);
        assert!(parse_from(["--all"]).all_scope_enabled);
    }

    #[test]
    fn unknown_flags_are_ignored() {
        let args = parse_from(["--wat", "positional"]);
        assert_eq!(args.scope, Scope::CurrentFolder);
    }

    #[test]
    fn print_list_flag_defaults_off_and_sets_on() {
        assert!(!parse_from(Vec::<String>::new()).print_list);
        assert!(parse_from(["--print-list"]).print_list);
        // The hidden flag is orthogonal to scope.
        let both = parse_from(["--all", "--print-list"]);
        assert_eq!(both.scope, Scope::All);
        assert!(both.print_list);
    }
}
