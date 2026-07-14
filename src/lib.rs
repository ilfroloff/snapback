//! `snapback` — Claude session launcher (short alias `sb`).
//!
//! A ratatui TUI that browses, searches, and resumes Claude Code sessions
//! stored as JSONL under `~/.claude/projects/`.
//!
//! Architecture (data-core-first): the framework-independent data layer
//! (`store`) is unit-tested before any TUI code lands. The `tui` module is the
//! elm-style shell; `watch` feeds autorefresh events; `search` isolates nucleo;
//! `resume` spawns `claude` as a child and RETURNS so `snapback` stays a
//! persistent dashboard (quitting a resumed session drops you back onto the
//! board).
//!
//! This is a personal tool; see `README.md`. The expensive module tree lives
//! here in the library crate so it compiles once, and the `snapback` and `sb`
//! binaries are thin shims that both call [`run`].

mod agents;
mod cli;
mod resume;
mod search;
mod store;
mod tui;
mod watch;

use std::io::IsTerminal;

use store::SessionStore;
use tui::{App, Outcome};

/// Program entry point shared by both the `snapback` and `sb` binaries.
///
/// The two binary shims call straight into this function, so the CLI behaves
/// identically no matter which name launched it (no `argv[0]` dispatch).
pub fn run() {
    let args = cli::parse();

    // Hidden `--print-list` mode: dump the data core non-interactively and
    // exit, never touching the terminal or `claude`.
    if args.print_list {
        print_session_list();
        return;
    }

    let launch_dir = cli::launch_dir();
    let root = store::discover::store_root();
    let sessions = SessionStore::load_from(&root);

    // Guard against a non-TTY / headless environment: a ratatui TUI needs a
    // real terminal, so instead of entering raw mode (which would fail) print a
    // clear message and exit cleanly rather than panicking.
    if !std::io::stdout().is_terminal() {
        eprintln!(
            "snapback: stdout is not a terminal — the interactive UI needs a TTY. \
             Run snapback directly in an interactive terminal."
        );
        eprintln!(
            "snapback: {} session(s) available under {}",
            sessions.len(),
            root.display()
        );
        return;
    }

    // `snapback` is a persistent dashboard: it OWNS the `App` across resume round
    // trips so selection/query/scope/scroll survive, and only exits when the
    // user quits `snapback` itself (`q` / `Esc` / `Ctrl-C`).
    let mut app = App::new(sessions, args.scope, launch_dir);
    loop {
        match tui::run(&mut app, &root) {
            // User quit the board (or all event senders dropped).
            Ok(Outcome::Quit) => break,
            Ok(Outcome::Resume(ready)) => {
                // `tui::run` already tore the terminal down. Spawn `claude` as a
                // CHILD inheriting this TTY (the refusal gate ran while the UI
                // was still up, so `ready` is confirmed), wait for it to exit,
                // then reload the store and loop — `tui::run` re-initializes AND
                // hard-resets the terminal on the next iteration (clears the alt
                // screen + disables input modes the child may have leaked), so a
                // child that left the terminal dirty — notably one suspended with
                // Ctrl-Z — repaints from a known-good state whatever its exit. A
                // non-zero child exit becomes a neutral transient board status; a
                // spawn/chdir failure likewise, rather than a crash.
                match resume::launch(&ready) {
                    Ok(Some(status)) => app.set_status(status),
                    Ok(None) => {}
                    Err(err) => app.set_status(err.message().to_string()),
                }
                app.apply_sessions(SessionStore::load_from(&root));
            }
            // `run` only breaks its own loop on Quit/Resume; Continue never
            // escapes, but treat it as a clean exit for totality.
            Ok(Outcome::Continue) => break,
            Err(err) => {
                // `tui::run` restores the terminal on every exit once it is live
                // — including this error path — so it is already out of raw mode
                // and the alternate screen by the time we print and exit.
                eprintln!("snapback: {err:#}");
                std::process::exit(1);
            }
        }
    }
}

/// Hidden `--print-list` mode: a non-interactive session dump for debugging.
///
/// Loads the store non-interactively — no terminal, no `claude` — and prints one
/// line per resumable session (`session_id\trepo\tbranch\tcwd`) followed by a
/// repo->branch group breakdown and a total, so the discovered resumable set can
/// be inspected from a script. Meta lines are prefixed with `#`; session rows
/// are not, so `grep -vc '^#'` counts them. It deliberately does NOT start the
/// TUI.
fn print_session_list() {
    // `SessionStore::load()` resolves the default root itself
    // ($CLAUDE_PROJECTS_DIR or ~/.claude/projects) — the same root the
    // interactive path discovers — and applies the identical subagent-excluding,
    // fail-soft, no-`cwd`-dropping pipeline.
    let sessions = SessionStore::load();
    let root = store::discover::store_root();

    println!("# store: {}", root.display());
    println!("# columns: session_id\trepo\tbranch\tcwd");
    for s in &sessions {
        println!(
            "{}\t{}\t{}\t{}",
            s.session_id,
            s.repo,
            s.branch_display(),
            s.cwd.display()
        );
    }

    // Repo -> branch group breakdown. Sessions arrive sorted
    // repo->branch->timestamp, so each contiguous run is one group.
    let mut groups: Vec<(String, String, usize)> = Vec::new();
    for s in &sessions {
        let branch = s.branch_display().to_string();
        match groups.last_mut() {
            Some((repo, br, n)) if *repo == s.repo && *br == branch => *n += 1,
            _ => groups.push((s.repo.clone(), branch, 1)),
        }
    }
    println!("# groups (repo / branch: count): {}", groups.len());
    for (repo, branch, n) in &groups {
        println!("#   {repo} / {branch}: {n}");
    }

    println!("# total resumable sessions: {}", sessions.len());
}
