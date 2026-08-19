//! Command-line argument parsing.
//!
//! Parses the launch flags for `snapback` (short alias `sb`):
//! - `--all` / `-a`: show every session grouped by folder (instead of the
//!   default current-folder scope), AND make that scope the third stop of the
//!   `Ctrl-A` cycle — it is reachable no other way.
//! - `--project` / `-p`: show every session from this project — the launch
//!   repo and all of its git worktrees — grouped by branch under one project
//!   head.
//! - `--model <value>`: pre-arm the sticky model override `Ctrl-X m` otherwise
//!   sets, so an alias or a shell function can launch straight into it. The
//!   value is taken RAW and never validated here.
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
    --model <name> Start with the model override already set to <name> — the
                   same sticky pick Ctrl-X m makes, so everything this run
                   resumes, forks, starts or sends asks for that model until you
                   change it in the board. Passed to claude as-is and never
                   checked here, so an alias or a full model id both work and an
                   unknown one is claude's to reject
    -h, --help     Print this help and exit

KEYS:
    ↑/↓           move          Enter        resume (returns to the board on exit)
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
                  · h show/hide hidden · m model · r re-read every transcript
                  from disk (the board already autorefreshes and reuses unchanged
                  files; r is the force, for a row that looks stale)
    Ctrl-X m      pick the model every later hand-off asks for — resume, fork,
                  new session, quick reply and background launch alike. The pick
                  sticks until you change it and shows in the header as
                  'model: <alias>'; the first row clears it, so your claude
                  settings decide again and no --model flag is sent. It is held
                  in memory only, never written to disk, so it is forgotten on
                  restart — and it beats an agent definition's own model: field.
                  The rows are the aliases the installed claude accepts, each
                  carrying its own note where one helps; a value claude accepts
                  but the picker does not offer goes in with --model at launch
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
    Home/End      preview top / bottom (also Ctrl-T/Ctrl-E; fn+←/→ on a MacBook
                  keyboard, where Home/End are not their own keys)
    Shift+↑/↓     jump the preview to the previous / next line the query marks.
                  Only while something is marked there — with nothing marked they
                  stay plain move, so they never take a key away from you. One
                  stop per marked line, not per repeated word
    wheel         scroll preview / list (mouse mode on; hold Shift/Option to
                  select). While a compose or draft box is open the list stops
                  taking notches — a wheel over it does nothing, so the session
                  you are writing to cannot slide away under the pointer.
                  Anywhere else the wheel still scrolls the transcript
    Backspace     delete the last query character
    Alt+Backspace delete the last query WORD — one whole search atom, so a
                  path or a branch name goes in one press. Ctrl-W and Alt+H do
                  the same, so it works whatever your terminal sends for
                  Option, and it is the same set the reply box word-deletes on
    paste         your terminal's own paste (Cmd/Ctrl-V, middle-click) is inserted
                  as TEXT: into a compose draft at the cursor, newlines and all, or
                  appended to the search query with newlines flattened to spaces.
                  It never sends, resumes, or answers a confirmation
    Esc           quit          (type to search)

BACK TO THE BOARD (typed inside a resumed Claude session, not a snapback key):
    /bg           detach the session — it keeps running as a bg agent — and snap
                  back to the board; /exit ends it. Prefer these over Ctrl-Z, which
                  only detaches cleanly when you're attached to a background agent.";

