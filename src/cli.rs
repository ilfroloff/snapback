//! Command-line argument parsing.
//!
//! Parses the launch flags for `snapback` (short alias `sb`):
//! - `--all` / `-a`: show every session grouped by folder (instead of the
//!   default current-folder scope).
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
    -a, --all     Show every session grouped by folder (default: only the
                  current folder's sessions)
    -h, --help    Print this help and exit

KEYS:
    ↑/↓, j/k      move          Enter        resume (returns to the board on exit)
    Ctrl-F        fork          Tab          toggle name / name+content search
    Ctrl-A        toggle scope  Ctrl-/       toggle preview
    PgUp/PgDn     preview page  Ctrl-U/Ctrl-D  preview quarter-page
    Home/End      preview top / bottom
    wheel         scroll preview / list (mouse mode on; hold Shift/Option to select)
    q, Esc        quit          (type to search)";

/// Parsed launch options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Args {
    /// The initial scope (current-folder unless `--all`/`-a` was given).
    pub scope: Scope,
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
pub fn parse_from<I, S>(args: I) -> Args
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut scope = Scope::CurrentFolder;
    let mut print_list = false;
    for arg in args {
        match arg.as_ref() {
            "--all" | "-a" => scope = Scope::All,
            // Hidden debug/dump flag (not advertised in USAGE); see `Args::print_list`.
            "--print-list" => print_list = true,
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            _ => {}
        }
    }
    Args { scope, print_list }
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
