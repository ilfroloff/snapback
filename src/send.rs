//! Non-UI send core: the one-shot `claude -p -r <id>` quick-reply path.
//!
//! `snapback` is otherwise read-only — it browses sessions and delegates every
//! WRITE to an interactive `claude` child. This module is the first path that
//! writes to a session WITHOUT tearing the terminal down: `claude -p -r <id>
//! --output-format json "<msg>"` resumes a session non-interactively, replays
//! its full context, APPENDS the exchange in place to the same `<id>.jsonl`,
//! prints a JSON result, and exits. It needs no TTY (its stdio is a pipe), so it
//! runs on its OWN detached thread (mirroring [`crate::resume::open_url`]) while
//! the board stays up; the reply then renders through the existing
//! `SessionWatcher` → `SessionsChanged` → reload → preview path.
//!
//! The seam mirrors `resume.rs`: the DECISIONS are pure and unit-tested with no
//! process ever spawned —
//!
//! * [`build_send_argv`] — the exact `claude` invocation (a dumb formatter, like
//!   [`crate::resume::build_argv`]).
//! * [`reply_gate`] — what `Ctrl-R` does per the session's live-agent state.
//!   `claude -p -r <id>` refuses a session claude holds as an agent (`Error:
//!   Session <id> is currently running as a background agent (bg)…`), but `claude
//!   stop <job-id>` deregisters the job (conversation kept), after which `-p -r`
//!   resumes and appends in place. So: not held → reply; `done` → stop then reply;
//!   `needs input` → confirm then stop then reply; `working`/`idle`/unstoppable →
//!   refuse ([`SEND_LIVE_REFUSED`]). [`build_stop_argv`] is the stop step;
//!   [`run_send`] runs it (best-effort) before the send.
//! * [`plan_send`] — the AUTHORITATIVE re-read of `(cwd, session_id)` from INSIDE
//!   the file at send time (via [`crate::store::parse::parse_file`], the one
//!   parser), plus the cwd-existence gate — the send counterpart of
//!   [`crate::resume::plan`].
//! * [`status_for_send`] — map the `--output-format json` payload to a board
//!   status (cost on success, an error hint on `is_error`), FAIL-SOFT.
//!
//! [`spawn_send`] is the only impure piece: the detached-thread driver that
//! spawns the child, reaps it, and delivers exactly one
//! [`AppEvent::SendFinished`] on the merged channel — the UI thread never blocks.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;

use serde_json::Value;

use crate::agents::{self, AgentActivity, ReportedAgent};
use crate::store::parse;
use crate::watch::AppEvent;

/// Refusal shown when a Reply (Ctrl-R) targets a live agent that must NOT be
/// stopped to reply — a `working` (mid-turn) or `idle` agent, or a held session with
/// no stoppable job id (e.g. interactive).
///
/// `claude -p -r <id>` refuses to resume a session registered as an agent (see
/// [`reply_gate`] for claude's verbatim error). A `done` or `needs input` agent can
/// be deregistered first with `claude stop` and replied to in place, but stopping a
/// `working` agent would interrupt live work and stopping an `idle` one abandons a
/// live agent for no clear gain — so those refuse and point at the moves claude's
/// own error names: Attach (interact with it directly) or Fork (`Ctrl-F`, to branch
/// a copy). Mirrors [`crate::resume::ATTACH_NO_JOB_ID`]'s "refuse with a clear next
/// move" shape.
pub const SEND_LIVE_REFUSED: &str =
    "This session is running as an agent — claude won't resume it in place. \
     Attach to answer it, or Fork (Ctrl-F) to branch a copy.";

/// Board status while a send is in flight, set the moment the compose zone hands
/// off. Replaced by [`status_for_send`]'s result (cost / error) when the
/// [`AppEvent::SendFinished`] lands.
pub const SEND_IN_FLIGHT: &str = "sending…";

/// Neutral success status when the JSON parsed but carried no `total_cost_usd`,
/// or when stdout was unreadable/empty (the child ran, but said nothing we can
/// price). Never claims a cost it did not observe.
const SEND_OK: &str = "sent";

/// Status when the `--output-format json` payload reports `is_error: true` — the
/// send ran but claude flagged the turn as an error (surfaced so a failed send is
/// never silently mistaken for a clean one).
const SEND_ERROR: &str = "send failed — claude reported an error (check the transcript)";

/// Neutral error status when the child could not even be spawned (no `claude` on
/// PATH, etc.). FAIL-SOFT: a board status, never a panic.
const SEND_SPAWN_FAILED: &str = "could not start claude to send the message";

/// Fallback error status when the send exited NON-ZERO but left nothing readable
/// on stdout or stderr to quote. Rare — claude normally prints its reason.
const SEND_FAILED_GENERIC: &str = "send failed — claude could not resume this session";

/// Prefix a surfaced claude error carries on the status line, so a failure never
/// reads like the neutral success ([`SEND_OK`]).
const SEND_FAILED_PREFIX: &str = "send failed: ";

/// Max characters of a surfaced claude error kept on the (one-row) status line, so
/// a verbose message cannot balloon the status past what is useful.
const SEND_ERROR_MAX: usize = 200;

/// A ready-to-run send, or a refusal with a user-facing message.
///
/// The send counterpart of [`crate::resume::ResumePlan`]: split from the impure
/// [`spawn_send`] so the authoritative re-read + cwd-existence decision is unit
/// tested without spawning anything.
#[derive(Debug)]
pub enum SendPlan {
    /// The file re-read succeeded and `cwd` still exists: run `claude -p` there.
    Ready {
        /// Authoritative `cwd` read from INSIDE the file (never the folder name).
        cwd: PathBuf,
        /// Authoritative `sessionId` read from inside the file (else the stem).
        session_id: String,
    },
    /// Do not send; `message` explains why (surfaced as a board status).
    Refuse(String),
}

