//! `snapback` — Claude session launcher (short alias `sb`).
//!
//! A ratatui TUI that browses, searches, and resumes Claude Code sessions
//! stored as JSONL under `~/.claude/projects/`.
//!
//! Architecture (data-core-first): the framework-independent data layer
//! (`store`) is unit-tested before any TUI code lands. The `tui` module is the
//! elm-style shell; `watch` feeds autorefresh events; `search` isolates nucleo;
//! `worktrees` resolves the launch project's live worktree set (the one git
//! shell-out, kept out of the pure `store` core); `resume` spawns `claude` as a
//! child and RETURNS so `snapback` stays a persistent dashboard (quitting a
//! resumed session drops you back onto the board).
//!
//! This is a personal tool; see `README.md`. The expensive module tree lives
//! here in the library crate so it compiles once, and the `snapback` and `sb`
//! binaries are thin shims that both call [`run`].

mod agents;
mod cli;
mod config;
mod defined_agents;
mod delete;
mod hidden;
mod resume;
mod search;
mod send;
mod store;
mod tui;
mod watch;
mod worktrees;

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
    // The OTHER half of `--all`/`-a` (see `cli::Args::all_scope_enabled`): the
    // scope above says where the board STARTS, this says the all scope stays on
    // the `Ctrl-A` cycle. It is board state rather than a constructor argument
    // because it filters nothing — see `App::all_scope_enabled`.
    app.all_scope_enabled = args.all_scope_enabled;
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
                    Ok(Some(status)) => after_nonzero_resume(&mut app, &ready, status),
                    Ok(None) => {}
                    Err(err) => app.set_status(err.message().to_string()),
                }
                // One call reloads BOTH halves of the board's world: the store
                // read below, and — inside `apply_sessions` — the launch
                // project's worktree set, which the child may have changed (a
                // `git worktree add` in the resumed session). No extra wiring
                // here on purpose; see `App::apply_sessions`.
                app.apply_sessions(SessionStore::load_from(&root));
            }
            // `run` only breaks its own loop on Quit/Resume; Continue never
            // escapes, and a quick-reply Send, an interrupt, and a background-agent
            // launch are all handled INSIDE `run_inner` (the board stays up, so none
            // of them propagates here) — treat them all as a clean exit for totality.
            Ok(
                Outcome::Continue | Outcome::Send(_) | Outcome::Interrupt(_) | Outcome::BgLaunch(_),
            ) => break,
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

