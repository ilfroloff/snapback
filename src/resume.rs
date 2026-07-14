//! Resume / fork handoff to `claude`.
//!
//! Given a selected `Session`, re-reads `cwd` + `sessionId` from INSIDE the
//! file (authoritative) and verifies `cwd` still exists; if gone, surfaces a
//! clear message and refuses (deleted worktrees are common).
//!
//! `snapback` is a persistent dashboard: on a confirmed resume it SPAWNS
//! `claude` as a child process that inherits the current stdio (a real TTY),
//! WAITS for it to exit, and RETURNS so control comes back to the board. It
//! never replaces its own process image, so quitting the resumed `claude` drops
//! you back onto the session list rather than ending `snapback`.
//!
//! Two seams keep the terminal safe across that round trip:
//!
//! * [`check`] runs the pure re-read + existence predicate while the terminal is
//!   still up, so a refusal (deleted worktree / unreadable file) never tears the
//!   UI down — the caller surfaces the message as a transient board status.
//! * [`launch`] runs only for a confirmed [`Ready`] plan, AFTER `tui::run` has
//!   already torn the terminal down, so `claude` starts on a clean terminal and
//!   the re-initialized board picks up on the next loop iteration.
//!
//! The pure, separable pieces — [`build_argv`], [`read_authoritative`], and the
//! existence check in [`plan_from_parts`] — are unit tested. The impure driver
//! ([`launch`]) performs `chdir` + spawn + wait and is kept thin over those
//! tested helpers.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::store::{parse, Session};

/// A ready-to-run hand-off, or a refusal with a user-facing message.
///
/// Split from the impure launch so the re-read + existence logic can be decided
/// (and unit tested) without touching the process image.
#[derive(Debug)]
pub enum ResumePlan {
    /// The file re-read succeeded and `cwd` still exists: `chdir` here and spawn
    /// `claude` with these parts.
    Ready {
        /// Authoritative `cwd` read from inside the file.
        cwd: PathBuf,
        /// Authoritative `sessionId` read from inside the file (else the stem).
        session_id: String,
        /// Whether to fork rather than plain-resume.
        fork: bool,
    },
    /// Do not resume; `message` explains why (surfaced as a board status).
    Refuse {
        /// User-facing explanation (multi-line, self-contained).
        message: String,
    },
}

/// A confirmed, ready-to-spawn hand-off produced by [`check`] / [`check_attach`].
///
/// Carrying the already-decided parts across the teardown boundary means the
/// spawn side ([`launch`]) never has to re-open the file after the terminal is
/// gone — the existence check happened once, while the UI was still up. The
/// hand-off is described by its full `argv` (built from the session id + mode)
/// so a plain resume, a fork, and an Attach (`claude attach <id>`) all funnel
/// through the SAME launch path with the SAME terminal round trip.
#[derive(Debug, Clone)]
pub struct Ready {
    /// Authoritative `cwd` to `chdir` into before spawning the child.
    pub cwd: PathBuf,
    /// The full argv to spawn; `argv[0]` is the program (always `claude`).
    pub argv: Vec<String>,
}

/// Neutral hint shown on the board when a resumed `claude` exits NON-ZERO.
///
/// Deliberately does not assert a cause: a `claude -r` refusal (the session is
/// live) and a user Ctrl-C'ing a healthy session both exit non-zero, so the
/// message only points at the next moves rather than diagnosing.
pub const RESUME_NONZERO_HINT: &str =
    "claude exited with an error — if this session is running, use Fork (Ctrl-F) or Attach.";

/// Refusal shown when Attach is chosen for a session with no attachable agent
/// job.
///
/// `claude attach` matches the agent-view JOB id (the short `id` from
/// `claude agents --json`), which only BACKGROUND agents expose. An INTERACTIVE
/// live session has no such id, so there is nothing to attach to — refuse with
/// this hint rather than spawning a broken `claude attach` (which would exit 1
/// with "No job matching").
pub const ATTACH_NO_JOB_ID: &str = "This interactive session has no attachable agent job; \
     open it in its own terminal, or Fork instead.";

