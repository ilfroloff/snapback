//! Moving a session to another folder WITHOUT leaving the board: what `Ctrl-X w`
//! does once its picker has a target.
//!
//! One headless `claude` child per move ([`build_set_cwd_argv`]: `-p` with the
//! stream-json control protocol and `-r <id>`) starts in the session's CURRENT
//! folder, reads ONE `set_cwd` control request ([`set_cwd_request_line`]) and
//! answers it; its stdin is then closed and it exits. claude itself moves the
//! transcript (and its sibling `<id>/` dir) into the target's project folder and
//! appends a `relocated` record, which the parser already reads
//! (`store::parse`). snapback writes nothing: the move is a delegated write by a
//! `claude` child (AGENTS.md STORE WRITES).
//!
//! The request is UNDOCUMENTED. Its wire shape, the answers it gives and what a
//! move leaves in the transcript were observed on `claude 2.1.291` (2026-10-08,
//! throwaway `CLAUDE_CONFIG_DIR`); those facts live in `docs/agents/CLAUDE_CLI.md`
//! ("`set_cwd`: moving a session to another folder"), not here. Two shapes there
//! answer `ok` while moving nothing, which is why success is `status == "ok"` AND
//! `changed == true` ([`parse_set_cwd_response`]) and why the argv never carries
//! `--no-session-persistence`. It carries no `--model`/`--effort` either: no
//! model turn is taken (AGENTS.md A MODEL IS PICKED PER COMPOSE).
//!
//! It SHELLS OUT, so it lives outside `src/store/`, and everything that blocks
//! runs on the move's own worker thread ([`spawn_move`]): the authoritative
//! re-read of the transcript, the folder pre-checks ([`check_target`]), the
//! liveness probe, claude's workspace-trust read and the child itself. The key
//! handler only packs a [`MoveRequest`]; the worker reports back with exactly one
//! [`AppEvent::MoveFinished`] (AGENTS.md OFF-UI-THREAD), through
//! [`UndeliveredEvents`] so a hand-off cannot lose it.
//!
//! The child machinery is the catalog fetch's
//! ([`claude_catalog::exchange_reaped`]: pipes only, a process group of its own,
//! one deadline, kill and reap — AGENTS.md TERMINAL SAFETY) with this module's
//! reply parser, and the untrusted form is the catalog's too
//! ([`claude_catalog::trust_flags`], [`claude_catalog::build_catalog_env`]). The
//! reply is parsed FAIL-SOFT over `serde_json::Value`.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use crate::agents;
use crate::claude_catalog::{self, NoAnswer, CLAUDE_PROGRAM, CONTROL_RESPONSE_MARKER};
use crate::claude_trust::{self, FolderTrust};
use crate::resume::{self, ResumePlan};
use crate::send::{sanitize_status, UndeliveredEvents};
use crate::watch::AppEvent;
use crate::worktrees;

/// The `request_id` stamped on the one `set_cwd` request. claude echoes it in its
/// `control_response`, so a response carrying any other id answers some other
/// request and is not the move's.
const MOVE_REQUEST_ID: &str = "snapback-move";

/// The control request's `subtype`: claude's working-directory setter, an
/// undocumented stream-json request (claude 2.1.291; CLAUDE_CLI.md).
const SET_CWD_SUBTYPE: &str = "set_cwd";

/// The `--settings` JSON the move runs under. A move is no turn the user
/// started, so no hook should run for it: with it, no hook record was appended to
/// the moved transcript (claude 2.1.291, 2026-10-08).
const MOVE_SETTINGS: &str = r#"{"disableAllHooks":true}"#;

/// `response.subtype` of a reply claude ANSWERED; any other subtype (claude's
/// `error`) is a protocol failure.
const SUBTYPE_SUCCESS: &str = "success";

/// The answer's `status` when claude accepted the target. Not proof of a move on
/// its own: only `changed: true` beside it is ([`parse_set_cwd_response`]).
const STATUS_OK: &str = "ok";

/// The answer's `status` when claude has not trusted the target yet (it never
/// asks in `-p` mode, so it refuses instead).
const STATUS_NEEDS_TRUST: &str = "needs_trust";

/// The answer's `status` when claude refused the target, with a `reason`
/// (`unsafe_path`, `not_found`, `not_a_directory`, `blocked_by_rule`, `busy` at
/// claude 2.1.291) and a `message`.
const STATUS_REJECTED: &str = "rejected";

/// How long one move may take: the answer, the close of stdout AND the child's
/// exit together. Past it the child's process group and the child are killed and
/// the child reaped. Measured (claude 2.1.291, 2026-10-08): ~0.7 s end to end on
/// a small transcript. `-r` loads the whole transcript first, so a long one takes
/// longer, and 30 s only ever cuts off a stuck child. Only the move's own worker
/// waits on it.
const MOVE_TIMEOUT: Duration = Duration::from_secs(30);

/// Refusal (worker): claude's one-shot probe lists the session as active, so a
/// process may be writing its transcript and moving the file would split it.
pub const MOVE_LIVE_REFUSAL: &str = "claude lists this session as active. Close it (or stop it \
     with Ctrl-K) before moving it.";

/// Refusal (worker): the liveness probe could not answer — `claude agents --json`
/// did not start, exited non-zero, or printed no readable list — so whether a
/// process is writing the transcript is unknown. The move alone fails toward
/// REFUSING here, never toward "not live" ([`liveness_refusal`]); worded for what
/// was observed, not for a cause.
pub const MOVE_PROBE_FAILED_REFUSAL: &str = "snapback could not ask claude whether this \
     session is active (claude agents --json failed), so nothing moved.";

/// Refusal (`Ctrl-X w`): snapback's OWN quick reply is in flight to the session,
/// the writer claude's probe cannot be relied on to see. Names snapback, not
/// claude, because that is the writer observed (see
/// `delete::DELETE_SENDING_REFUSAL`).
pub const MOVE_SENDING_REFUSAL: &str = "snapback is still sending a reply to this session — \
     wait for it to land, then move it.";

/// Refusal (a second `Ctrl-X w`): this session's own move is still in flight, and
/// a second child would race the first for the same file.
pub const MOVE_IN_FLIGHT_REFUSAL: &str = "snapback is already moving this session — wait for \
     it to finish.";

/// Refusal (`Enter` / `Ctrl-F`): the session's move is in flight, so its file is
/// being renamed into another folder while `claude -r` would read it.
pub const MOVING_RESUME_REFUSAL: &str = "snapback is still moving this session to another \
     folder — wait for it to finish, then resume or fork it.";

/// Refusal (`Ctrl-R`): the session's move is in flight, and a `claude -p -r`
/// reply would append to a file another child is moving.
pub const MOVING_REPLY_REFUSAL: &str = "snapback is still moving this session to another \
     folder — wait for it to finish, then reply.";

/// Refusal (worker): the target folder no longer exists — a worktree can be
/// removed while the picker is open.
pub const MOVE_TARGET_GONE: &str = "The target folder no longer exists, so the session \
     cannot be moved there.";