/// Handle a child that exited NON-ZERO, recovering the TOCTOU resume race when
/// that is provably what happened.
///
/// A plain `claude -r <id>` can be refused because the session went live between
/// the gate's probe and the spawn — the window is small but real, since claude
/// re-evaluates liveness at spawn time. When that happens the generic
/// [`resume::RESUME_NONZERO_HINT`] is a guess, and the user is left to work out
/// the route themselves.
///
/// So on the plain-resume path ONLY (`Ready::race_probe_id` is `Some` — every
/// other hand-off is structurally excluded), re-ask claude. If the session IS
/// live now, say so and open the Attach/Fork overlay, which persists across the
/// loop into the next `tui::run`. If it is NOT live, the failure was something
/// else entirely and the neutral hint stands UNCHANGED — we must not claim a
/// session is running on anything but the probe's word.
///
/// The recovery is deliberately NEVER derived from claude's error TEXT: that
/// wording is undocumented and would drift, and stdout/stderr are inherited by the
/// child anyway (see [`resume::launch`] — piping them to read would hide claude's
/// output from the user and risk a deadlock). The probe is the only authority.
///
/// Runs AFTER `launch` returns, with the board torn down and no `claude` in
/// flight, so it costs the happy path nothing.
fn after_nonzero_resume(app: &mut App, ready: &resume::Ready, status: String) {
    match &ready.race_probe_id {
        Some(id) if app.is_live_now(id) => {
            app.set_status(resume::RESUME_RACE_STATUS);
            app.open_live_choice(id.clone());
        }
        // Not a plain resume, or the session genuinely is not live: keep the
        // neutral hint the plan carried and assert nothing about the cause.
        _ => app.set_status(status),
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;
    use std::path::PathBuf;

    use agents::ReportedAgent;
    use tui::Scope;

    /// An app with claude's ACTIVE list seeded to `live`.
    ///
    /// No `claude` is ever spawned: the probe is injected, exactly as at the
    /// resume gate's own tests. Recovery routes on the id carried by the `Ready`,
    /// so no loaded session is needed here.
    ///
    /// Membership is all this path asks (`is_live_now`), so the seeded records
    /// carry no attach job id — the Attach hand-off's question is pinned where it
    /// is answered, in `tui::update`.
    fn app_with_live(live: &[&str]) -> App {
        let mut app = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp"));
        let live: HashMap<String, ReportedAgent> = live
            .iter()
            .map(|id| {
                (
                    (*id).to_string(),
                    ReportedAgent {
                        kind: "interactive".to_string(),
                        id: None,
                        state: None,
                        status: None,
                        name: None,
                    },
                )
            })
            .collect();
        app.set_live_probe(move || live.clone());
        app
    }

    /// A plain-resume `Ready` — the ONE hand-off that can lose the liveness race,
    /// and so the only one carrying a `race_probe_id`.
    fn plain_resume_ready(session_id: &str) -> resume::Ready {
        resume::Ready {
            cwd: PathBuf::from("/tmp"),
            argv: resume::build_argv(session_id, false),
            nonzero_hint: resume::RESUME_NONZERO_HINT,
            race_probe_id: Some(session_id.to_string()),
        }
    }

    /// THE race, recovered after the fact. A plain resume exited non-zero and a
    /// FRESH probe says claude is holding the session — so the refusal is
    /// explained, and the user is routed to the way through (Attach/Fork) instead
    /// of being handed a generic guess.
    ///
    /// The overlay is the observable part that matters: it lives on `App`, which
    /// the driver owns across the loop, so it survives into the next `tui::run`.
    #[test]
    fn a_nonzero_plain_resume_on_a_now_live_session_routes_to_the_overlay() {
        let mut app = app_with_live(&["sess-raced"]);
        let ready = plain_resume_ready("sess-raced");

        after_nonzero_resume(&mut app, &ready, resume::RESUME_NONZERO_HINT.to_string());

        let modal = app
            .modal
            .clone()
            .expect("claude reports the session live, so the board must offer Attach/Fork");
        assert_eq!(modal.session_id.as_deref(), Some("sess-raced"));
        assert_eq!(
            app.status.as_deref(),
            Some(resume::RESUME_RACE_STATUS),
            "the probe confirmed it, so the board may say the session is running"
        );
    }

    /// The honesty half, and the one that must never regress: a non-zero exit
    /// with the session NOT live is some OTHER failure — a user Ctrl-C, a broken
    /// config, anything. We know nothing, so we claim nothing and leave the
    /// neutral hint exactly as it was.
    #[test]
    fn a_nonzero_plain_resume_on_a_session_that_is_not_live_keeps_the_neutral_hint() {
        let mut app = app_with_live(&[]);
        let ready = plain_resume_ready("sess-plain");

        after_nonzero_resume(&mut app, &ready, resume::RESUME_NONZERO_HINT.to_string());

        assert!(
            app.modal.is_none(),
            "nothing says this session is running, so it must not be routed to \
             the running-session overlay"
        );
        assert_eq!(
            app.status.as_deref(),
            Some(resume::RESUME_NONZERO_HINT),
            "the neutral hint must survive unchanged"
        );
        let status = app.status.clone().unwrap();
        assert!(
            !status.contains("is now running"),
            "the board must NEVER claim a session is running without the probe's \
             word: {status}"
        );
    }

    /// Recovery is scoped by CONSTRUCTION, not by sniffing argv: a fork carries
    /// no `race_probe_id`, so it cannot recover even though the session IS live.
    ///
    /// That is correct — a fork of a live session is expected to work, so its
    /// non-zero exit is a genuine failure and re-routing it to "Fork instead"
    /// would be a loop. The live set is seeded LIVE here on purpose: only the
    /// `None` id keeps the neutral hint, so this fails if the guard is dropped.
    #[test]
    fn a_nonzero_fork_never_recovers_even_when_the_session_is_live() {
        let mut app = app_with_live(&["sess-forked"]);
        let ready = resume::Ready {
            cwd: PathBuf::from("/tmp"),
            argv: resume::build_argv("sess-forked", true),
            nonzero_hint: resume::RESUME_NONZERO_HINT,
            // A fork is structurally excluded from race recovery.
            race_probe_id: None,
        };

        after_nonzero_resume(&mut app, &ready, resume::RESUME_NONZERO_HINT.to_string());

        assert!(
            app.modal.is_none(),
            "a fork's non-zero exit is a real failure, not a lost race"
        );
        assert_eq!(app.status.as_deref(), Some(resume::RESUME_NONZERO_HINT));
    }

    /// A new session carries its OWN hint, and recovery must not overwrite it
    /// with the resume-worded one. Pins that the `_ =>` arm passes the plan's
    /// status through untouched rather than reaching for a resume default.
    #[test]
    fn a_nonzero_new_session_keeps_its_own_hint() {
        let mut app = app_with_live(&[]);
        let ready = resume::Ready {
            cwd: PathBuf::from("/tmp"),
            argv: resume::build_new_argv(None, None),
            nonzero_hint: resume::NEW_SESSION_NONZERO_HINT,
            race_probe_id: None,
        };

        after_nonzero_resume(
            &mut app,
            &ready,
            resume::NEW_SESSION_NONZERO_HINT.to_string(),
        );

        assert!(app.modal.is_none());
        assert_eq!(
            app.status.as_deref(),
            Some(resume::NEW_SESSION_NONZERO_HINT),
            "a new session has no session to probe and its own hint must stand"
        );
    }
}