/// Why a resume did not hand off to `claude`.
///
/// [`check`] yields [`ResumeError::Refused`] when the session cannot be resumed
/// in place (no readable `cwd`, or the `cwd` is gone); [`launch`] yields
/// [`ResumeError::Launch`] when the confirmed hand-off itself fails (could not
/// `chdir`, or `claude` would not spawn). Both are surfaced as a transient
/// board status — `snapback` stays running either way.
#[derive(Debug)]
pub enum ResumeError {
    /// Refused before launch (no readable `cwd`, or the `cwd` is gone).
    Refused(String),
    /// The launch itself failed (could not `chdir`, or `claude` would not spawn).
    Launch(String),
}

impl ResumeError {
    /// The user-facing message for either failure case.
    pub fn message(&self) -> &str {
        match self {
            ResumeError::Refused(m) | ResumeError::Launch(m) => m,
        }
    }
}

/// Build the `claude` argv for a resume (`claude -r <id>`) or fork
/// (`claude -r <id> --fork-session`). Pure so it is directly assertable; `argv[0]`
/// is the program to spawn.
#[must_use]
pub fn build_argv(session_id: &str, fork: bool) -> Vec<String> {
    let mut argv = vec![
        "claude".to_string(),
        "-r".to_string(),
        session_id.to_string(),
    ];
    if fork {
        argv.push("--fork-session".to_string());
    }
    argv
}

/// Build the `claude` argv for ATTACHING to a running session
/// (`claude attach <job-id>`).
///
/// `claude attach <job-id>` is a one-shot command that opens the running agent
/// in this terminal. The argument is the agent-view JOB id — the SHORT id from
/// `claude agents --json` (e.g. `ca56b543`), NOT the full `sessionId`: `claude
/// attach` matches jobs on that short id and returns exit 1 ("No job matching")
/// for a full UUID. This stays a DUMB builder — it just formats whatever id it
/// is handed; picking the authoritative short id is [`attach_job_id`]'s job. It
/// fails soft on a stale/finished id (exit 1), so a non-zero exit surfaces a
/// board status rather than hanging. Pure so the exact invocation is directly
/// assertable.
#[must_use]
pub fn build_attach_argv(job_id: &str) -> Vec<String> {
    vec![
        "claude".to_string(),
        "attach".to_string(),
        job_id.to_string(),
    ]
}

/// Decide the Attach target from the matched live agent's agent-view `id`, or
/// refuse.
///
/// Pure gate so the "attachable vs interactive" decision is unit-tested without
/// spawning anything. A background agent carries a non-empty short `id` (the
/// authoritative `claude attach` target) → `Ok(id)`. An interactive session (or
/// a live record that has since dropped its job) has `None`/empty → `Err` with
/// [`ATTACH_NO_JOB_ID`]. The short id is NEVER derived by splitting the
/// `sessionId`; it comes only from claude's own authoritative `id` so it stays
/// collision-safe.
fn attach_job_id(agent_id: Option<&str>) -> Result<&str, String> {
    match agent_id {
        Some(id) if !id.trim().is_empty() => Ok(id),
        _ => Err(ATTACH_NO_JOB_ID.to_string()),
    }
}

/// Map a child's exit code to an optional board status.
///
/// `Some(0)` (a clean exit) → `None` (no status). Any non-zero code, or `None`
/// (killed by a signal, so no code) → the neutral [`RESUME_NONZERO_HINT`]. Pure
/// so the exit-handling is unit-testable without actually spawning `claude`.
#[must_use]
pub fn status_for_exit(code: Option<i32>) -> Option<String> {
    match code {
        Some(0) => None,
        _ => Some(RESUME_NONZERO_HINT.to_string()),
    }
}

/// Re-read the authoritative `(cwd, session_id)` from INSIDE the session file.
///
/// Never trusts the in-memory `Session` copy or the encoded folder name — the
/// file on disk is the source of truth at hand-off time (it may have changed
/// since the store was last loaded, and the `/`->`-` folder encoding is lossy).
/// Returns `None` if the file is gone or carries no `cwd` (a sidecar
/// agent-name/ai-title file, which is not resumable). Reuses the data core's
/// fail-soft [`parse::parse_file`] so parsing lives in exactly one place.
fn read_authoritative(file: &Path) -> Option<(PathBuf, String)> {
    let parsed = parse::parse_file(file)?;
    Some((PathBuf::from(parsed.cwd), parsed.session_id))
}