/// Refusal (worker): the target has no usable spelling (relative or not UTF-8);
/// the request must carry exactly the folder git reported, as a JSON string.
pub const MOVE_TARGET_UNUSABLE: &str = "The target folder has no usable absolute path, so the \
     session cannot be moved there.";

/// Refusal (worker): the session already sits in the target. claude would answer
/// `ok` with `changed: false` and move nothing.
pub const MOVE_ALREADY_THERE: &str = "This session is already in that folder.";

/// Refusal (worker): the authoritative re-read ([`resume::plan_at`]) found no
/// folder to move the session FROM — the transcript is gone, unreadable or names
/// no `cwd`, or the folder it names no longer exists. It stands in for
/// `plan_at`'s own refusal, which is worded for a resume, spans several lines and
/// quotes a path from the transcript unsanitized.
pub const MOVE_SOURCE_GONE: &str = "This session's current folder no longer exists (or its \
     transcript could not be read), so nothing moved.";

/// The success status's lead, followed by the folder claude moved the session to.
const MOVED_PREFIX: &str = "moved to ";

/// Status when claude answered `ok` but `changed` was not `true`: nothing moved.
const MOVE_UNCHANGED: &str = "claude accepted the move but moved nothing — the session stays \
     where it was.";

/// Status when claude refused the move with no readable reason or message.
const MOVE_REJECTED_GENERIC: &str = "claude refused the move.";

/// Status when claude answered the request with an error, or with a body this
/// module cannot read, and gave no readable text.
const MOVE_PROTOCOL_GENERIC: &str = "claude could not run the move — this claude may not \
     support it.";

/// Status when claude closed its output without answering the request.
const MOVE_NO_ANSWER: &str = "claude exited without answering the move — the board shows \
     where the session is now.";

/// Status when the child outlived [`MOVE_TIMEOUT`] and was killed.
const MOVE_TIMED_OUT: &str = "claude did not finish the move in time and was stopped — the \
     board shows where the session is now.";

/// Status when no child could be started: the session's current folder, which
/// the child runs in, vanished between the pre-check and the spawn. A missing
/// `claude` is caught earlier, by the liveness probe
/// ([`MOVE_PROBE_FAILED_REFUSAL`]).
const MOVE_SPAWN_FAILED: &str = "could not start claude to move the session.";

/// One confirmed `Ctrl-X w` pick, packed by the key handler for the worker.
/// Everything that needs the disk or a child is the worker's: the request
/// carries only what the board already held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveRequest {
    /// The board's id for the row, the key `App::moving` holds the move under and
    /// [`AppEvent::MoveFinished`] carries back.
    pub session_id: String,
    /// The transcript, re-read on the worker for the authoritative `cwd` and
    /// `sessionId` (AGENTS.md AUTHORITATIVE-FROM-FILE).
    pub file: PathBuf,
    /// The chosen folder, as the picker's choice carried it.
    pub target: PathBuf,
}

/// What one move came to: claude's answer, or why there was none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveOutcome {
    /// `status: "ok"` AND `changed: true`: the transcript moved. `cwd` is the
    /// folder claude reports, when it is a non-blank string.
    Moved {
        /// The folder claude says the session is now in.
        cwd: Option<String>,
    },
    /// `status: "ok"` with `changed` anything but the JSON boolean `true`: claude
    /// moved nothing. A failure, never a success.
    Unchanged,
    /// `status: "needs_trust"`: claude has not trusted the target. snapback never
    /// claims trust on the user's behalf.
    NeedsTrust {
        /// The folder claude names.
        directory: Option<String>,
        /// The folder whose trust would cover it, when claude names one.
        trust_root: Option<String>,
    },
    /// `status: "rejected"`.
    Rejected {
        /// claude's machine reason (`unsafe_path`, `not_found`, …).
        reason: Option<String>,
        /// claude's own sentence.
        message: Option<String>,
    },
    /// The reply to THIS request was claude's `error`, or a success body with no
    /// known `status`.
    ProtocolError {
        /// claude's error text, when it gave one.
        error: Option<String>,
    },
    /// The child closed its output with no reply to this request.
    NoAnswer,
    /// [`MOVE_TIMEOUT`] passed first; the child was killed.
    TimedOut,
    /// No child could be started.
    SpawnFailed,
    /// Refused before any child ran: the session's folder is gone (or its
    /// transcript unreadable), the target is unusable, gone or the same folder,
    /// claude lists the session as active, or claude could not be asked.
    Refused(String),
}

/// The move's argv, exactly: `claude -p --input-format stream-json
/// --output-format stream-json --verbose --strict-mcp-config --settings
/// {"disableAllHooks":true} -r <session_id>`, followed by
/// [`claude_catalog::trust_flags`] (`--setting-sources user`) unless the
/// session's current folder is [`FolderTrust::Trusted`].
///
/// - `--input-format` / `--output-format stream-json` carry the control
///   protocol; `--verbose` is required with them.
/// - `--strict-mcp-config` starts no MCP server, and `--settings`
///   [`MOVE_SETTINGS`] runs no hook.
/// - NEVER `--no-session-persistence`: with it claude answers `ok`,
///   `changed: true` while the transcript stays put (claude 2.1.291).
/// - NEVER `--model` / `--effort`: there is no parameter to pass one, and no
///   model turn is taken.
#[must_use]
pub fn build_set_cwd_argv(session_id: &str, trust: FolderTrust) -> Vec<String> {
    [
        CLAUDE_PROGRAM,
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--strict-mcp-config",
        "--settings",
        MOVE_SETTINGS,
        "-r",
        session_id,
    ]
    .iter()
    .chain(claude_catalog::trust_flags(trust))
    .map(|word| (*word).to_owned())
    .collect()
}

/// The one stdin line of a move: a `set_cwd` control request for `target`,
/// stamped with [`MOVE_REQUEST_ID`] and newline-terminated. `target` travels as a
/// JSON string, so a space (or any other character) needs no quoting.
#[must_use]
pub fn set_cwd_request_line(target: &str) -> String {
    let request = json!({
        "type": "control_request",
        "request_id": MOVE_REQUEST_ID,
        "request": { "subtype": SET_CWD_SUBTYPE, "path": target },
    });
    format!("{request}\n")
}