/// What `Ctrl-R` should do for the selected session, decided from what claude is
/// holding it as right now.
///
/// `claude -p -r <id>` refuses a session claude holds as a live agent, so a reply
/// to one cannot land in place UNLESS the job is first stopped (`claude stop
/// <job-id>` deregisters it, keeping the conversation, after which `-p -r` resumes
/// and appends). Stopping is only safe when nothing is running to interrupt, so the
/// decision turns on the agent's STATE:
#[derive(Debug, PartialEq, Eq)]
pub enum ReplyGate {
    /// Not a live agent — open compose and reply in place directly.
    Reply,
    /// A FINISHED (`done`) or TERMINAL (`stopped` / `failed`) agent — the job is
    /// over, so stopping it is harmless; stop it, then reply. Opens compose
    /// straight away; the send stops `job_id` first.
    StopThenReply {
        /// The short agent-view job id to `claude stop`.
        job_id: String,
    },
    /// A WAITING (`needs input`) agent — stopping abandons a live agent, so CONFIRM
    /// first; on confirm, compose opens and the send stops `job_id`.
    ConfirmStopThenReply {
        /// The short agent-view job id to `claude stop`.
        job_id: String,
    },
    /// A busy/idle agent (or one with no stoppable job id) — refuse with a hint.
    Refuse(&'static str),
}

/// Decide [`ReplyGate`] from the session's current live-agent record (`None` when
/// claude is not holding it).
///
/// Pure so the whole decision tree is unit-tested without a probe. It reuses the
/// one classifier ([`agents::classify`]) so "what state is this" is answered in one
/// place. `done` — or a TERMINAL `stopped` / `failed` — → stop-then-reply (safe:
/// the job has ended, so stopping it interrupts nothing); `needs input` → confirm
/// first (stopping abandons a waiting agent); `working`/`idle`/unknown → refuse
/// (stopping would interrupt live work, and the user should Attach/Fork). A held
/// agent with no stoppable job id (`id == None`, e.g. an interactive session) can't
/// be stopped, so it refuses too. Not held at all → reply in place directly.
#[must_use]
pub fn reply_gate(record: Option<&ReportedAgent>) -> ReplyGate {
    let Some(agent) = record else {
        return ReplyGate::Reply; // claude isn't holding it -> plain in-place reply
    };
    let Some(job_id) = agent.id.as_deref().filter(|id| !id.trim().is_empty()) else {
        // Held but not stoppable by job id (e.g. an interactive session).
        return ReplyGate::Refuse(SEND_LIVE_REFUSED);
    };
    let job_id = job_id.to_string();
    match agents::classify(agent) {
        AgentActivity::Done | AgentActivity::Ended => ReplyGate::StopThenReply { job_id },
        AgentActivity::NeedsInput => ReplyGate::ConfirmStopThenReply { job_id },
        AgentActivity::Working | AgentActivity::Idle | AgentActivity::Other => {
            ReplyGate::Refuse(SEND_LIVE_REFUSED)
        }
    }
}

/// Board status while an interrupt (`Ctrl-K`) is in flight, set the moment the stop
/// is dispatched. Replaced by [`status_for_stop`]'s result when the
/// [`AppEvent::InterruptFinished`](crate::watch::AppEvent::InterruptFinished) lands.
pub const INTERRUPT_IN_FLIGHT: &str = "stopping…";

/// Refusal shown when `Ctrl-K` targets a session claude is NOT holding as an agent:
/// there is no live job to stop. A resumable transcript on disk is not a running
/// process, so stopping is meaningless — say so rather than shell out to fail.
pub const INTERRUPT_NOT_LIVE: &str =
    "This session isn't running as an agent — there's nothing to stop.";

/// Refusal shown when `Ctrl-K` targets a LIVE session with no stoppable job id — an
/// interactive session (running in another terminal) that `claude agents` lists
/// without an `id`. `claude stop` only takes the short background job id, so an
/// interactive one can't be stopped from here; point at the terminal that owns it.
pub const INTERRUPT_NO_JOB_ID: &str =
    "This session is running interactively, not as a background agent — \
     stop it from the terminal that's running it.";

/// What `Ctrl-K` should do for the selected session, decided from what claude is
/// holding it as right now.
///
/// The interrupt counterpart of [`ReplyGate`], with the OPPOSITE intent: a reply
/// must never interrupt live work, whereas an interrupt exists to stop it — so a
/// `working` (mid-turn) agent is a valid target here, not a refusal. Only a
/// background job carries the short id `claude stop` takes; an interactive live
/// session (no id) can't be stopped from here, and a session claude isn't holding at
/// all has nothing to stop. Stopping abandons live work, so every state EXCEPT a
/// finished (`done`) or terminal (`stopped` / `failed`) one confirms first.
#[derive(Debug, PartialEq, Eq)]
pub enum InterruptGate {
    /// A FINISHED (`done`) or TERMINAL (`stopped` / `failed`) agent — the job is
    /// already over, so stop it immediately (harmless; nothing runs).
    StopNow {
        /// The short agent-view job id to `claude stop`.
        job_id: String,
    },
    /// A LIVE agent (`working` / `needs input` / `idle` / other) — stopping abandons
    /// live work, so CONFIRM first; on confirm, run `claude stop <job-id>`.
    Confirm {
        /// The short agent-view job id to `claude stop`.
        job_id: String,
    },
    /// Nothing stoppable — refuse with a message (not a live agent, or interactive
    /// with no job id).
    Refuse(&'static str),
}

/// Decide [`InterruptGate`] from the session's current live-agent record (`None`
/// when claude is not holding it).
///
/// Pure so the whole decision tree is unit-tested without a probe, reusing the one
/// classifier ([`agents::classify`]). Mirrors [`reply_gate`]'s shape but routes by
/// the interrupt intent: not held → refuse (nothing to stop); held without a
/// stoppable job id → refuse (interactive, can't stop from here); `done` — or a
/// TERMINAL `stopped` / `failed` — → stop immediately (harmless; the job is already
/// over); every other live state → confirm first (stopping abandons live work).
#[must_use]
pub fn interrupt_gate(record: Option<&ReportedAgent>) -> InterruptGate {
    let Some(agent) = record else {
        return InterruptGate::Refuse(INTERRUPT_NOT_LIVE); // nothing running to stop
    };
    let Some(job_id) = agent.id.as_deref().filter(|id| !id.trim().is_empty()) else {
        // Live but not stoppable by job id (e.g. an interactive session).
        return InterruptGate::Refuse(INTERRUPT_NO_JOB_ID);
    };
    let job_id = job_id.to_string();
    match agents::classify(agent) {
        AgentActivity::Done | AgentActivity::Ended => InterruptGate::StopNow { job_id },
        AgentActivity::Working
        | AgentActivity::NeedsInput
        | AgentActivity::Idle
        | AgentActivity::Other => InterruptGate::Confirm { job_id },
    }
}

/// A confirmed send request handed from the compose zone (pure decision) to the
/// driver ([`crate::tui::run`]), which spawns it via [`spawn_send`].
///
/// Carrying the already-built parts across to the driver keeps the process spawn
/// OUT of the pure event handler — mirroring how [`crate::resume::Ready`] carries
/// a confirmed hand-off so the terminal-teardown spawn lives only in the driver.
/// Here there is no teardown (the board stays up), so the driver just fires the
/// detached thread and keeps looping.
#[derive(Debug, Clone)]
pub struct SendRequest {
    /// The full argv to spawn; `argv[0]` is the program (always `claude`).
    pub argv: Vec<String>,
    /// Authoritative `cwd` to run the child in (never mutates the process cwd).
    pub cwd: PathBuf,
    /// Authoritative `sessionId` the completion event is keyed by, so the handler
    /// can tell whether the finished send targets the currently-previewed row.
    pub session_id: String,
    /// Short agent-view job id to `claude stop` FIRST when set — the stop-then-reply
    /// path for a held (`done`/`needs input`) background agent. `None` for a plain
    /// in-place reply to a session claude is not holding.
    pub stop_job: Option<String>,
}

/// A confirmed interrupt handed from the pure event handler to the driver
/// ([`crate::tui::run`]), which spawns it via [`spawn_interrupt`].
///
/// The interrupt counterpart of [`SendRequest`]: the pure handler builds the argv
/// and the driver fires the detached thread, keeping the process spawn OUT of the
/// event handler. `claude stop <job-id>` acts on the GLOBAL background-job registry,
/// so it does not need the session's project dir — it runs in `cwd` (the launch dir)
/// only because a child needs some valid working directory. Using the launch dir
/// (never a re-read of the session's `cwd`) is deliberate: a deleted worktree must
/// never block stopping its still-live job.
#[derive(Debug, Clone)]
pub struct InterruptRequest {
    /// The full argv to spawn; `argv[0]` is the program (always `claude`).
    pub argv: Vec<String>,
    /// A valid directory to run the child in (the launch dir). Never the process cwd.
    pub cwd: PathBuf,
}

/// Build the `claude` argv for a one-shot send:
/// `claude -p -r <id> --output-format json <message>`.
///
/// A DUMB pure formatter, like [`crate::resume::build_argv`] — no trimming or
/// validation beyond formatting (the empty-message guard lives at the call site).
/// `--output-format json` makes the reply machine-readable for [`status_for_send`];
/// the prompt is the trailing positional argument.
///
/// NO permission flags are passed (`--permission-mode` / `--allowedTools`): a send
/// INHERITS the user's existing settings, matching an ordinary interactive resume.
#[must_use]
pub fn build_send_argv(session_id: &str, message: &str) -> Vec<String> {
    vec![
        "claude".to_string(),
        "-p".to_string(),
        "-r".to_string(),
        session_id.to_string(),
        "--output-format".to_string(),
        "json".to_string(),
        message.to_string(),
    ]
}

/// Build the `claude stop <job-id>` argv that DEREGISTERS a background job so a
/// subsequent `claude -p -r` may resume it in place.
///
/// `<job-id>` is the SHORT agent-view id (`claude agents --json`'s own `id`, e.g.
/// `70933ea6`), the same id `claude attach` takes — NOT the full `sessionId`.
/// Stopping keeps the conversation (claude: "Its conversation is kept"); it only
/// drops the live job registration, which is exactly what lets `-p -r` accept the
/// session afterward. A DUMB pure formatter like [`build_send_argv`].
#[must_use]
pub fn build_stop_argv(job_id: &str) -> Vec<String> {
    vec!["claude".to_string(), "stop".to_string(), job_id.to_string()]
}

/// Re-read the authoritative `(cwd, session_id)` from INSIDE the session file at
/// send time and gate on the cwd still existing.
///
/// The send counterpart of [`crate::resume::plan`], and it obeys the same two
/// rules: AUTHORITATIVE-FROM-FILE (reuse [`parse::parse_file`], the ONE parser,
/// never decode the folder name) and refuse rather than guess. A file that is
/// gone or carries no `cwd` (a sidecar) refuses; a `cwd` whose directory was
/// deleted (a removed worktree) refuses. Pure so the refusal path is unit tested
/// against a real temp file exactly like `resume::plan_refuses_...`.
#[must_use]
pub fn plan_send(file: &Path) -> SendPlan {
    let Some(parsed) = parse::parse_file(file) else {
        return SendPlan::Refuse(format!(
            "Could not read a cwd from the session file; refusing to send:\n    {}",
            file.display()
        ));
    };
    let cwd = PathBuf::from(parsed.cwd);
    if !cwd.is_dir() {
        return SendPlan::Refuse(format!(
            "The original working directory no longer exists:\n    {}\n\
             That worktree/branch was probably deleted, so this session cannot \
             receive a message in place.",
            cwd.display()
        ));
    }
    SendPlan::Ready {
        cwd,
        session_id: parsed.session_id,
    }
}

/// Map the `--output-format json` stdout to an optional board status.
///
/// FAIL-SOFT by construction (AGENTS.md): the payload is parsed as
/// `serde_json::Value`, never a hard-typed struct, and no field access can panic
/// on an absent/mistyped key. On `is_error == true` it returns an error status;
/// on success it surfaces `total_cost_usd` (e.g. `"sent — $0.0136"`) when present,
/// else a neutral `"sent"`; unparseable/empty stdout also degrades to the neutral
/// `"sent"` (the child ran, but printed nothing we can read — never a panic,
/// never a false cost). Always `Some` so the caller has a status to show; the
/// `Option` keeps the type uniform with [`AppEvent::SendFinished`]'s field.
#[must_use]
pub fn status_for_send(raw_stdout: &str) -> Option<String> {
    let Ok(value) = serde_json::from_str::<Value>(raw_stdout) else {
        return Some(SEND_OK.to_string()); // unparseable / empty -> neutral, no panic
    };
    if value.get("is_error").and_then(Value::as_bool) == Some(true) {
        return Some(SEND_ERROR.to_string());
    }
    match value.get("total_cost_usd").and_then(Value::as_f64) {
        Some(cost) => Some(format!("sent — ${cost:.4}")),
        None => Some(SEND_OK.to_string()),
    }
}

/// Combine a finished send's exit status + captured streams into a board status.
///
/// This is the honesty seam: a send that FAILED must never read as the neutral
/// success. On a clean (zero) exit the JSON payload is mapped by
/// [`status_for_send`] (cost / `is_error` / neutral). On a NON-ZERO exit —
/// notably claude refusing to resume a session it holds as an agent, which exits
/// `1` with the reason on `stderr` and nothing on `stdout` — the reason is
/// surfaced by [`status_for_failed_send`] instead, so the user sees why rather
/// than a false `"sent"`. Pure so both branches are unit-tested without spawning.
#[must_use]
pub fn status_for_output(success: bool, stdout: &str, stderr: &str) -> String {
    if success {
        // Always `Some` in practice; the fallback keeps the type total.
        status_for_send(stdout).unwrap_or_else(|| SEND_OK.to_string())
    } else {
        status_for_failed_send(stdout, stderr)
    }
}

/// Build the status for a NON-ZERO send exit, surfacing claude's OWN reason.
///
/// Preference order, so the most specific truthful message wins: a JSON
/// `is_error` payload's `result` (when `--output-format json` still printed one),
/// else the first non-empty `stderr` line (the common case — e.g. `Error: Session
/// <id> is currently running as a background agent (bg)…`), else a generic
/// fallback. The quoted text is sanitized (ANSI/control stripped, one line,
/// length-capped) so a raw escape from claude's stderr can never reach the ratatui
/// buffer (TERMINAL-SAFE STYLING). Pure and unit-tested.
#[must_use]
pub fn status_for_failed_send(stdout: &str, stderr: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(stdout) {
        if value.get("is_error").and_then(Value::as_bool) == Some(true) {
            if let Some(result) = value.get("result").and_then(Value::as_str) {
                let cleaned = sanitize_status(result);
                if !cleaned.is_empty() {
                    return format!("{SEND_FAILED_PREFIX}{cleaned}");
                }
            }
        }
    }
    // Sanitize FIRST, then strip the `Error: ` label — claude may color the line,
    // so the label can sit behind an ANSI escape that must be removed before it is
    // visible to the strip.
    let stderr_line = stderr
        .lines()
        .map(sanitize_status)
        .map(|l| strip_error_prefix(&l).to_string())
        .find(|l| !l.is_empty());
    match stderr_line {
        Some(line) => format!("{SEND_FAILED_PREFIX}{line}"),
        None => SEND_FAILED_GENERIC.to_string(),
    }
}

/// Strip a leading `Error: ` label claude prefixes onto a stderr message, so the
/// status is not doubled up (`send failed: Error: …`). Expects an already-sanitized
/// line (see [`status_for_failed_send`]).
fn strip_error_prefix(line: &str) -> &str {
    line.strip_prefix("Error: ").unwrap_or(line)
}

/// Make an external message safe for the one-row status line: drop ANSI escape
/// sequences and other control characters (never embed a raw escape — AGENTS.md
/// TERMINAL-SAFE STYLING), collapse whitespace runs, and cap the length at
/// [`SEND_ERROR_MAX`] characters. Pure.
fn sanitize_status(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(SEND_ERROR_MAX));
    let mut chars = s.chars().peekable();
    let mut last_was_space = false;
    while let Some(c) = chars.next() {
        // Skip a CSI/escape sequence (`ESC [ … <final letter>`), best-effort.
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        if c.is_control() {
            continue;
        }
        if c.is_whitespace() {
            if !last_was_space && !out.is_empty() {
                out.push(' ');
                last_was_space = true;
            }
            continue;
        }
        last_was_space = false;
        out.push(c);
        if out.chars().count() >= SEND_ERROR_MAX {
            break;
        }
    }
    out.trim_end().to_string()
}

/// Spawn the confirmed send on its OWN detached thread and deliver exactly one
/// [`AppEvent::SendFinished`] when it completes — the UI thread never blocks.
///
/// Mirrors [`crate::resume::open_url`] (the fire-and-forget, off-the-render-loop
/// precedent), NOT [`crate::resume::launch`] (which spawns+waits after a terminal
/// teardown). It runs the child in `cwd` via [`Command::current_dir`] — it does
/// NOT mutate the process cwd — captures stdout AND stderr, nulls stdin (so it can
/// never read the board's keystrokes), `wait`s to reap it (no zombie), maps the
/// result through [`status_for_output`], and sends the completion keyed by
/// `session_id`.
///
/// FAIL-SOFT throughout: a spawn error yields a neutral error status rather than a
/// panic, and a send failure on the channel (the board went away) is ignored.
///
/// When `req.stop_job` is set it FIRST runs `claude stop <job-id>` to deregister
/// the held background job (so the following `-p -r` is accepted); if the stop
/// itself fails the reply is NOT attempted (it would only be refused) and the stop
/// error is surfaced.
pub fn spawn_send(req: SendRequest, tx: Sender<AppEvent>) {
    std::thread::spawn(move || {
        let status = run_send(&req);
        // A send failure means the receiver (TUI) has gone away; ignore it.
        let _ = tx.send(AppEvent::SendFinished {
            session_id: req.session_id,
            status,
        });
    });
}

/// Run the (optional stop +) send to completion and map it to a status. The impure
/// step [`spawn_send`] wraps; split out so the spawn/capture/reap lives in one place.
///
/// If `stop_job` is set, `claude stop <job-id>` runs first — BEST-EFFORT: its
/// result is ignored and the reply is attempted regardless. This is deliberate:
/// the job may already have been reaped between the gate and the send (then the
/// stop fails but `-p -r` works), and if the session really is still held the reply
/// itself surfaces the honest reason. The send captures BOTH stdout and stderr and
/// honors the EXIT CODE (via [`status_for_output`]): claude prints a refusal to
/// stderr and exits non-zero with an empty stdout, so nulling stderr / ignoring the
/// code would report the neutral `"sent"` over a failed send — the false positive
/// this avoids. What the user sees is always the REPLY's result.
fn run_send(req: &SendRequest) -> Option<String> {
    if let Some(job_id) = req.stop_job.as_deref() {
        // Deregister the held job so `-p -r` is accepted; ignore the outcome (see above).
        let _ = run_child(&build_stop_argv(job_id), &req.cwd);
    }
    match run_child(&req.argv, &req.cwd) {
        Ok((success, stdout, stderr)) => Some(status_for_output(success, &stdout, &stderr)),
        Err(()) => Some(SEND_SPAWN_FAILED.to_string()),
    }
}

/// Spawn `argv` in `cwd`, capture both streams, reap it, and return `(success,
/// stdout, stderr)` — or `Err(())` if the child could not be spawned. Stdin is
/// nulled so a child can never read the board's keystrokes; the process cwd is
/// never mutated. Shared by the stop step and the send step.
fn run_child(argv: &[String], cwd: &Path) -> Result<(bool, String, String), ()> {
    let output = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|_| ())?;
    Ok((
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

/// Neutral success status for a completed interrupt — `claude stop` exited clean.
const STOP_OK: &str = "stopped";

/// Error status when `claude stop` could not even be spawned (no `claude` on PATH).
const STOP_SPAWN_FAILED: &str = "could not start claude to stop the agent";

/// Prefix a surfaced `claude stop` failure carries, so it never reads as success.
const STOP_FAILED_PREFIX: &str = "stop failed: ";

/// Fallback when `claude stop` exited non-zero but left nothing readable to quote.
const STOP_FAILED_GENERIC: &str = "stop failed — claude could not stop this agent";

/// Spawn a confirmed interrupt on its OWN detached thread and deliver exactly one
/// [`AppEvent::InterruptFinished`] when it completes — the UI thread never blocks.
///
/// The interrupt sibling of [`spawn_send`]: same fire-and-forget shape (run the
/// child in `cwd`, null stdin, capture both streams, reap it), mapped through
/// [`status_for_stop`]. FAIL-SOFT: a spawn error yields a neutral error status
/// rather than a panic, and a failure to report back (the board went away) is
/// ignored.
pub fn spawn_interrupt(req: InterruptRequest, tx: Sender<AppEvent>) {
    std::thread::spawn(move || {
        let status = match run_child(&req.argv, &req.cwd) {
            Ok((success, stdout, stderr)) => status_for_stop(success, &stdout, &stderr),
            Err(()) => STOP_SPAWN_FAILED.to_string(),
        };
        // A send failure means the receiver (TUI) has gone away; ignore it.
        let _ = tx.send(AppEvent::InterruptFinished { status });
    });
}

/// Map a finished `claude stop` (exit status + captured streams) to a board status.
///
/// On a clean exit it is the neutral [`STOP_OK`]. On a NON-ZERO exit — notably
/// stopping a job id that is already gone — claude's OWN reason is surfaced (the
/// first non-empty stderr, else stdout, line), sanitized for the one-row status line
/// (ANSI/control stripped, one line, length-capped — TERMINAL-SAFE STYLING), so a
/// failed stop never reads as success. Reuses [`sanitize_status`]/[`strip_error_prefix`]
/// so the interrupt and the send map external errors identically. Pure and
/// unit-tested.
#[must_use]
pub fn status_for_stop(success: bool, stdout: &str, stderr: &str) -> String {
    if success {
        return STOP_OK.to_string();
    }
    let line = stderr
        .lines()
        .chain(stdout.lines())
        .map(sanitize_status)
        .map(|l| strip_error_prefix(&l).to_string())
        .find(|l| !l.is_empty());
    match line {
        Some(line) => format!("{STOP_FAILED_PREFIX}{line}"),
        None => STOP_FAILED_GENERIC.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    /// A send builds `claude -p -r <id> --output-format json <message>` — no
    /// permission flags, the prompt as the trailing positional argument.
    #[test]
    fn argv_is_claude_dash_p_resume_json_for_a_plain_send() {
        let argv = build_send_argv("abc-123", "hello there");
        assert_eq!(
            argv.join(" "),
            "claude -p -r abc-123 --output-format json hello there"
        );
        // The send INHERITS the user's settings: no permission posture is forced.
        assert!(
            !argv
                .iter()
                .any(|a| a == "--permission-mode" || a == "--allowedTools"),
            "a send must pass no permission flags: {argv:?}"
        );
    }

    /// The stop step is `claude stop <short-job-id>` — the SHORT agent-view id, not
    /// the full sessionId.
    #[test]
    fn stop_argv_is_claude_stop_the_short_job_id() {
        assert_eq!(
            build_stop_argv("70933ea6").join(" "),
            "claude stop 70933ea6"
        );
    }

    /// A message with spaces / newlines stays ONE argv element (never re-split),
    /// so a multiline reply reaches claude intact.
    #[test]
    fn argv_keeps_a_multiline_message_as_a_single_argument() {
        let argv = build_send_argv("id", "line one\nline two");
        assert_eq!(argv.last().map(String::as_str), Some("line one\nline two"));
        assert_eq!(argv.len(), 7, "no extra args from the newline: {argv:?}");
    }

    /// A successful payload surfaces `total_cost_usd` in the status (the whole
    /// point of `--output-format json`: the user sees what the reply cost).
    #[test]
    fn status_surfaces_the_cost_on_a_successful_send() {
        let raw = r#"{"type":"result","subtype":"success","is_error":false,
                      "session_id":"s","num_turns":2,"total_cost_usd":0.0136,
                      "result":"done"}"#;
        let status = status_for_send(raw).expect("always Some");
        assert!(
            status.contains("0.0136"),
            "the cost must be surfaced: {status}"
        );
        assert!(
            status.starts_with("sent"),
            "and it reads as a success: {status}"
        );
    }

    /// `is_error: true` maps to an error status — a failed send is never mistaken
    /// for a clean one, even though `--output-format json` still exits printing a
    /// payload.
    #[test]
    fn status_reports_an_error_payload() {
        let raw = r#"{"type":"result","is_error":true,"total_cost_usd":0.01,
                      "result":"tool blew up"}"#;
        let status = status_for_send(raw).expect("always Some");
        let lower = status.to_lowercase();
        assert!(
            lower.contains("error") || lower.contains("fail"),
            "an is_error payload must read as a failure: {status}"
        );
        // Even though a cost is present, the error verdict wins over the price.
        assert!(
            !status.contains("0.01"),
            "error status must not read as a priced success: {status}"
        );
    }

    /// Garbage, empty, and non-object JSON all degrade to a neutral status and
    /// NEVER panic (FAIL-SOFT) — no field access on an absent/mistyped key faults.
    #[test]
    fn status_is_neutral_and_never_panics_on_garbage_or_empty_stdout() {
        for raw in [
            "",
            "   ",
            "not json at all",
            "{",
            "null",
            "42",
            "\"a bare string\"",
            "[1, 2, 3]",
            r#"{"total_cost_usd":"not-a-number"}"#, // mistyped cost -> neutral, no panic
            r#"{"is_error":"yes"}"#,                // mistyped flag -> not treated as error
        ] {
            let status = status_for_send(raw).expect("neutral fallback is still Some");
            assert!(
                status.starts_with("sent"),
                "unreadable stdout must degrade to a neutral status, got {status:?} for {raw:?}"
            );
        }
    }

    /// A background agent record in a given `state`, carrying a stoppable job id.
    fn bg(state: &str, job_id: Option<&str>) -> ReportedAgent {
        ReportedAgent {
            kind: "background".to_string(),
            id: job_id.map(str::to_owned),
            state: Some(state.to_string()),
            status: None,
            name: None,
        }
    }

    /// The reply gate: not held → reply; `done` → stop-then-reply; `needs input` →
    /// confirm-then-stop-then-reply; busy/idle → refuse; held-without-a-job-id →
    /// refuse. The stop paths carry the SHORT job id.
    #[test]
    fn reply_gate_routes_by_agent_state() {
        // Not held at all -> plain in-place reply.
        assert_eq!(reply_gate(None), ReplyGate::Reply);

        // done -> stop then reply, straight to compose, carrying the job id.
        assert_eq!(
            reply_gate(Some(&bg("done", Some("job-1")))),
            ReplyGate::StopThenReply {
                job_id: "job-1".to_string()
            }
        );

        // A terminal (stopped/failed) agent is over just like `done`, so it takes
        // the same stop-then-reply path — stopping a dead job is harmless.
        for terminal in ["stopped", "failed"] {
            assert_eq!(
                reply_gate(Some(&bg(terminal, Some("job-1")))),
                ReplyGate::StopThenReply {
                    job_id: "job-1".to_string()
                },
                "{terminal:?} is terminal -> stop then reply like done"
            );
        }

        // needs input (blocked / waiting) -> CONFIRM before stopping a live agent.
        for waiting in ["blocked", "waiting"] {
            assert_eq!(
                reply_gate(Some(&bg(waiting, Some("job-2")))),
                ReplyGate::ConfirmStopThenReply {
                    job_id: "job-2".to_string()
                },
                "{waiting:?} must confirm before stopping"
            );
        }

        // working / idle / unknown -> refuse (stopping would interrupt live work).
        for busy in ["working", "busy", "idle", "compacting"] {
            assert_eq!(
                reply_gate(Some(&bg(busy, Some("job-3")))),
                ReplyGate::Refuse(SEND_LIVE_REFUSED),
                "{busy:?} must refuse"
            );
        }

        // Held but no stoppable job id (e.g. an interactive session) -> refuse.
        assert_eq!(
            reply_gate(Some(&bg("done", None))),
            ReplyGate::Refuse(SEND_LIVE_REFUSED),
            "a done agent with no job id cannot be stopped -> refuse"
        );
        assert_eq!(
            reply_gate(Some(&bg("blocked", Some("   ")))),
            ReplyGate::Refuse(SEND_LIVE_REFUSED),
            "a blank job id is not stoppable -> refuse"
        );
    }

    /// The interrupt gate has the OPPOSITE intent to the reply gate: it exists to
    /// stop live work, so `working` is a valid target (Confirm), not a refusal.
    /// Not held → refuse (nothing to stop); no/blank job id → refuse (interactive);
    /// `done` → stop immediately; every other live state → confirm. Stop paths carry
    /// the SHORT job id.
    #[test]
    fn interrupt_gate_routes_by_agent_state() {
        // Not held at all -> nothing to stop.
        assert_eq!(
            interrupt_gate(None),
            InterruptGate::Refuse(INTERRUPT_NOT_LIVE)
        );

        // done -> stop immediately (harmless), carrying the job id.
        assert_eq!(
            interrupt_gate(Some(&bg("done", Some("job-1")))),
            InterruptGate::StopNow {
                job_id: "job-1".to_string()
            }
        );

        // A terminal (stopped/failed) agent is already over, so it stops
        // immediately like `done` rather than confirming.
        for terminal in ["stopped", "failed"] {
            assert_eq!(
                interrupt_gate(Some(&bg(terminal, Some("job-1")))),
                InterruptGate::StopNow {
                    job_id: "job-1".to_string()
                },
                "{terminal:?} is terminal -> stop immediately like done"
            );
        }

        // Every OTHER live state confirms first — including `working`, which the
        // reply gate refuses. This is the interrupt's whole point.
        for live in [
            "working",
            "busy",
            "idle",
            "compacting",
            "blocked",
            "waiting",
        ] {
            assert_eq!(
                interrupt_gate(Some(&bg(live, Some("job-2")))),
                InterruptGate::Confirm {
                    job_id: "job-2".to_string()
                },
                "{live:?} must confirm before stopping"
            );
        }

        // Live but no stoppable job id (interactive) -> refuse with the right hint.
        assert_eq!(
            interrupt_gate(Some(&bg("working", None))),
            InterruptGate::Refuse(INTERRUPT_NO_JOB_ID),
            "an interactive live session has no job id -> refuse"
        );
        assert_eq!(
            interrupt_gate(Some(&bg("done", Some("   ")))),
            InterruptGate::Refuse(INTERRUPT_NO_JOB_ID),
            "a blank job id is not stoppable -> refuse"
        );
    }

    /// A clean stop is the neutral success; a NON-ZERO stop surfaces claude's own
    /// reason (never a false `"stopped"`), with the duplicated `Error:` label
    /// stripped; an empty failure degrades to the generic message.
    #[test]
    fn status_for_stop_maps_success_and_failure() {
        assert_eq!(status_for_stop(true, "", ""), STOP_OK);

        let status = status_for_stop(false, "", "Error: No job matching 70933ea6");
        assert!(
            status.starts_with(STOP_FAILED_PREFIX),
            "a failed stop must read as a failure: {status}"
        );
        assert!(
            status.contains("No job matching"),
            "it must quote claude's own reason: {status}"
        );
        assert!(
            !status.contains("Error:"),
            "the Error: label is stripped: {status}"
        );
        assert!(
            !status.contains("stopped"),
            "a failed stop must NEVER read as stopped: {status}"
        );

        assert_eq!(status_for_stop(false, "   \n", "  \n"), STOP_FAILED_GENERIC);
    }

    /// The honesty seam: a NON-ZERO send exit must surface claude's OWN reason, not
    /// the neutral success — this is the false-`"sent"` regression, pinned. A clean
    /// exit still maps the JSON payload (cost / neutral) as before.
    #[test]
    fn a_failed_send_surfaces_the_reason_not_a_false_sent() {
        // The real wire failure: claude refuses to resume a held agent, exiting
        // non-zero with the reason on stderr and NOTHING on stdout.
        let stderr = "Error: Session abc is currently running as a background agent \
                      (bg). Use `claude agents` to find and attach to it, or add \
                      --fork-session to branch off a copy.";
        let status = status_for_output(false, "", stderr);
        assert!(
            status.starts_with(SEND_FAILED_PREFIX),
            "a failed send must read as a failure, got {status:?}"
        );
        assert!(
            status.contains("running as a background agent"),
            "it must quote claude's own reason: {status}"
        );
        assert!(
            !status.contains("sent"),
            "a failed send must NEVER read as sent: {status}"
        );
        // The duplicated `Error:` label is stripped (no `send failed: Error: …`).
        assert!(
            !status.contains("Error:"),
            "the Error: label is stripped: {status}"
        );

        // A clean exit is unchanged: the cost still comes through.
        let ok = status_for_output(true, r#"{"is_error":false,"total_cost_usd":0.0136}"#, "");
        assert_eq!(ok, "sent — $0.0136");
    }

    /// A non-zero exit with NO readable stdout/stderr degrades to the generic
    /// failure — still never a false success — and an `is_error` JSON payload is
    /// preferred over stderr when present.
    #[test]
    fn failed_send_fallbacks_and_is_error_precedence() {
        assert_eq!(
            status_for_output(false, "", "   \n  \n"),
            SEND_FAILED_GENERIC
        );
        assert_eq!(
            status_for_output(false, "not json", ""),
            SEND_FAILED_GENERIC
        );
        let from_json = status_for_output(
            false,
            r#"{"is_error":true,"result":"tool exploded"}"#,
            "some stderr noise",
        );
        assert_eq!(from_json, "send failed: tool exploded");
    }

    /// A raw ANSI escape / control chars from claude's stderr must never reach the
    /// status verbatim (TERMINAL-SAFE STYLING): the sequence is stripped, leaving
    /// only the readable text, whitespace collapsed.
    #[test]
    fn failed_send_strips_ansi_and_control_chars_from_the_reason() {
        let stderr = "\u{1b}[33mError: it \t broke\u{1b}[39m\nsecond line";
        let status = status_for_output(false, "", stderr);
        assert_eq!(status, "send failed: it broke");
        assert!(
            !status.contains('\u{1b}') && !status.contains('['),
            "no escape residue may remain: {status:?}"
        );
    }

    /// A resumable session file whose IN-FILE `cwd` exists on this host, so
    /// `plan_send` reaches `Ready`. Returns the file path and its temp dir (the
    /// authoritative cwd) to clean up. Mirrors `resume`'s `resumable_session`.
    fn resumable_file(tag: &str, id: &str) -> (PathBuf, PathBuf) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "snapback-send-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create the temp cwd");
        let file = dir.join(format!("{id}.jsonl"));
        std::fs::write(
            &file,
            format!(
                r#"{{"type":"user","sessionId":"{id}","cwd":"{cwd}","message":{{"role":"user","content":"hi"}}}}"#,
                id = id,
                cwd = dir.display(),
            ),
        )
        .expect("write the resumable fixture");
        (file, dir)
    }

    /// `plan_send` re-reads the AUTHORITATIVE `(cwd, session_id)` from inside the
    /// file and proceeds when the cwd exists — the send counterpart of
    /// `resume`'s existence-proceed test.
    #[test]
    fn plan_send_proceeds_reading_the_authoritative_cwd_and_id_from_the_file() {
        let (file, dir) = resumable_file("ready", "sess-in-file");
        match plan_send(&file) {
            SendPlan::Ready { cwd, session_id } => {
                assert_eq!(cwd, dir, "the cwd is the one read from inside the file");
                assert_eq!(session_id, "sess-in-file");
            }
            SendPlan::Refuse(msg) => panic!("an existing cwd must proceed: {msg}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `plan_send` REFUSES (status only, no send) when the session's `cwd` no
    /// longer exists — analogous to `resume::plan_refuses_a_session_whose_cwd_is_gone`.
    #[test]
    fn plan_send_refuses_a_session_whose_cwd_is_gone() {
        let (file, dir) = resumable_file("gone", "sess-gone");
        // Delete the cwd out from under the session, leaving only the file we read.
        let stashed = std::env::temp_dir().join(format!(
            "snapback-send-stash-{}-{}.jsonl",
            std::process::id(),
            "gone"
        ));
        std::fs::copy(&file, &stashed).expect("stash the transcript");
        std::fs::remove_dir_all(&dir).expect("delete the cwd");
        match plan_send(&stashed) {
            SendPlan::Refuse(message) => {
                assert!(message.contains("no longer exists"), "{message}");
            }
            SendPlan::Ready { .. } => panic!("a missing cwd must refuse"),
        }
        let _ = std::fs::remove_file(&stashed);
    }

    /// A file with no `cwd` (a sidecar) refuses rather than guessing.
    #[test]
    fn plan_send_refuses_a_file_with_no_cwd() {
        let dir = std::env::temp_dir().join(format!(
            "snapback-send-nocwd-{}-{}",
            std::process::id(),
            "x"
        ));
        std::fs::create_dir_all(&dir).expect("create dir");
        let file = dir.join("agent-title.jsonl");
        std::fs::write(&file, r#"{"type":"agent-name","agentName":"whatever"}"#).expect("write");
        match plan_send(&file) {
            SendPlan::Refuse(message) => assert!(message.contains("refusing to send"), "{message}"),
            SendPlan::Ready { .. } => panic!("a sidecar with no cwd must refuse"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