/// Decide a [`ResumePlan`] from an already-re-read `(cwd, session_id)`.
///
/// Split out so the cwd-existence predicate is unit-testable without a real
/// session file: an existing directory yields [`ResumePlan::Ready`], a missing
/// one yields [`ResumePlan::Refuse`] (the refusal path for deleted worktrees).
fn plan_from_parts(cwd: PathBuf, session_id: String, fork: bool) -> ResumePlan {
    if cwd.is_dir() {
        ResumePlan::Ready {
            cwd,
            session_id,
            fork,
        }
    } else {
        ResumePlan::Refuse {
            message: format!(
                "The original working directory no longer exists:\n    {}\n\
                 That worktree/branch was probably deleted, so this session \
                 cannot be resumed in place.",
                cwd.display()
            ),
        }
    }
}

/// Re-read the selected session and decide whether it can be resumed.
///
/// Refuses (rather than guessing) when the file yields no `cwd`, and refuses
/// when that `cwd` no longer exists on disk.
pub fn plan(session: &Session, fork: bool) -> ResumePlan {
    match read_authoritative(&session.file) {
        Some((cwd, session_id)) => plan_from_parts(cwd, session_id, fork),
        None => ResumePlan::Refuse {
            message: format!(
                "Could not read a cwd from the session file; refusing to guess:\n    {}",
                session.file.display()
            ),
        },
    }
}

/// Terminal-up refusal gate: run the pure [`plan`] while the UI is still drawn.
///
/// `Ok(Ready)` means the caller may tear the terminal down and [`launch`] the
/// child; `Err(ResumeError::Refused)` means stay on the board and surface the
/// message. Doing this BEFORE teardown avoids a needless teardown/re-init flash
/// on a session that cannot be resumed anyway.
pub fn check(session: &Session, fork: bool) -> Result<Ready, ResumeError> {
    match plan(session, fork) {
        ResumePlan::Ready {
            cwd,
            session_id,
            fork,
        } => Ok(Ready {
            argv: build_argv(&session_id, fork),
            cwd,
        }),
        ResumePlan::Refuse { message } => Err(ResumeError::Refused(message)),
    }
}

/// Terminal-up gate for ATTACHING to a running session (the Attach choice on the
/// running-session overlay).
///
/// `agent_id` is the matched live agent's agent-view job `id` (the SHORT id from
/// `claude agents --json`), which is what `claude attach` matches — the full
/// `sessionId` read from the file does NOT work here. It is gated first via
/// [`attach_job_id`]: an interactive session (no job id) refuses with
/// [`ATTACH_NO_JOB_ID`] rather than spawning a broken `claude attach <uuid>`.
///
/// On an attachable job, it reuses the same authoritative re-read +
/// `cwd`-existence check as [`check`] (the `fork` flag is irrelevant here) and,
/// on success, produces a [`Ready`] that reattaches through the identical
/// [`launch`] round trip — built from the short job `id`, never the session
/// UUID. The `cwd` is still carried so `launch` `chdir`s into it, keeping parity
/// with resume/fork. A deleted worktree / unreadable file refuses with a board
/// status, exactly like a plain resume.
pub fn check_attach(session: &Session, agent_id: Option<&str>) -> Result<Ready, ResumeError> {
    let job_id = attach_job_id(agent_id).map_err(ResumeError::Refused)?;
    match plan(session, false) {
        ResumePlan::Ready { cwd, .. } => Ok(Ready {
            argv: build_attach_argv(job_id),
            cwd,
        }),
        ResumePlan::Refuse { message } => Err(ResumeError::Refused(message)),
    }
}

/// Build a `Command` from an argv whose first element is the program.
fn command(argv: &[String]) -> Command {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd
}