/// claude's answer to the move carried by ONE stdout line, or `None` when the
/// line is not a reply to [`set_cwd_request_line`] at all (noise, a malformed
/// line, a reply to another request): FAIL-SOFT, the line is skipped.
///
/// The line must be a `control_response` whose `response.request_id` is
/// [`MOVE_REQUEST_ID`]. Then:
/// - `response.subtype` other than `success` → [`MoveOutcome::ProtocolError`]
///   carrying `response.error`;
/// - the body `response.response` must be an object whose `status` is
///   [`STATUS_OK`] (→ [`MoveOutcome::Moved`] only when `changed` is the JSON
///   boolean `true`, else [`MoveOutcome::Unchanged`]), [`STATUS_NEEDS_TRUST`] or
///   [`STATUS_REJECTED`]; any other body is a [`MoveOutcome::ProtocolError`].
///
/// Optional string fields that are absent, not strings or blank read as `None`.
#[must_use]
pub fn parse_set_cwd_response(line: &str) -> Option<MoveOutcome> {
    if !line.contains(CONTROL_RESPONSE_MARKER) {
        return None;
    }
    let value: Value = serde_json::from_str(line).ok()?;
    if value.get("type").and_then(Value::as_str) != Some(CONTROL_RESPONSE_MARKER) {
        return None;
    }
    let response = value.get("response")?;
    if response.get("request_id").and_then(Value::as_str) != Some(MOVE_REQUEST_ID) {
        return None;
    }
    if response.get("subtype").and_then(Value::as_str) != Some(SUBTYPE_SUCCESS) {
        return Some(MoveOutcome::ProtocolError {
            error: text(response, "error"),
        });
    }
    let Some(body) = response.get("response").filter(|body| body.is_object()) else {
        return Some(MoveOutcome::ProtocolError { error: None });
    };
    Some(match body.get("status").and_then(Value::as_str) {
        Some(STATUS_OK) if body.get("changed") == Some(&Value::Bool(true)) => MoveOutcome::Moved {
            cwd: text(body, "cwd"),
        },
        Some(STATUS_OK) => MoveOutcome::Unchanged,
        Some(STATUS_NEEDS_TRUST) => MoveOutcome::NeedsTrust {
            directory: text(body, "directory"),
            trust_root: text(body, "trust_root"),
        },
        Some(STATUS_REJECTED) => MoveOutcome::Rejected {
            reason: text(body, "reason"),
            message: text(body, "message"),
        },
        _ => MoveOutcome::ProtocolError { error: None },
    })
}

/// `value[key]` as a trimmed, non-blank string, or `None`.
fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// The worker's pre-checks on the chosen `target`, against the session's
/// authoritative CURRENT folder `current` (already known to exist,
/// [`resume::plan_at`]): the target must have an absolute UTF-8 spelling
/// ([`MOVE_TARGET_UNUSABLE`]), still be a folder ([`MOVE_TARGET_GONE`]) and not
/// resolve to `current` ([`MOVE_ALREADY_THERE`]). Returns the spelling the
/// request carries.
///
/// Reads the filesystem (`is_dir`, the one `worktrees::resolve_dir` the scope
/// compares with), so only the worker calls it.
pub fn check_target<'a>(current: &Path, target: &'a Path) -> Result<&'a str, &'static str> {
    let spelled = match target.to_str() {
        Some(spelled) if target.is_absolute() => spelled,
        _ => return Err(MOVE_TARGET_UNUSABLE),
    };
    if !target.is_dir() {
        return Err(MOVE_TARGET_GONE);
    }
    if worktrees::resolve_dir(target) == worktrees::resolve_dir(current) {
        return Err(MOVE_ALREADY_THERE);
    }
    Ok(spelled)
}

/// The move's verdict on the liveness probe's answer for the session: `None`
/// lets the move go on, `Some` is the refusal.
///
/// `probe` is `Some(true)` when claude lists the session as active
/// ([`MOVE_LIVE_REFUSAL`]), `Some(false)` when it answered without it, and
/// `None` when the probe could not answer ([`MOVE_PROBE_FAILED_REFUSAL`]). That
/// last arm is the move's alone: every other gate reads a failed probe as "not
/// live" (`agents::live_agents`), and the move REFUSES instead, since it renames
/// a transcript a live process may be writing (DOMAIN.md "Reported agents").
#[must_use]
pub fn liveness_refusal(probe: Option<bool>) -> Option<&'static str> {
    match probe {
        Some(false) => None,
        Some(true) => Some(MOVE_LIVE_REFUSAL),
        None => Some(MOVE_PROBE_FAILED_REFUSAL),
    }
}

/// The board status a finished move reports, and its class: `true` (a transient
/// confirmation) for [`MoveOutcome::Moved`] alone, `false` (sticky until the next
/// keypress) for every other outcome. `target` is the folder the move asked for.
///
/// Every string from claude's answer, and the path git reported, passes through
/// `send::sanitize_status`, so no escape reaches the status line. A
/// [`MoveOutcome::Refused`] is snapback's own wording and is shown as is.
#[must_use]
pub fn status_for_move(outcome: &MoveOutcome, target: &Path) -> (String, bool) {
    let target = sanitize_status(&target.to_string_lossy());
    let quoted = |s: &Option<String>| s.as_deref().map(sanitize_status);
    match outcome {
        MoveOutcome::Moved { cwd } => (
            format!("{MOVED_PREFIX}{}", quoted(cwd).unwrap_or(target)),
            true,
        ),
        MoveOutcome::Unchanged => (MOVE_UNCHANGED.to_owned(), false),
        MoveOutcome::NeedsTrust {
            directory,
            trust_root,
        } => {
            let folder = quoted(trust_root)
                .or_else(|| quoted(directory))
                .unwrap_or_else(|| target.clone());
            (
                format!(
                    "claude has not trusted {folder} yet, so nothing moved — press Enter, then \
                     run /cd {target} once so claude can ask."
                ),
                false,
            )
        }
        MoveOutcome::Rejected { reason, message } => {
            let status = match (quoted(message), quoted(reason)) {
                (Some(message), Some(reason)) => {
                    format!("claude refused the move ({reason}): {message}")
                }
                (Some(said), None) | (None, Some(said)) => {
                    format!("claude refused the move: {said}")
                }
                (None, None) => MOVE_REJECTED_GENERIC.to_owned(),
            };
            (status, false)
        }
        MoveOutcome::ProtocolError { error } => (
            quoted(error).map_or_else(
                || MOVE_PROTOCOL_GENERIC.to_owned(),
                |error| format!("claude could not run the move: {error}"),
            ),
            false,
        ),
        MoveOutcome::NoAnswer => (MOVE_NO_ANSWER.to_owned(), false),
        MoveOutcome::TimedOut => (MOVE_TIMED_OUT.to_owned(), false),
        MoveOutcome::SpawnFailed => (MOVE_SPAWN_FAILED.to_owned(), false),
        // Always one of this module's consts: no foreign text to sanitize.
        MoveOutcome::Refused(refusal) => (refusal.clone(), false),
    }
}