/// Parsed launch options.
///
/// Not `Copy` since [`model`](Self::model) owns its string; nothing consumes the
/// whole struct by value, so the fields are read (and the model moved) one by one
/// in `lib::run`.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// The `--model <value>` launch override, or `None` when the flag was absent
    /// (or carried no value). Seeds `App::model_override` — the SAME sticky pick
    /// `Ctrl-X m` writes, through the same setter — so a shell alias can start the
    /// board already aimed at a model, and the picker can still change or clear it.
    ///
    /// RAW and UNVALIDATED on purpose, exactly like the picker's value: `--model`
    /// takes a full model id as readily as an alias, and snapback holds no list it
    /// could check one against without going stale on the next `claude` release. An
    /// unknown value reaches claude and is refused there, which
    /// `resume::MODEL_NONZERO_HINT` is already worded for.
    ///
    /// A BLANK value is the one exception, and it is handled a step later rather
    /// than here: `--model ""` parses to `Some("")`, which `App::set_model_override`
    /// normalizes to no override at all. That keeps the guard on the ONE setter both
    /// doors write through, and gives a present-but-empty value the same answer this
    /// parser already gives a trailing `--model` with nothing after it.
    pub model: Option<String>,
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
///
/// `--model` is the one flag that takes a VALUE, so the walk is an explicit
/// iterator rather than a `for`: the flag consumes the NEXT argument verbatim,
/// getopt-style, with no `--model=<value>` form and no inspection of what it
/// swallowed (a value is claude's to judge, per [`Args::model`]). A trailing
/// `--model` with nothing after it takes `None` and is otherwise ignored — the
/// same forgiving posture as the unknown-flag arm, and not an error, because this
/// tool has no error channel before the terminal is even set up. Repeats follow
/// the scope flags' LAST ONE WINS rule, including a valueless last one, which is
/// the plain assignment below.
pub fn parse_from<I, S>(args: I) -> Args
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut scope = Scope::CurrentFolder;
    let mut all_scope_enabled = false;
    let mut print_list = false;
    let mut model = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_ref() {
            "--all" | "-a" => {
                scope = Scope::All;
                all_scope_enabled = true;
            }
            "--project" | "-p" => scope = Scope::Project,
            "--model" => model = args.next().map(|v| v.as_ref().to_string()),
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
        model,
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

    use crate::tui::App;

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

    /// The one flag that takes a VALUE, in the three shapes a command line can
    /// hand it over: absent, present with a value, and present with nothing after
    /// it.
    ///
    /// The value is asserted VERBATIM, because not validating it is the design: a
    /// full model id must survive as readily as an alias, and an unknown one has to
    /// reach `claude` to be refused there (`resume::MODEL_NONZERO_HINT` is worded
    /// for exactly that). A test that only accepted known aliases would pin the
    /// opposite rule.
    #[test]
    fn the_model_flag_takes_the_next_argument_raw() {
        assert_eq!(parse_from(Vec::<String>::new()).model, None);
        assert_eq!(
            parse_from(["--model", "opus"]).model,
            Some("opus".to_string())
        );
        // A full model id, not an alias — nothing here narrows the value.
        assert_eq!(
            parse_from(["--model", "claude-sonnet-5"]).model,
            Some("claude-sonnet-5".to_string())
        );
        // A trailing flag with nothing after it asks for no override rather than
        // erroring: the same forgiving posture the unknown-flag arm takes, and
        // there is no error channel before the terminal is even set up.
        assert_eq!(parse_from(["--model"]).model, None);
        // Orthogonal to scope, exactly as the other flags are, and the flag AFTER
        // it is still parsed — the value consumed one argument, not the rest.
        let with_scope = parse_from(["--model", "haiku", "--all"]);
        assert_eq!(with_scope.model, Some("haiku".to_string()));
        assert_eq!(with_scope.scope, Scope::All);
        // Repeats follow the scope flags' LAST ONE WINS rule.
        assert_eq!(
            parse_from(["--model", "opus", "--model", "sonnet"]).model,
            Some("sonnet".to_string())
        );
    }

    /// The flag's whole point: it lands on the SAME sticky override `Ctrl-X m`
    /// writes, through the same setter, so a shell alias starts the board already
    /// aimed at a model.
    ///
    /// Asserted through `App` rather than on `Args` alone, because a parsed field
    /// nothing reads would satisfy the parse test above while changing nothing a
    /// user could see.
    ///
    /// What this does NOT reach is the one-line call in `lib::run` that joins the
    /// two, and that gap is ACCEPTED rather than papered over: `run` sets up a
    /// terminal and spawns children, so it has no test here at all — its sibling
    /// wiring (`app.all_scope_enabled = args.all_scope_enabled`) is untested for the
    /// same reason. Both halves of the seam are pinned; the assignment between them
    /// is read, not asserted.
    #[test]
    fn the_model_flag_round_trips_into_the_boards_override() {
        let mut app = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp/launch"));
        app.set_model_override(parse_from(["--model", "opusplan"]).model);
        assert_eq!(app.model_override.as_deref(), Some("opusplan"));

        // And no flag leaves the board exactly as it launches today: no override,
        // so no `--model` token reaches any argv.
        let mut bare = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp/launch"));
        bare.set_model_override(parse_from(Vec::<String>::new()).model);
        assert_eq!(bare.model_override, None);
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