/// Spawn the confirmed [`Ready`] hand-off, wait for it, and RETURN.
///
/// Preconditions: the terminal has already been torn down by `tui::run`, so the
/// child inherits a clean, non-raw TTY (inherited stdin/stdout/stderr are the
/// default for [`Command`]). This `chdir`s into the authoritative `cwd` first,
/// then spawns `ready.argv` (`claude -r <id> [--fork-session]` for a
/// resume/fork, or `claude attach <id>` for an Attach) and blocks until
/// it exits.
///
/// Returns `Ok(Some(status))` on a NON-ZERO / signalled child exit (a neutral
/// board hint — see [`status_for_exit`]) and `Ok(None)` on a clean exit;
/// `snapback` always returns to the board afterwards. Returns [`ResumeError::Launch`] only
/// when the hand-off could not even start (the `chdir` failed or the program
/// could not be spawned). The process cwd is restored afterwards so a subsequent
/// store reload / relative path is unaffected.
pub fn launch(ready: &Ready) -> Result<Option<String>, ResumeError> {
    let restore_to = std::env::current_dir().ok();

    if let Err(e) = std::env::set_current_dir(&ready.cwd) {
        return Err(ResumeError::Launch(format!(
            "could not chdir into {}: {e}",
            ready.cwd.display()
        )));
    }

    let result = match command(&ready.argv).status() {
        // A clean exit shows nothing; a non-zero / signalled exit surfaces the
        // neutral hint. Either way we return straight to the board.
        Ok(status) => Ok(status_for_exit(status.code())),
        Err(spawn_err) => Err(ResumeError::Launch(format!(
            "failed to launch `{}`: {spawn_err}",
            ready.argv.join(" ")
        ))),
    };

    // Best-effort: put the process cwd back so store reloads / relative paths
    // behave as they did before the hand-off.
    if let Some(dir) = restore_to {
        let _ = std::env::set_current_dir(dir);
    }

    result
}

/// The OS "open this in the default application" launcher argv, per target: `open`
/// on macOS, `xdg-open` on Linux, `cmd /C start` on Windows (the leading empty
/// string is `start`'s window-title argument, so a url with special characters is
/// not mistaken for the title). `None` on any other target, where opening a link
/// is simply a no-op rather than a broken spawn. Pure so the exact invocation is
/// unit-testable without spawning anything.
fn opener_argv(url: &str) -> Option<Vec<String>> {
    #[cfg(target_os = "macos")]
    let argv = Some(vec!["open".to_string(), url.to_string()]);
    #[cfg(target_os = "linux")]
    let argv = Some(vec!["xdg-open".to_string(), url.to_string()]);
    #[cfg(target_os = "windows")]
    let argv = Some(vec![
        "cmd".to_string(),
        "/C".to_string(),
        "start".to_string(),
        String::new(),
        url.to_string(),
    ]);
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let argv = {
        let _ = url; // unsupported target: nothing to open
        None
    };
    argv
}