/// Run one move to its outcome, in order: the authoritative re-read and the
/// "current folder exists" gate ([`resume::plan_at`], whose refusal becomes
/// [`MOVE_SOURCE_GONE`]), [`check_target`], the
/// liveness probe `is_live` (for the AUTHORITATIVE id; `None` when it could not
/// answer, judged by [`liveness_refusal`]), claude's trust verdict
/// `trust_of` for the CURRENT folder, then the child — `program` followed by the
/// rest of [`build_set_cwd_argv`], with [`claude_catalog::build_catalog_env`]
/// for the same verdict — spawned IN the current folder, never the target: run
/// from the target, claude answers `ok`, `changed: false` and moves nothing.
///
/// The probe runs after the pre-checks and right before the trust read and the
/// spawn, so the window it leaves is short. Blocking throughout; only
/// [`spawn_move_in`]'s worker calls it.
/// `is_live`, `trust_of`, `program` and `timeout` are parameters so the suite
/// can state them and run a stand-in child; [`spawn_move`] names the real ones.
fn run_move<L, T>(
    req: &MoveRequest,
    is_live: L,
    trust_of: T,
    program: &[String],
    timeout: Duration,
) -> MoveOutcome
where
    L: FnOnce(&str) -> Option<bool>,
    T: FnOnce(&Path) -> FolderTrust,
{
    let (cwd, session_id) = match resume::plan_at(&req.file, false) {
        ResumePlan::Ready {
            cwd, session_id, ..
        } => (cwd, session_id),
        ResumePlan::Refuse { .. } => return MoveOutcome::Refused(MOVE_SOURCE_GONE.to_owned()),
    };
    let target = match check_target(&cwd, &req.target) {
        Ok(spelled) => spelled,
        Err(refusal) => return MoveOutcome::Refused(refusal.to_owned()),
    };
    if let Some(refusal) = liveness_refusal(is_live(&session_id)) {
        return MoveOutcome::Refused(refusal.to_owned());
    }
    let trust = trust_of(&cwd);
    let argv: Vec<String> = program
        .iter()
        .cloned()
        .chain(build_set_cwd_argv(&session_id, trust).into_iter().skip(1))
        .collect();
    let exchanged = claude_catalog::exchange_reaped(
        &argv,
        claude_catalog::build_catalog_env(trust),
        &cwd,
        &set_cwd_request_line(target),
        timeout,
        parse_set_cwd_response,
    );
    match exchanged.answer {
        Ok(outcome) => outcome,
        Err(NoAnswer::Spawn) => MoveOutcome::SpawnFailed,
        Err(NoAnswer::Closed) => MoveOutcome::NoAnswer,
        Err(NoAnswer::TimedOut) => MoveOutcome::TimedOut,
    }
}

/// Run `req` on a thread of its own and deliver exactly one
/// [`AppEvent::MoveFinished`] carrying `req.session_id` back. The ONE site that
/// names the real liveness probe (`agents::try_live_agents`: the bare active
/// list every hand-off asks, read so that a probe that could not answer is
/// `None`), trust reader (`claude_trust::folder_trust`), program and timeout.
pub fn spawn_move(req: MoveRequest, tx: Sender<AppEvent>, undelivered: UndeliveredEvents) {
    spawn_move_in(
        req,
        tx,
        undelivered,
        |id: &str| agents::try_live_agents().map(|live| live.contains_key(id)),
        claude_trust::folder_trust,
        vec![CLAUDE_PROGRAM.to_owned()],
        MOVE_TIMEOUT,
    );
}

/// [`spawn_move`] over a stated `is_live`, `trust_of`, `program` and `timeout`:
/// one worker thread runs [`run_move`], the probe and the trust read included.
/// Both block, so they must stay INSIDE the worker; hoisting either to the
/// spawning (UI) thread is the regression the suite pins here.
fn spawn_move_in<L, T>(
    req: MoveRequest,
    tx: Sender<AppEvent>,
    undelivered: UndeliveredEvents,
    is_live: L,
    trust_of: T,
    program: Vec<String>,
    timeout: Duration,
) where
    L: FnOnce(&str) -> Option<bool> + Send + 'static,
    T: FnOnce(&Path) -> FolderTrust + Send + 'static,
{
    spawn_move_with(req, tx, undelivered, move |req| {
        run_move(req, is_live, trust_of, &program, timeout)
    });
}