/// Open `url` in the user's default browser — fire-and-forget and OFF the render
/// loop.
///
/// Unlike [`launch`] (which spawns AND waits, after the terminal is torn down),
/// this must never block the drawing loop and must NOT touch the terminal: the TUI
/// stays up while the browser opens in the background. It therefore spawns the OS
/// opener on its OWN detached thread, which then `wait`s on the child so it is
/// reaped (no zombie) without the UI thread ever blocking. The opener
/// (`open`/`xdg-open`) hands the url to the browser and exits promptly.
///
/// FAIL-SOFT throughout: an empty url, an unsupported target, a missing opener, or
/// a spawn error are all swallowed, so a malformed link or a machine with no
/// browser can never crash — or even disturb — the board. Child stdio is nulled so
/// the opener can neither read the board's stdin nor paint over the alt screen.
pub fn open_url(url: &str) {
    if url.trim().is_empty() {
        return;
    }
    let Some(argv) = opener_argv(url) else {
        return; // unsupported target: no-op rather than a broken spawn
    };
    std::thread::spawn(move || {
        let mut cmd = command(&argv);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Ok(mut child) = cmd.spawn() {
            let _ = child.wait();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Path to a committed fixture session file.
    fn fixture(folder: &str, file: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("store")
            .join(folder)
            .join(file)
    }

    #[test]
    fn reads_cwd_and_session_id_from_inside_the_file_not_the_folder() {
        let path = fixture("-Users-me-project-alpha", "sess-normal-1.jsonl");
        let (cwd, session_id) = read_authoritative(&path).expect("normal fixture is resumable");
        // In-file `cwd`, NOT a decode of the folder name "-Users-me-project-alpha".
        assert_eq!(cwd, PathBuf::from("/Users/me/project-alpha"));
        assert_eq!(session_id, "sess-normal-1");
    }

    #[test]
    fn sidecar_without_cwd_is_not_resumable() {
        // The agent-title sidecar carries no `cwd`: refuse rather than guess.
        let path = fixture("-Users-me-project-alpha", "agent-title-xyz.jsonl");
        assert!(read_authoritative(&path).is_none());
    }

    #[test]
    fn plan_refuses_a_session_whose_cwd_is_gone() {
        // The normal fixture's in-file cwd (/Users/me/project-alpha) does not
        // exist on the test host, so the full re-read + existence check refuses.
        let session = Session {
            file: fixture("-Users-me-project-alpha", "sess-normal-1.jsonl"),
            session_id: "stale-in-memory".into(),
            cwd: PathBuf::from("/tmp"),
            git_branch: None,
            timestamp: None,
            repo: "project-alpha".into(),
            label: String::new(),
            content_index: String::new(),
        };
        match plan(&session, false) {
            ResumePlan::Refuse { message } => {
                assert!(message.contains("no longer exists"), "{message}");
            }
            ResumePlan::Ready { .. } => panic!("a missing cwd must refuse"),
        }
    }

    #[test]
    fn check_refuses_a_session_whose_cwd_is_gone() {
        // The terminal-up gate must surface the same refusal as `plan`, mapped
        // onto `ResumeError::Refused`, so the board can show it without tearing
        // the UI down.
        let session = Session {
            file: fixture("-Users-me-project-alpha", "sess-normal-1.jsonl"),
            session_id: "stale-in-memory".into(),
            cwd: PathBuf::from("/tmp"),
            git_branch: None,
            timestamp: None,
            repo: "project-alpha".into(),
            label: String::new(),
            content_index: String::new(),
        };
        match check(&session, false) {
            Err(ResumeError::Refused(message)) => {
                assert!(message.contains("no longer exists"), "{message}");
            }
            other => panic!("a missing cwd must refuse via check: {other:?}"),
        }
    }

    #[test]
    fn existence_check_refuses_a_missing_cwd() {
        let missing = PathBuf::from("/no/such/snapback/worktree/anywhere");
        assert!(!missing.exists(), "test path must not exist");
        match plan_from_parts(missing, "sess-x".into(), false) {
            ResumePlan::Refuse { message } => {
                assert!(message.contains("no longer exists"), "{message}")
            }
            ResumePlan::Ready { .. } => panic!("a missing cwd must refuse"),
        }
    }

    #[test]
    fn existence_check_proceeds_for_an_existing_dir() {
        let existing = std::env::temp_dir();
        assert!(existing.is_dir(), "temp_dir should exist");
        match plan_from_parts(existing.clone(), "sess-x".into(), true) {
            ResumePlan::Ready {
                cwd,
                session_id,
                fork,
            } => {
                assert_eq!(cwd, existing);
                assert_eq!(session_id, "sess-x");
                assert!(fork, "fork flag must carry through");
            }
            ResumePlan::Refuse { .. } => panic!("an existing cwd must proceed"),
        }
    }

    #[test]
    fn argv_is_claude_dash_r_for_a_plain_resume() {
        assert_eq!(build_argv("abc-123", false).join(" "), "claude -r abc-123");
    }

    #[test]
    fn argv_appends_fork_session_for_a_fork() {
        assert_eq!(
            build_argv("abc-123", true).join(" "),
            "claude -r abc-123 --fork-session"
        );
    }

    #[test]
    fn attach_argv_reattaches_by_the_short_agent_view_job_id() {
        // Attach uses the one-shot `claude attach <job-id>` command, keyed on the
        // SHORT agent-view id (e.g. `ca56b543`), which is what `claude attach`
        // matches — NOT the full session UUID.
        assert_eq!(
            build_attach_argv("ca56b543").join(" "),
            "claude attach ca56b543"
        );
    }

    #[test]
    fn attach_job_id_takes_the_short_id_and_refuses_when_absent() {
        // A background agent exposes a non-empty short id -> that is the target.
        assert_eq!(attach_job_id(Some("ca56b543")), Ok("ca56b543"));
        // An interactive session (or a dropped job) has no id -> refuse; an empty
        // / whitespace id is treated the same (nothing attachable).
        assert_eq!(attach_job_id(None), Err(ATTACH_NO_JOB_ID.to_string()));
        assert_eq!(attach_job_id(Some("")), Err(ATTACH_NO_JOB_ID.to_string()));
        assert_eq!(
            attach_job_id(Some("   ")),
            Err(ATTACH_NO_JOB_ID.to_string())
        );
    }

    #[test]
    fn attach_plumbs_the_short_id_not_the_full_session_uuid() {
        // The whole point of Option A: attach argv is built from claude's
        // authoritative SHORT id, never the full `sessionId`.
        let short = "ca56b543";
        let full_uuid = "ca56b543-bf36-4a1e-9c2d-0123456789ab";
        let job_id = attach_job_id(Some(short)).expect("a background id is attachable");
        let argv = build_attach_argv(job_id).join(" ");
        assert_eq!(argv, "claude attach ca56b543");
        assert!(
            !argv.contains(full_uuid),
            "attach must use the short agent-view job id, not the full session UUID: {argv}"
        );
    }

    /// The URL opener must target the platform's default-app launcher and pass the
    /// url through verbatim as the final argument, so click-to-open hands the
    /// browser exactly the link under the pointer. Guarded to the supported
    /// targets (the only ones this personal tool builds for).
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    #[test]
    fn opener_argv_targets_the_platform_launcher_and_passes_the_url_last() {
        let url = "https://example.com/page?x=1";
        let argv = opener_argv(url).expect("a supported target has an opener");
        assert_eq!(
            argv.last().map(String::as_str),
            Some(url),
            "the url is the final argument, verbatim"
        );
        assert!(
            matches!(argv[0].as_str(), "open" | "xdg-open" | "cmd"),
            "argv[0] must be the OS default-app launcher, got {}",
            argv[0]
        );
    }

    #[test]
    fn check_attach_refuses_an_interactive_session_without_a_job_id() {
        // The interactive gate fires BEFORE the cwd re-read, so no attachable-id
        // check depends on the (here missing) worktree: a live-but-interactive
        // session must refuse with the clear hint rather than spawn a broken
        // `claude attach <uuid>`.
        let session = Session {
            file: fixture("-Users-me-project-alpha", "sess-normal-1.jsonl"),
            session_id: "sess-normal-1".into(),
            cwd: PathBuf::from("/tmp"),
            git_branch: None,
            timestamp: None,
            repo: "project-alpha".into(),
            label: String::new(),
            content_index: String::new(),
        };
        match check_attach(&session, None) {
            Err(ResumeError::Refused(message)) => {
                assert_eq!(message, ATTACH_NO_JOB_ID);
            }
            other => panic!("an interactive session (no job id) must refuse Attach: {other:?}"),
        }
    }

    /// Task VERIFY-3: a NON-ZERO child exit maps to a status; a clean (exit 0)
    /// one does not. Tested through the pure `status_for_exit` helper so no
    /// `claude` process is ever spawned.
    #[test]
    fn nonzero_exit_yields_a_status_and_clean_exit_does_not() {
        assert_eq!(
            status_for_exit(Some(0)),
            None,
            "a clean exit shows no status"
        );
        assert!(
            status_for_exit(Some(1)).is_some(),
            "a non-zero exit must surface a status"
        );
        assert!(status_for_exit(Some(2)).is_some());
        assert!(
            status_for_exit(None).is_some(),
            "a signalled exit (no code) must also surface a status"
        );
        // The hint is NEUTRAL — it points at the next moves, not a specific cause.
        let msg = status_for_exit(Some(1)).expect("non-zero has a message");
        let lower = msg.to_lowercase();
        assert!(
            lower.contains("fork") && lower.contains("attach"),
            "the hint should point at Fork/Attach: {msg}"
        );
    }
}