/// [`spawn_move`] over a stated `run`: a one-shot thread that runs it, maps its
/// outcome with [`status_for_move`] and delivers one [`AppEvent::MoveFinished`]
/// through `undelivered` — onto the board's channel while it is up, into the
/// queue the next board replays once it is not. `run` is a SEAM production swaps
/// exactly never, so the suite can state an outcome instead of spawning `claude`.
fn spawn_move_with<F>(
    req: MoveRequest,
    tx: Sender<AppEvent>,
    undelivered: UndeliveredEvents,
    run: F,
) where
    F: FnOnce(&MoveRequest) -> MoveOutcome + Send + 'static,
{
    thread::spawn(move || {
        let outcome = run(&req);
        let (status, success) = status_for_move(&outcome, &req.target);
        undelivered.deliver(
            &tx,
            AppEvent::MoveFinished {
                session_id: req.session_id,
                status,
                success,
            },
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::time::Instant;

    /// A reply line to the move's request carrying `body` spliced in raw.
    fn answer(body: &str) -> String {
        format!(
            r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"{MOVE_REQUEST_ID}","response":{body}}}}}"#
        )
    }

    /// A fresh, CANONICAL temp dir (on macOS `/var` is `/private/var`).
    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let base = std::fs::canonicalize(std::env::temp_dir()).expect("canonicalise the temp dir");
        let dir = base.join(format!(
            "snapback-claude-move-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create the test's temp dir");
        dir
    }

    // --- the argv, the environment and the request --------------------------

    #[test]
    fn the_move_argv_is_exact() {
        assert_eq!(
            build_set_cwd_argv("sess-1", FolderTrust::Trusted),
            [
                "claude",
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--strict-mcp-config",
                "--settings",
                r#"{"disableAllHooks":true}"#,
                "-r",
                "sess-1",
            ]
        );
    }

    /// The UNTRUSTED form, pinned because no probe covered it (CLAUDE_CLI.md):
    /// a session whose current folder claude does not trust is moved with the
    /// catalog's untrusted argv tail AND its child environment, spelled here as
    /// literals so a drift in either shared half fails here too.
    #[test]
    fn the_untrusted_move_reads_user_settings_only_with_git_prefetch_off() {
        assert_eq!(
            build_set_cwd_argv("sess-1", FolderTrust::Untrusted),
            [
                "claude",
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--strict-mcp-config",
                "--settings",
                r#"{"disableAllHooks":true}"#,
                "-r",
                "sess-1",
                "--setting-sources",
                "user",
            ]
        );
        assert_eq!(
            claude_catalog::build_catalog_env(FolderTrust::Untrusted),
            [("CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS", "1")]
        );
        assert_eq!(claude_catalog::build_catalog_env(FolderTrust::Trusted), []);
    }

    /// The two false-success shapes and the model rule, stated as membership: no
    /// form of the argv persists nothing or picks a model.
    #[test]
    fn no_move_argv_drops_persistence_or_picks_a_model() {
        for trust in [FolderTrust::Trusted, FolderTrust::Untrusted] {
            let argv = build_set_cwd_argv("sess-1", trust);
            for banned in ["--no-session-persistence", "--model", "--effort"] {
                assert!(!argv.iter().any(|w| w == banned), "{banned} in {argv:?}");
            }
        }
    }

    #[test]
    fn the_request_line_is_one_set_cwd_control_request() {
        let target = "/r/main/.agents/worktrees/with space";
        let line = set_cwd_request_line(target);
        assert!(line.ends_with('\n'), "claude reads one line: {line:?}");
        assert_eq!(line.matches('\n').count(), 1, "exactly one newline");
        let value: Value = serde_json::from_str(line.trim_end()).expect("the line is JSON");
        assert_eq!(
            value,
            json!({
                "type": "control_request",
                "request_id": "snapback-move",
                "request": { "subtype": "set_cwd", "path": target },
            })
        );
    }

    // --- the reply parser ----------------------------------------------------

    #[test]
    fn only_ok_with_changed_true_is_a_move() {
        assert_eq!(
            parse_set_cwd_response(&answer(
                r#"{"status":"ok","cwd":"/r/wt","changed":true,"transcript_relocated":true}"#
            )),
            Some(MoveOutcome::Moved {
                cwd: Some("/r/wt".to_owned())
            })
        );
        assert_eq!(
            parse_set_cwd_response(&answer(r#"{"status":"ok","changed":true}"#)),
            Some(MoveOutcome::Moved { cwd: None }),
            "a missing cwd still moved; the status falls back to the target"
        );
        for body in [
            r#"{"status":"ok","cwd":"/r/main","changed":false}"#,
            r#"{"status":"ok","cwd":"/r/main"}"#,
            r#"{"status":"ok","changed":"true"}"#,
            r#"{"status":"ok","changed":1}"#,
        ] {
            assert_eq!(
                parse_set_cwd_response(&answer(body)),
                Some(MoveOutcome::Unchanged),
                "{body}"
            );
        }
    }

    #[test]
    fn needs_trust_and_rejected_carry_claudes_words() {
        assert_eq!(
            parse_set_cwd_response(&answer(
                r#"{"status":"needs_trust","directory":"/x/wt","trust_root":"/x"}"#
            )),
            Some(MoveOutcome::NeedsTrust {
                directory: Some("/x/wt".to_owned()),
                trust_root: Some("/x".to_owned()),
            })
        );
        assert_eq!(
            parse_set_cwd_response(&answer(r#"{"status":"needs_trust","directory":"/x/wt"}"#)),
            Some(MoveOutcome::NeedsTrust {
                directory: Some("/x/wt".to_owned()),
                trust_root: None,
            })
        );
        for reason in [
            "unsafe_path",
            "not_found",
            "not_a_directory",
            "blocked_by_rule",
            "busy",
        ] {
            assert_eq!(
                parse_set_cwd_response(&answer(&format!(
                    r#"{{"status":"rejected","reason":"{reason}","message":"no: {reason}"}}"#
                ))),
                Some(MoveOutcome::Rejected {
                    reason: Some(reason.to_owned()),
                    message: Some(format!("no: {reason}")),
                }),
                "{reason}"
            );
        }
    }

    #[test]
    fn an_error_or_an_unreadable_body_for_this_request_is_a_protocol_error() {
        let error = format!(
            r#"{{"type":"control_response","response":{{"subtype":"error","request_id":"{MOVE_REQUEST_ID}","error":"Unsupported control request subtype: set_cwd"}}}}"#
        );
        assert_eq!(
            parse_set_cwd_response(&error),
            Some(MoveOutcome::ProtocolError {
                error: Some("Unsupported control request subtype: set_cwd".to_owned())
            })
        );
        for body in [r#"{"status":"maybe"}"#, r#"{}"#, "null", r#"["ok"]"#] {
            assert_eq!(
                parse_set_cwd_response(&answer(body)),
                Some(MoveOutcome::ProtocolError { error: None }),
                "{body}"
            );
        }
    }

    /// FAIL-SOFT: noise, malformed JSON and replies to other requests are
    /// skipped, never a verdict.
    #[test]
    fn noise_malformed_and_foreign_lines_are_skipped() {
        let foreign = r#"{"type":"control_response","response":{"subtype":"success","request_id":"snapback-catalog","response":{"status":"ok","changed":true}}}"#;
        let wrong_type = format!(
            r#"{{"type":"system","note":"control_response","response":{{"subtype":"success","request_id":"{MOVE_REQUEST_ID}","response":{{"status":"ok","changed":true}}}}}}"#
        );
        for line in [
            r#"{"type":"system","subtype":"init"}"#,
            r#"{"type":"control_response","response":{"subtype":"success""#,
            foreign,
            wrong_type.as_str(),
            r#"{"type":"control_response"}"#,
            "",
            "not json control_response",
        ] {
            assert_eq!(parse_set_cwd_response(line), None, "{line}");
        }
    }

    // --- the status line -----------------------------------------------------

    #[test]
    fn only_a_real_move_is_a_transient_success() {
        let target = Path::new("/r/main/.agents/worktrees/wt");
        assert_eq!(
            status_for_move(
                &MoveOutcome::Moved {
                    cwd: Some("/r/main/.agents/worktrees/wt".to_owned())
                },
                target
            ),
            ("moved to /r/main/.agents/worktrees/wt".to_owned(), true)
        );
        assert_eq!(
            status_for_move(&MoveOutcome::Moved { cwd: None }, target),
            ("moved to /r/main/.agents/worktrees/wt".to_owned(), true)
        );
        for failed in [
            MoveOutcome::Unchanged,
            MoveOutcome::NeedsTrust {
                directory: None,
                trust_root: None,
            },
            MoveOutcome::Rejected {
                reason: None,
                message: None,
            },
            MoveOutcome::ProtocolError { error: None },
            MoveOutcome::NoAnswer,
            MoveOutcome::TimedOut,
            MoveOutcome::SpawnFailed,
            MoveOutcome::Refused(MOVE_LIVE_REFUSAL.to_owned()),
        ] {
            let (status, success) = status_for_move(&failed, target);
            assert!(!success, "{failed:?} must stay sticky");
            assert!(!status.starts_with(MOVED_PREFIX), "{failed:?}: {status}");
        }
    }

    /// `needs_trust` is a refusal carrying the way through: resume the session,
    /// then `/cd` the target once, so claude itself asks.
    #[test]
    fn needs_trust_points_at_enter_then_cd_the_target() {
        let target = Path::new("/x/repo/.agents/worktrees/wt");
        let (status, success) = status_for_move(
            &MoveOutcome::NeedsTrust {
                directory: Some("/x/repo/.agents/worktrees/wt".to_owned()),
                trust_root: Some("/x/repo".to_owned()),
            },
            target,
        );
        assert!(!success);
        assert_eq!(
            status,
            "claude has not trusted /x/repo yet, so nothing moved — press Enter, then run \
             /cd /x/repo/.agents/worktrees/wt once so claude can ask."
        );
    }

    #[test]
    fn rejected_and_protocol_errors_quote_claude_sanitized() {
        let target = Path::new("/t");
        let (status, _) = status_for_move(
            &MoveOutcome::Rejected {
                reason: Some("busy".to_owned()),
                message: Some("Session is \u{1b}[1mbusy\u{1b}[0m\u{7}   now".to_owned()),
            },
            target,
        );
        assert_eq!(
            status,
            "claude refused the move (busy): Session is busy now"
        );
        let (status, _) = status_for_move(
            &MoveOutcome::Rejected {
                reason: Some("not_found".to_owned()),
                message: None,
            },
            target,
        );
        assert_eq!(status, "claude refused the move: not_found");
        let (status, _) = status_for_move(
            &MoveOutcome::ProtocolError {
                error: Some("Unsupported control request subtype: set_cwd".to_owned()),
            },
            target,
        );
        assert_eq!(
            status,
            "claude could not run the move: Unsupported control request subtype: set_cwd"
        );
        assert_eq!(
            status_for_move(&MoveOutcome::ProtocolError { error: None }, target).0,
            MOVE_PROTOCOL_GENERIC
        );
    }

    // --- the folder pre-checks -----------------------------------------------

    #[test]
    fn check_target_refuses_unusable_missing_and_identical_targets() {
        let root = temp_dir("check");
        let current = root.join("main");
        let spaced = root.join("wt with space");
        std::fs::create_dir_all(&current).expect("create current");
        std::fs::create_dir_all(&spaced).expect("create target");

        assert_eq!(
            check_target(&current, Path::new("relative/dir")),
            Err(MOVE_TARGET_UNUSABLE)
        );
        assert_eq!(
            check_target(&current, &root.join("gone")),
            Err(MOVE_TARGET_GONE)
        );
        assert_eq!(check_target(&current, &current), Err(MOVE_ALREADY_THERE));
        assert_eq!(
            check_target(&current, &current.join("..").join("main")),
            Err(MOVE_ALREADY_THERE),
            "the same folder by another spelling is still the same folder"
        );
        assert_eq!(
            check_target(&current, &spaced),
            Ok(spaced.to_str().expect("a UTF-8 temp path"))
        );
        std::fs::remove_dir_all(&root).ok();
    }

    // --- the liveness verdict ------------------------------------------------

    /// Decision 6a, as a pure verdict: only a probe that ANSWERED without the
    /// session lets the move go on. A live session and a probe that could not
    /// answer refuse in two distinct sentences, so the status line says which.
    #[test]
    fn only_an_answered_not_live_probe_lets_the_move_go_on() {
        assert_eq!(liveness_refusal(Some(false)), None);
        assert_eq!(liveness_refusal(Some(true)), Some(MOVE_LIVE_REFUSAL));
        assert_eq!(liveness_refusal(None), Some(MOVE_PROBE_FAILED_REFUSAL));
        assert_ne!(MOVE_PROBE_FAILED_REFUSAL, MOVE_LIVE_REFUSAL);
        let (status, success) = status_for_move(
            &MoveOutcome::Refused(MOVE_PROBE_FAILED_REFUSAL.to_owned()),
            Path::new("/r/wt"),
        );
        assert!(!success, "a refusal stays sticky");
        assert!(!status.contains('\n'), "one line: {status:?}");
    }

    // --- the worker ----------------------------------------------------------

    /// A move with no folder to move FROM — the transcript's `cwd` is gone, or the
    /// transcript itself is — refuses in the move's own single line, before the
    /// probe, the trust read and the child. Never resume's multi-line refusal,
    /// and never a path read out of the transcript: the `cwd` here carries an
    /// escape and a newline that would reach the status line as they are.
    #[test]
    fn a_gone_source_refuses_in_one_line_quoting_nothing_from_the_transcript() {
        let root = temp_dir("source-gone");
        let target = root.join("wt");
        std::fs::create_dir_all(&target).expect("create target");
        let store = root.join("store");
        std::fs::create_dir_all(&store).expect("create the store dir");
        let transcript = store.join("sess-gone.jsonl");
        let hostile_cwd = format!("{}/gone\u{1b}[31m\nred", root.display());
        let record = json!({
            "type": "user",
            "sessionId": "sess-gone",
            "cwd": hostile_cwd,
            "message": { "role": "user", "content": "hi" },
        });
        std::fs::write(&transcript, record.to_string()).expect("write the transcript");
        let never_live = |_: &str| -> Option<bool> { panic!("the probe must not run") };
        let never_trusted = |_: &Path| -> FolderTrust { panic!("the trust read must not run") };
        let never_spawned = vec!["snapback-test-never-spawned".to_owned()];

        for file in [transcript, store.join("vanished.jsonl")] {
            let req = MoveRequest {
                session_id: "sess-gone".to_owned(),
                file,
                target: target.clone(),
            };
            let outcome = run_move(
                &req,
                never_live,
                never_trusted,
                &never_spawned,
                Duration::from_secs(1),
            );
            assert_eq!(
                outcome,
                MoveOutcome::Refused(MOVE_SOURCE_GONE.to_owned()),
                "{}",
                req.file.display()
            );
            let (status, success) = status_for_move(&outcome, &target);
            assert!(!success, "a refusal stays sticky");
            assert!(
                !status.contains('\n') && !status.contains('\u{1b}'),
                "one line, no escape: {status:?}"
            );
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// A board's request for a transcript at `<root>/store/<id>.jsonl` whose `cwd`
    /// is `current`, moving to `target`.
    fn request_in(root: &Path, current: &Path, target: &Path) -> MoveRequest {
        let store = root.join("store");
        std::fs::create_dir_all(&store).expect("create the store dir");
        let file = store.join("sess-mv.jsonl");
        std::fs::write(
            &file,
            format!(
                r#"{{"type":"user","sessionId":"sess-mv","cwd":"{}","message":{{"role":"user","content":"hi"}}}}"#,
                current.display()
            ),
        )
        .expect("write the transcript");
        MoveRequest {
            session_id: "sess-mv".to_owned(),
            file,
            target: target.to_path_buf(),
        }
    }

    /// The worker delivers exactly one `MoveFinished`, mapped, for the asked row,
    /// and the spawn returns before the run does.
    #[test]
    fn the_worker_delivers_exactly_one_move_finished() {
        /// Longer than a spawn takes, so an INLINE run cannot beat it by luck.
        const RUN_BLOCKS: Duration = Duration::from_millis(300);
        const DELIVERED_WITHIN: Duration = Duration::from_secs(2);

        let (tx, rx) = mpsc::channel::<AppEvent>();
        let req = MoveRequest {
            session_id: "row-1".to_owned(),
            file: PathBuf::from("/nowhere/row-1.jsonl"),
            target: PathBuf::from("/r/wt"),
        };
        let spawned_at = Instant::now();
        spawn_move_with(req, tx, UndeliveredEvents::default(), |_| {
            thread::sleep(RUN_BLOCKS);
            MoveOutcome::Moved {
                cwd: Some("/r/wt".to_owned()),
            }
        });
        assert!(spawned_at.elapsed() < RUN_BLOCKS, "spawning must not wait");
        match rx.recv_timeout(DELIVERED_WITHIN) {
            Ok(AppEvent::MoveFinished {
                session_id,
                status,
                success,
            }) => {
                assert_eq!(session_id, "row-1");
                assert_eq!(status, "moved to /r/wt");
                assert!(success);
            }
            other => panic!("the worker must deliver MoveFinished, got {other:?}"),
        }
        match rx.recv_timeout(DELIVERED_WITHIN) {
            Err(RecvTimeoutError::Disconnected) => {}
            other => panic!("a one-shot sends once and ends, got {other:?}"),
        }
    }

    /// A move whose board is gone (a hand-off dropped the receiver) is QUEUED
    /// for the next board, never lost: it is what clears `App::moving`.
    #[test]
    fn a_move_finished_whose_board_is_gone_is_queued() {
        let (tx, rx) = mpsc::channel::<AppEvent>();
        drop(rx);
        let queue = UndeliveredEvents::default();
        let (done_tx, done_rx) = mpsc::channel();
        let req = MoveRequest {
            session_id: "row-gone".to_owned(),
            file: PathBuf::from("/nowhere/row-gone.jsonl"),
            target: PathBuf::from("/r/wt"),
        };
        spawn_move_with(req, tx, queue.clone(), move |_| {
            let _ = done_tx.send(());
            MoveOutcome::NoAnswer
        });
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the run ran");
        let deadline = Instant::now() + Duration::from_secs(2);
        let kept = loop {
            let kept = queue.take();
            if !kept.is_empty() || Instant::now() > deadline {
                break kept;
            }
            thread::sleep(Duration::from_millis(10));
        };
        assert!(
            matches!(
                kept.as_slice(),
                [AppEvent::MoveFinished { session_id, success: false, .. }] if session_id == "row-gone"
            ),
            "{kept:?}"
        );
    }

    /// Stand-in children (`sh`): no test spawns `claude`.
    #[cfg(unix)]
    mod children {
        use super::*;

        /// Generous for stand-ins that answer at once.
        const ANSWERS_WITHIN: Duration = Duration::from_secs(10);

        /// A stand-in claude: it reads ONE line and, only if that line is a
        /// `set_cwd` request, drains stdin to EOF, prints a noise line, then a
        /// successful `ok`/`changed:true` answer whose `cwd` reports what the
        /// child saw: `pwd=<its folder>;args=<its args>;git=<its
        /// CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS or unset>;line=<the request>`.
        /// Answering only after EOF proves the request arrived AND stdin closed.
        fn reporting() -> Vec<String> {
            // Quotes and backslashes are dropped from what it saw, so the report
            // stays one JSON string (the `--settings` value reads
            // `{disableAllHooks:true}`).
            let script = format!(
                r#"IFS= read -r line; cat >/dev/null
case "$line" in *'"subtype":"set_cwd"'*)
  seen=$(printf 'pwd=%s;args=%s;git=%s;line=%s' "$(pwd -P)" "$*" "${{CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS-unset}}" "$line" | tr -d '"\\')
  printf '%s\n' '{{"type":"system","subtype":"init"}}'
  printf '{{"type":"control_response","response":{{"subtype":"success","request_id":"{MOVE_REQUEST_ID}","response":{{"status":"ok","changed":true,"cwd":"%s"}}}}}}\n' "$seen";;
esac"#
            );
            vec!["sh".to_owned(), "-c".to_owned(), script, "sh".to_owned()]
        }

        /// What this process's own environment holds for the git prefetch
        /// switch, as [`reporting`] spells it: a trusted child inherits it as is.
        fn inherited_git_switch() -> String {
            std::env::var("CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS")
                .unwrap_or_else(|_| "unset".to_owned())
        }

        /// A global config whose `projects` trusts exactly `folder`.
        fn record_trusting(folder: &Path) -> String {
            let mut projects = serde_json::Map::new();
            projects.insert(
                folder.to_str().expect("a UTF-8 temp path").to_owned(),
                json!({ "hasTrustDialogAccepted": true }),
            );
            json!({ "projects": projects }).to_string()
        }

        /// What [`reporting`] saw, from a [`MoveOutcome::Moved`].
        fn seen(outcome: &MoveOutcome) -> String {
            match outcome {
                MoveOutcome::Moved { cwd: Some(seen) } => seen.clone(),
                other => panic!("the stand-in answers a move, got {other:?}"),
            }
        }

        /// The happy path: the child runs IN the session's current folder (the
        /// source, never the target), gets `-r <authoritative id>`, reads one
        /// `set_cwd` request for the target — a path with a space survives — and
        /// its `ok`/`changed:true` answer is the outcome.
        #[test]
        fn a_trusted_move_runs_in_the_current_folder_and_asks_for_the_target() {
            let root = temp_dir("happy");
            let current = root.join("main");
            let target = root.join("wt with space");
            std::fs::create_dir_all(&current).expect("create current");
            std::fs::create_dir_all(&target).expect("create target");
            let req = request_in(&root, &current, &target);

            let outcome = run_move(
                &req,
                |_| Some(false),
                |_| FolderTrust::Trusted,
                &reporting(),
                ANSWERS_WITHIN,
            );
            let report = seen(&outcome);
            assert!(
                report.starts_with(&format!("pwd={};", current.display())),
                "spawned from the current folder, never the target: {report}"
            );
            assert!(
                report.contains(
                    "args=-p --input-format stream-json --output-format stream-json --verbose \
                     --strict-mcp-config --settings {disableAllHooks:true} -r sess-mv;"
                ),
                "{report}"
            );
            assert!(
                report.contains(&format!(";git={};", inherited_git_switch())),
                "a trusted move adds nothing to the child's environment: {report}"
            );
            assert!(
                report.contains(&format!("path:{}", target.display())),
                "the request names the target, spaces and all: {report}"
            );
            std::fs::remove_dir_all(&root).ok();
        }

        /// The UNTRUSTED path the probe never exercised: a current folder the
        /// record does not trust is moved with `--setting-sources user` on the
        /// argv AND the git prefetch switch in the child's environment, both read
        /// from the real trust mirror over a stated record (never the user's).
        #[test]
        fn an_untrusted_current_folder_is_moved_in_the_untrusted_form() {
            let root = temp_dir("untrusted");
            let current = root.join("main");
            let target = root.join("wt");
            std::fs::create_dir_all(&current).expect("create current");
            std::fs::create_dir_all(&target).expect("create target");
            let cfg = root.join("claude.json");
            std::fs::write(&cfg, r#"{"projects":{}}"#).expect("write the record");
            let req = request_in(&root, &current, &target);

            let outcome = run_move(
                &req,
                |_| Some(false),
                |folder| claude_trust::folder_trust_in(&cfg, folder),
                &reporting(),
                ANSWERS_WITHIN,
            );
            let report = seen(&outcome);
            assert!(
                report.contains("-r sess-mv --setting-sources user;"),
                "{report}"
            );
            assert!(report.contains(";git=1;"), "{report}");

            // Control: the record trusting the folder drops both halves.
            std::fs::write(&cfg, record_trusting(&current)).expect("write the record");
            let outcome = run_move(
                &req,
                |_| Some(false),
                |folder| claude_trust::folder_trust_in(&cfg, folder),
                &reporting(),
                ANSWERS_WITHIN,
            );
            let trusted = seen(&outcome);
            assert!(!trusted.contains("--setting-sources"), "{trusted}");
            assert!(
                trusted.contains(&format!(";git={};", inherited_git_switch())),
                "{trusted}"
            );
            std::fs::remove_dir_all(&root).ok();
        }

        /// A stand-in child that only creates `marker`, so a test can tell
        /// whether any child ran.
        fn touching(marker: &Path) -> Vec<String> {
            vec![
                "sh".to_owned(),
                "-c".to_owned(),
                r#"touch "$1""#.to_owned(),
                "sh".to_owned(),
                marker.to_str().expect("utf-8").to_owned(),
            ]
        }

        /// The probe is asked for the AUTHORITATIVE id, after the pre-checks, and
        /// a live session never reaches a child.
        #[test]
        fn a_live_session_is_refused_before_any_child() {
            let root = temp_dir("live");
            let current = root.join("main");
            let target = root.join("wt");
            std::fs::create_dir_all(&current).expect("create current");
            std::fs::create_dir_all(&target).expect("create target");
            let req = request_in(&root, &current, &target);
            let marker = root.join("spawned");

            let mut asked = None;
            let outcome = run_move(
                &req,
                |id| {
                    asked = Some(id.to_owned());
                    Some(true)
                },
                |_| FolderTrust::Trusted,
                &touching(&marker),
                ANSWERS_WITHIN,
            );
            assert_eq!(outcome, MoveOutcome::Refused(MOVE_LIVE_REFUSAL.to_owned()));
            assert_eq!(asked.as_deref(), Some("sess-mv"));
            assert!(!marker.exists(), "a refused move spawns nothing");
            std::fs::remove_dir_all(&root).ok();
        }

        /// Decision 6a: a probe that could not answer REFUSES the move in its own
        /// words, before the trust read and any child — never the "not live" guess
        /// every other gate takes.
        #[test]
        fn a_probe_that_cannot_answer_refuses_before_any_child() {
            let root = temp_dir("probe-failed");
            let current = root.join("main");
            let target = root.join("wt");
            std::fs::create_dir_all(&current).expect("create current");
            std::fs::create_dir_all(&target).expect("create target");
            let req = request_in(&root, &current, &target);
            let marker = root.join("spawned");

            let outcome = run_move(
                &req,
                |_| None,
                |_| -> FolderTrust { panic!("the trust read must not run") },
                &touching(&marker),
                ANSWERS_WITHIN,
            );
            assert_eq!(
                outcome,
                MoveOutcome::Refused(MOVE_PROBE_FAILED_REFUSAL.to_owned())
            );
            assert!(!marker.exists(), "a refused move spawns nothing");
            std::fs::remove_dir_all(&root).ok();
        }

        /// The pre-checks refuse before the probe and the child: a current folder
        /// that is gone ([`MOVE_SOURCE_GONE`]), and each bad target.
        #[test]
        fn pre_checks_refuse_before_the_probe_and_the_child() {
            let root = temp_dir("pre");
            let current = root.join("main");
            let target = root.join("wt");
            std::fs::create_dir_all(&target).expect("create target");
            let never_live = |_: &str| -> Option<bool> { panic!("the probe must not run") };

            // The current folder does not exist.
            let req = request_in(&root, &current, &target);
            assert_eq!(
                run_move(
                    &req,
                    never_live,
                    |_| FolderTrust::Trusted,
                    &reporting(),
                    ANSWERS_WITHIN,
                ),
                MoveOutcome::Refused(MOVE_SOURCE_GONE.to_owned()),
                "a gone current folder refuses in the move's own words"
            );

            std::fs::create_dir_all(&current).expect("create current");
            for (to, refusal) in [
                (root.join("gone"), MOVE_TARGET_GONE),
                (current.clone(), MOVE_ALREADY_THERE),
                (PathBuf::from("relative"), MOVE_TARGET_UNUSABLE),
            ] {
                let req = request_in(&root, &current, &to);
                assert_eq!(
                    run_move(
                        &req,
                        never_live,
                        |_| FolderTrust::Trusted,
                        &reporting(),
                        ANSWERS_WITHIN
                    ),
                    MoveOutcome::Refused(refusal.to_owned())
                );
            }
            std::fs::remove_dir_all(&root).ok();
        }

        /// No answer is told apart from a timeout and from no child at all.
        #[test]
        fn silence_a_hang_and_a_missing_program_are_three_outcomes() {
            let root = temp_dir("silent");
            let current = root.join("main");
            let target = root.join("wt");
            std::fs::create_dir_all(&current).expect("create current");
            std::fs::create_dir_all(&target).expect("create target");
            let req = request_in(&root, &current, &target);
            let run = |program: &[&str], timeout| {
                let program: Vec<String> = program.iter().map(|w| (*w).to_owned()).collect();
                run_move(
                    &req,
                    |_| Some(false),
                    |_| FolderTrust::Trusted,
                    &program,
                    timeout,
                )
            };

            assert_eq!(run(&["true"], ANSWERS_WITHIN), MoveOutcome::NoAnswer);
            assert_eq!(
                run(&["sh", "-c", "sleep 5"], Duration::from_millis(300)),
                MoveOutcome::TimedOut
            );
            assert_eq!(
                run(&["snapback-test-no-such-claude"], ANSWERS_WITHIN),
                MoveOutcome::SpawnFailed
            );
            std::fs::remove_dir_all(&root).ok();
        }

        /// The liveness probe AND the trust read are the WORKER's (AGENTS.md
        /// OFF-UI-THREAD): the spawn returns before a slow probe does, and both
        /// run on another thread, in that order, for the authoritative id and the
        /// current folder.
        #[test]
        fn the_probe_and_the_trust_read_run_on_the_move_worker() {
            /// Longer than a spawn takes, so work on the SPAWNING thread cannot
            /// beat it by luck.
            const PROBE_BLOCKS: Duration = Duration::from_millis(300);

            let root = temp_dir("worker");
            let current = root.join("main");
            let target = root.join("wt");
            std::fs::create_dir_all(&current).expect("create current");
            std::fs::create_dir_all(&target).expect("create target");
            let req = request_in(&root, &current, &target);
            let (tx, rx) = mpsc::channel::<AppEvent>();
            let (seen_tx, seen_rx) = mpsc::channel();
            let trust_seen = seen_tx.clone();
            let spawner = thread::current().id();

            let spawned_at = Instant::now();
            spawn_move_in(
                req,
                tx,
                UndeliveredEvents::default(),
                move |id: &str| {
                    let _ = seen_tx.send((thread::current().id(), format!("probe {id}")));
                    thread::sleep(PROBE_BLOCKS);
                    Some(false)
                },
                move |folder: &Path| {
                    let _ = trust_seen.send((
                        thread::current().id(),
                        format!("trust {}", folder.display()),
                    ));
                    FolderTrust::Trusted
                },
                reporting(),
                ANSWERS_WITHIN,
            );
            assert!(
                spawned_at.elapsed() < PROBE_BLOCKS,
                "spawning must not wait on the probe"
            );
            let probe = seen_rx.recv_timeout(ANSWERS_WITHIN).expect("the probe ran");
            let trust = seen_rx
                .recv_timeout(ANSWERS_WITHIN)
                .expect("the trust read ran");
            assert_ne!(probe.0, spawner, "the probe runs on the worker");
            assert_ne!(trust.0, spawner, "the trust read runs on the worker");
            assert_eq!(probe.1, "probe sess-mv");
            assert_eq!(trust.1, format!("trust {}", current.display()));
            match rx.recv_timeout(ANSWERS_WITHIN) {
                Ok(AppEvent::MoveFinished {
                    session_id,
                    success,
                    ..
                }) => {
                    assert_eq!(session_id, "sess-mv");
                    assert!(success);
                }
                other => panic!("the worker must deliver MoveFinished, got {other:?}"),
            }
            std::fs::remove_dir_all(&root).ok();
        }
    }
}
