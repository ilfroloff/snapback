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
//! * [`reply_in_flight_refusal`] — what `Ctrl-R` asks BEFORE that gate and its
//!   probe. A board sends one quick reply at a time, so while one is in flight
//!   `Ctrl-R` refuses on every row ([`SEND_IN_FLIGHT_REFUSED`]), naming that
//!   reply's session.
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
//! A board session can end before that event is read, so [`UndeliveredEvents`]
//! keeps it for the next board on the same `App` rather than losing it.
//!
//! A THIRD family rides the same shape: the background-agent launch
//! ([`build_bg_launch_argv`] / [`plan_bg_launch`] / [`status_for_bg_launch`] /
//! [`spawn_bg_launch`]). It writes nothing to an existing session — it asks
//! `claude` to START one as a background agent (`claude [--agent <name>] --bg
//! <prompt>`) — but it belongs here rather than in `resume.rs` for the reason
//! that defines this module: there is NO terminal teardown, so the board stays up
//! and the result comes back as an event. Its honesty seam
//! ([`status_for_bg_launch`]) and the send's ([`status_for_output`]) share ONE
//! three-row shape, because BOTH can fail SILENTLY on a zero exit: claude prints
//! its reservation to stderr and exits **0** either way — an unrecognized
//! `--agent` on the launch, a background-task sweep that terminates the agent the
//! reply was aimed at on the send. So each treats a zero exit with a non-empty
//! stderr as *started/sent, but claude warned…* rather than as a clean success.
//!
//! `Ctrl-K`'s SIGNAL route lives here too, and it is the one piece that spawns no
//! `claude` child at all. A reported session with no stoppable job id but a `pid` a
//! signal could take ([`signallable_pid`]; [`interrupt_gate`] →
//! [`InterruptGate::ConfirmSignal`]) is confirmed, the pid is
//! re-verified against a fresh probe ([`signal_plan`]), and [`signal_term`] sends it
//! a SIGTERM through `kill(2)`, the crate's one syscall, with no thread and no event
//! because the call does not block. [`status_for_signal`] maps the result. Every
//! decision on that route is pure; [`signal_term`] is its only effect, and no test
//! calls it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{SendError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

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
/// live agent for no clear gain — so those refuse, and point at the next moves.
///
/// **It names only moves that hold for EVERY record that reaches it.** Two kinds
/// do: a job-id record in a live bucket, and a record with NO job id at all, which
/// [`reply_gate`] refuses before it looks at a bucket. Every `kind:"interactive"`
/// record measured so far is of that second kind, whichever process was behind it:
/// at `claude 2.1.278` every one (11/11) was a `claude -p` child, and at
/// `claude 2.1.280` both (2/2) were pty-backed TUIs, one busy and one idle (see
/// `docs/agents/DOMAIN.md`, "What `kind: "interactive"` denotes"). None carried a
/// job id. It used to say "Attach to answer it", but Attach
/// refuses that second kind ([`crate::resume::ATTACH_NO_JOB_ID`]), so the hint led
/// to a second refusal. It now names:
///
/// * Fork (`Ctrl-F`), which works on either kind;
/// * `Ctrl-K`, which has a route for both: `claude stop` for a job id, a confirmed
///   SIGTERM for a reported `pid` (see [`interrupt_gate`]). It is offered with
///   "Try", never promised, because a record with neither handle is still refused
///   ([`INTERRUPT_NO_JOB_ID`]), and so is one whose pid no signal could take
///   ([`INTERRUPT_PID_UNUSABLE`]).
///
/// Attach stays one keypress away on a job-id record (Enter's running-session
/// choice). It is just no longer named here. Mirrors
/// [`crate::resume::ATTACH_NOT_LIVE`]'s "refuse with a next move that holds in
/// every world" shape.
pub const SEND_LIVE_REFUSED: &str =
    "claude reports this session as a running agent, so it won't resume it in place. \
     Try Ctrl-K to stop it, or Fork (Ctrl-F) to branch a copy.";

/// Refusal shown when `Ctrl-R` is pressed on ANY row while snapback's own quick reply
/// is still in flight. It is a PREFIX: [`reply_in_flight_refusal`] appends the name
/// of the session that reply is going to.
///
/// A board sends ONE quick reply at a time, because `App::sending` tracks exactly
/// one. That slot is more than the preview's echo. It is the board's only record of
/// the `claude -p` child snapback spawned, the THIRD writer
/// [`crate::delete::can_delete_target`] refuses a hard delete on, and claude's
/// active list is not a witness snapback can rely on for that writer. A second
/// reply that replaced the slot would leave `Ctrl-X d` on the first session judged
/// by claude's probe alone. Sending several at once would need a per-session
/// registry of sends instead of one slot.
///
/// The session is named LAST so a one-row status line cut at the terminal's width
/// keeps the rule and loses only the tail of a long label.
pub const SEND_IN_FLIGHT_REFUSED: &str = "One reply at a time — still sending to: ";

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

/// Prefix for a send that LANDED (exit 0) but printed something on stderr.
///
/// Worded "sent, but" rather than as a failure because both halves are true: the
/// reply DID go through, and claude had a reservation about it. It exists because
/// a zero exit is not proof the send did what was asked — claude can print the
/// reservation to stderr and still exit **0**, the known case being the
/// background-task sweep (`Background tasks still running after <n>s;
/// terminating.`), which kills the background agent the reply was aimed at and
/// then exits clean. [`SEND_OK`] — or worse, a priced `sent — $…` — would
/// misreport that silent downgrade as a flawless reply. The launch path's
/// [`BG_LAUNCH_WARNED_PREFIX`] is the same prefix for the same reason.
const SEND_WARNED_PREFIX: &str = "sent, but claude warned: ";

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

/// The record's job `id` IF `claude stop` has something to take — `None` for both
/// an absent id and a BLANK one.
///
/// One spelling of that rule for every gate that asks it ([`reply_gate`],
/// [`interrupt_gate`], [`signal_plan`]), because the three must agree: a blank id
/// that read as stoppable would spawn `claude stop "   "`, and — since
/// `interrupt_gate` reads a record's `pid` exactly where this answers `None` — a
/// drift between two of them would move the boundary between the DELEGATED stop and
/// an irreversible signal. Pure; trimmed for the test rather than stored trimmed, so
/// the id handed to `claude` is byte-for-byte the one claude reported.
fn stoppable_job_id(agent: &ReportedAgent) -> Option<&str> {
    agent.id.as_deref().filter(|id| !id.trim().is_empty())
}

/// `Ctrl-R`'s FIRST question, asked before [`reply_gate`] and before the probe that
/// feeds it: is a quick reply already in flight? `in_flight` is the name of the
/// session the board's in-flight reply (`App::sending`) is going to, or `None` when
/// there is none.
///
/// `Some` refuses, naming that session ([`SEND_IN_FLIGHT_REFUSED`]), whatever row
/// `Ctrl-R` was pressed on. The selected row is deliberately not an input: a second
/// reply to the SAME session is refused too, because the first one's
/// `SendFinished` would clear the slot while the second was still writing. The
/// caller asks this before any compose box opens, so no typed message is thrown
/// away. `None` lets [`reply_gate`] decide.
#[must_use]
pub fn reply_in_flight_refusal(in_flight: Option<&str>) -> Option<String> {
    in_flight.map(|name| format!("{SEND_IN_FLIGHT_REFUSED}{name}"))
}

/// Decide [`ReplyGate`] from the session's current live-agent record (`None` when
/// claude is not holding it).
///
/// Pure so the whole decision tree is unit-tested without a probe. It reuses the
/// one classifier ([`agents::classify`]) so "what state is this" is answered in one
/// place. `done` — or a TERMINAL `stopped` / `failed` — → stop-then-reply (safe:
/// the job has ended, so stopping it interrupts nothing); `needs input` → confirm
/// first (stopping abandons a waiting agent); `working`/`idle`/`interrupted`/unknown
/// → refuse (stopping would interrupt live work, and the user should Attach/Fork). A
/// held agent with no stoppable job id (`id == None`, e.g. an interactive session)
/// gives this path's `claude stop` step nothing to take, so it refuses too (a
/// `pid` does not change that: signalling is `Ctrl-K`'s own confirmed verb, never a
/// reply's preparatory step). Not held at all → reply in place directly.
///
/// [`AgentActivity::WorkingButIdle`] rides with the LIVE states, NOT with
/// [`AgentActivity::Ended`], even though both badge steady. The difference is
/// EVIDENCE: `Ended` is claude reporting a terminal token, so "the job is over" is
/// claude's own answer, while `WorkingButIdle` is snapback INFERRING it from a
/// `state`/`status` contradiction whose documented false positive is a healthy
/// agent caught mid-flip (it self-heals on the next poll). Acting on that inference
/// would stop live work on a guess, so the bucket stays display-only exactly as its
/// own docs promise.
#[must_use]
pub fn reply_gate(record: Option<&ReportedAgent>) -> ReplyGate {
    let Some(agent) = record else {
        return ReplyGate::Reply; // claude isn't holding it -> plain in-place reply
    };
    let Some(job_id) = stoppable_job_id(agent) else {
        // Held but not stoppable by job id (e.g. an interactive session).
        return ReplyGate::Refuse(SEND_LIVE_REFUSED);
    };
    let job_id = job_id.to_string();
    match agents::classify(agent) {
        AgentActivity::Done | AgentActivity::Ended => ReplyGate::StopThenReply { job_id },
        AgentActivity::NeedsInput => ReplyGate::ConfirmStopThenReply { job_id },
        // `WorkingButIdle` refuses WITH the live states: claude's agent list still
        // reports it as working, and the contradiction is a display inference, not
        // proof the turn ended (see this function's docs).
        AgentActivity::Working
        | AgentActivity::WorkingButIdle
        | AgentActivity::Idle
        | AgentActivity::Other => ReplyGate::Refuse(SEND_LIVE_REFUSED),
    }
}

/// Refusal shown when `Ctrl-K` targets a session claude is NOT holding as an agent:
/// there is no live job to stop. A resumable transcript on disk is not a running
/// process, so stopping is meaningless — say so rather than shell out to fail.
pub const INTERRUPT_NOT_LIVE: &str =
    "This session isn't running as an agent — there's nothing to stop.";

/// Refusal shown when `Ctrl-K` targets a session claude reports with NEITHER handle:
/// no stoppable job `id` for `claude stop`, and no `pid` for the signal route
/// ([`InterruptGate::ConfirmSignal`]). Since that route landed, this is the only
/// case [`interrupt_gate`] refuses with it. A record that DOES carry a pid, but one
/// no signal could ever take, refuses with [`INTERRUPT_PID_UNUSABLE`] instead:
/// "no process id" would be false for it.
///
/// **Worded for what was OBSERVED, and nothing more** — the rule behind
/// [`crate::resume::ATTACH_NOT_LIVE`]. It used to send the user to "the terminal
/// that's running it", which named an owner the evidence does not support. The
/// `kind:"interactive"` records measured so far took two shapes: at `claude 2.1.278`
/// every one (11/11) was a `claude -p` child with no terminal of its own, and at
/// `claude 2.1.280` both (2/2) were pty-backed TUIs, one busy and one idle (see
/// `docs/agents/DOMAIN.md`, "What `kind: "interactive"` denotes"). So a terminal may
/// or may not exist, and nothing snapback observes says whose it is. Both readings
/// agree on the record, so the copy describes the record: two absent handles, hence
/// nothing here to act with. It does not claim the session stopped, or will.
pub const INTERRUPT_NO_JOB_ID: &str =
    "claude reports no attachable job and no process id for this session, \
     so there is no handle here to stop or signal it.";

/// Refusal shown when `Ctrl-K` targets a session claude reports with no stoppable job
/// `id` and a `pid` this board can never signal: `0`, a number past `i32::MAX` that no
/// `pid_t` can hold, or the board's OWN process id (the rule is [`signallable_pid`]).
/// The gate refuses it at the keypress rather than opening a confirm whose only
/// possible end is a failure — or, for the board's own pid, the board's exit without
/// its terminal restore.
///
/// Worded for what was OBSERVED, exactly like [`INTERRUPT_NO_JOB_ID`]: a process id IS
/// on the record, so this must never say there is none. It names no owner and picks no
/// reading of `kind`, and it does not claim the session stopped, or will. "Cannot be
/// signalled" is said from the board's side and holds for all three causes, which the
/// copy does not tell apart: no `kill(2)` can take the first two, and the board must
/// not send the third, because that signal would end the board itself. Each leaves
/// nothing here to stop the session with.
pub const INTERRUPT_PID_UNUSABLE: &str =
    "claude reports no attachable job for this session and a process id that \
     cannot be signalled, so there is nothing here to stop it.";

/// What `Ctrl-K` should do for the selected session, decided from what claude is
/// holding it as right now.
///
/// The interrupt counterpart of [`ReplyGate`], with the OPPOSITE intent: a reply
/// must never interrupt live work, whereas an interrupt exists to stop it — so a
/// `working` (mid-turn) agent is a valid target here, not a refusal. Only a
/// background job carries the short id `claude stop` takes; a reported session
/// WITHOUT one is reachable solely through the `pid` on its record, and a session
/// claude isn't holding at all has nothing to stop. Stopping abandons live work, so
/// on the job-id route every state EXCEPT a finished (`done`) or terminal
/// (`stopped` / `failed`) one confirms first — and the pid route confirms in every
/// state, for the reason [`InterruptGate::ConfirmSignal`] gives.
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
    /// A reported session with NO attachable job id but a `pid` on the wire that a
    /// signal could take ([`signallable_pid`]) — the only handle left. CONFIRM first,
    /// then send that pid a SIGTERM.
    ///
    /// The confirm is UNCONDITIONAL: there is deliberately no `StopNow` counterpart
    /// here, however finished the record looks. `StopNow` is safe because stopping a
    /// job that already ended is claude's own no-op (`claude stop <dead-job>` just
    /// exits non-zero). A signal has no such floor — a `state`/`status` bucket is a
    /// report ABOUT a session, and it cannot prove the process now wearing this pid
    /// is still the one claude reported.
    ConfirmSignal {
        /// The OS process id to signal, straight off the record
        /// ([`ReportedAgent::pid`]) — and nothing else, because the pid IS the whole
        /// decision here: no argv, no job, no child to spawn.
        pid: u32,
    },
    /// Nothing stoppable — refuse with a message (not a live agent; reported with
    /// neither a job id nor a pid; or with no job id and a pid no signal could take).
    Refuse(&'static str),
}

/// Decide [`InterruptGate`] from the session's current live-agent record (`None`
/// when claude is not holding it).
///
/// Pure so the whole decision tree is unit-tested without a probe, reusing the one
/// classifier ([`agents::classify`]). Spawns nothing and signals nothing — it only
/// says which route the keypress takes. Mirrors [`reply_gate`]'s shape but routes by
/// the interrupt intent: not held → refuse (nothing to stop); `done` — or a TERMINAL
/// `stopped` / `failed` — → stop immediately (harmless; the job is already over);
/// every other live state → confirm first (stopping abandons live work); no
/// stoppable job id but a reported `pid` that a signal could take → confirm, then
/// signal that pid; no job id and a `pid` no signal could ever take → refuse
/// ([`INTERRUPT_PID_UNUSABLE`]); neither → refuse (nothing on the record to act on).
///
/// # The boundary between the two stop mechanisms
///
/// Two now exist, and keeping them apart is this function's main job. `claude stop
/// <job-id>` is the DELEGATED verb: claude ends its own job, by a handle it issued.
/// A SIGTERM to a pid is the blunt one, aimed at a process this code cannot prove it
/// owns. So `claude stop` wins WHENEVER a job id exists — the pid arm is reachable
/// only through the `else` of the job-id read, which is why a record carrying BOTH
/// routes by the id and its pid is never even looked at. Do not widen the pid arm to
/// a bucket that has a job id.
///
/// "No job id" and "has a pid" stay two SEPARATE conditions, and neither may be
/// inferred from the other or from `kind` — [`ReportedAgent::pid`] carries the
/// measurement that forces this (pid on 3/3 interactive and 0/159 background records
/// at `claude 2.1.278`, but on 2/150 background ones in an earlier sample).
///
/// The pid route confirms UNCONDITIONALLY: there is no `StopNow` equivalent for it,
/// because a `state`/`status` bucket describes a SESSION claude reported and cannot
/// prove that a foreign process is safe to signal. That asymmetry against the job-id
/// route's `StopNow` is deliberate, not an oversight — see
/// [`InterruptGate::ConfirmSignal`].
///
/// The confirm opens only for a pid a signal could take. `0`, any number past
/// `i32::MAX`, and `own_pid` — this board's own process id, passed in rather than read
/// here so the gate stays pure — refuse here instead, by [`signallable_pid`]. Its range
/// half is the one [`signal_target`] applies again right before `kill(2)`, so no
/// confirm is ever offered for a pid that step would refuse. Its board half is asked
/// here alone, because a SIGTERM to the board's own process would end the board without
/// its terminal restore.
///
/// [`AgentActivity::WorkingButIdle`] CONFIRMS rather than stopping immediately, for
/// the same evidence gap [`reply_gate`] documents: `Ended` is claude's own terminal
/// token, whereas the interrupted bucket is inferred from a contradiction that can
/// legitimately catch a healthy agent mid-flip. Skipping the confirm here would kill
/// live work on that false positive with no way back; the confirm costs one keypress
/// and is exactly the safety net for a bucket that cannot prove its own cause.
#[must_use]
pub fn interrupt_gate(record: Option<&ReportedAgent>, own_pid: u32) -> InterruptGate {
    let Some(agent) = record else {
        return InterruptGate::Refuse(INTERRUPT_NOT_LIVE); // nothing running to stop
    };
    let Some(job_id) = stoppable_job_id(agent) else {
        // No attachable job, so `claude stop` has no id to take. The pid claude
        // reported for this session is the only handle left — and it is read HERE
        // and nowhere else, inside the `else` of the job-id read, so the delegated
        // verb keeps every record that has an id (see this function's docs).
        return match agent.pid {
            Some(pid) if signallable_pid(pid, own_pid).is_some() => {
                InterruptGate::ConfirmSignal { pid }
            }
            // A pid IS on the record, but no signal from this board may take it (out
            // of range, or the board's own): refuse now rather than open a confirm
            // whose only possible end is a failure or the board's own exit.
            Some(_) => InterruptGate::Refuse(INTERRUPT_PID_UNUSABLE),
            None => InterruptGate::Refuse(INTERRUPT_NO_JOB_ID),
        };
    };
    let job_id = job_id.to_string();
    match agents::classify(agent) {
        AgentActivity::Done | AgentActivity::Ended => InterruptGate::StopNow { job_id },
        // `WorkingButIdle` confirms WITH the live states — the inference is not
        // strong enough to skip the guard (see this function's docs).
        AgentActivity::Working
        | AgentActivity::WorkingButIdle
        | AgentActivity::NeedsInput
        | AgentActivity::Idle
        | AgentActivity::Other => InterruptGate::Confirm { job_id },
    }
}

/// Refusal when the confirm-time re-probe no longer reports the session AT ALL: it
/// ended while the confirm was open, so there is nothing left to signal — and the pid
/// the confirm still carries is exactly the stale number a reuse would turn into an
/// unrelated process.
pub const SIGNAL_RECORD_GONE: &str =
    "That session is no longer reported as running — nothing was signalled.";

/// Refusal when the re-probe DOES report the session but it now carries a stoppable
/// job id: the delegated verb became available while the confirm was open, and it
/// always wins (see [`interrupt_gate`]'s boundary section). The keypress re-routes
/// rather than signalling, so `Ctrl-K` again takes the `claude stop` path.
pub const SIGNAL_NOW_HAS_JOB: &str =
    "That session now reports a stoppable job — press Ctrl-K again to stop it.";

/// Refusal when the re-probed record carries a DIFFERENT pid — or none at all. The
/// number captured at confirm time is no longer what claude reports for this session,
/// so nothing on the current record vouches for it.
pub const SIGNAL_PID_MOVED: &str =
    "The process id reported for that session changed — nothing was signalled.";

/// What `Enter` on the interrupt confirm's SIGNAL route should do, judged against a
/// freshly re-probed record.
#[derive(Debug, PartialEq, Eq)]
pub enum SignalPlan {
    /// Re-verified against the fresh record: send THIS pid a SIGTERM.
    Signal {
        /// The pid to signal — the captured one, which the fresh record still names.
        pid: u32,
    },
    /// Do not signal; show this reason instead.
    Refuse(&'static str),
}

/// Decide whether the pid captured when the interrupt confirm OPENED is still the pid
/// to signal, given the record a FRESH probe returns at `Enter`.
///
/// Pure — it takes the re-probed record as an argument and spawns nothing, so all four
/// answers are unit-tested without a `claude` child. The probe itself happens at the
/// call site ([`crate::tui::update`]'s interrupt-confirm `Enter` arm), which is also
/// where the one-shot is argued.
///
/// # Why a second probe at all: pid reuse
///
/// Every other confirm in this codebase re-asks claude at the hand-off because the
/// overlay can sit open INDEFINITELY. Here that unbounded window is worse than stale
/// data: a pid is not a name, it is a slot the kernel recycles. A process that exits
/// while the confirm is open frees its pid, and a signal aimed at the number would
/// land on whatever took it. So the captured pid is treated as a CLAIM to re-verify,
/// never as a target — and the only answer that signals is the one where claude still
/// reports this very pid for this very session.
///
/// # The three refusals, in the order they are checked
///
/// 1. **The record is gone** ([`SIGNAL_RECORD_GONE`]) — the session ended on its own.
///    Refusing is not just caution: its pid is now the likeliest of all to have been
///    reused.
/// 2. **The record now carries a stoppable job id** ([`SIGNAL_NOW_HAS_JOB`]) — the
///    delegated `claude stop` wins wherever a job id exists ([`interrupt_gate`]), so a
///    record that gained one must re-route rather than be signalled. A matching pid is
///    permission to signal only in the absence of a better verb. It is checked BEFORE
///    the pid comparison, which changes no SAFETY property (with both stale, either
///    check refuses) but does decide which refusal the user reads: this one names a
///    next move, where [`SIGNAL_PID_MOVED`] is a dead end.
/// 3. **The pid moved** ([`SIGNAL_PID_MOVED`]) — the record reports a different pid,
///    or none. One comparison covers both, because the question is not "did it
///    change?" but "does the fresh record still name THIS pid?", and an absent pid
///    answers no just as loudly as a different one.
#[must_use]
pub fn signal_plan(captured_pid: u32, fresh: Option<&ReportedAgent>) -> SignalPlan {
    let Some(agent) = fresh else {
        return SignalPlan::Refuse(SIGNAL_RECORD_GONE);
    };
    if stoppable_job_id(agent).is_some() {
        return SignalPlan::Refuse(SIGNAL_NOW_HAS_JOB);
    }
    if agent.pid != Some(captured_pid) {
        return SignalPlan::Refuse(SIGNAL_PID_MOVED);
    }
    SignalPlan::Signal { pid: captured_pid }
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
///
/// Carries the target `session_id` so the completion event can be attributed to the
/// surface that dispatched it and ignored if the board has moved on.
#[derive(Debug, Clone)]
pub struct InterruptRequest {
    /// The full argv to spawn; `argv[0]` is the program (always `claude`).
    pub argv: Vec<String>,
    /// A valid directory to run the child in (the launch dir). Never the process cwd.
    pub cwd: PathBuf,
    /// Authoritative `sessionId` the completion event is keyed by, so the handler
    /// can tell whether the finished interrupt targets the row currently on screen.
    pub session_id: String,
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
/// deleted (a removed worktree) refuses. The parser's two non-session verdicts
/// COLLAPSE into that one refusal, whose wording already names only what was
/// observed rather than a cause it cannot distinguish. Pure so the refusal path
/// is unit tested against a real temp file exactly like `resume::plan_refuses_...`.
#[must_use]
pub fn plan_send(file: &Path) -> SendPlan {
    let Some(parsed) = parse::parse_file(file).session() else {
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

/// Map the `--output-format json` stdout to a board status and its class.
///
/// FAIL-SOFT by construction (AGENTS.md): the payload is parsed as
/// `serde_json::Value`, never a hard-typed struct, and no field access can panic
/// on an absent/mistyped key. On `is_error == true` it returns an error status
/// classified as **sticky**; on success it surfaces `total_cost_usd`
/// (e.g. `"sent — $0.0136"`) when present, else a neutral `"sent"`, both
/// classified as **transient**; unparseable/empty stdout also degrades to the
/// neutral `"sent"` (the child ran, but printed nothing we can read — never a
/// panic, never a false cost). Returns `(text, transient)` so the UI layer never
/// has to infer the class from the text.
#[must_use]
pub fn status_for_send(raw_stdout: &str) -> (String, bool) {
    let Ok(value) = serde_json::from_str::<Value>(raw_stdout) else {
        // Unparseable / empty -> neutral, no panic.
        return (SEND_OK.to_string(), true);
    };
    if value.get("is_error").and_then(Value::as_bool) == Some(true) {
        return (SEND_ERROR.to_string(), false);
    }
    match value.get("total_cost_usd").and_then(Value::as_f64) {
        Some(cost) => (format!("sent — ${cost:.4}"), true),
        None => (SEND_OK.to_string(), true),
    }
}

/// Combine a finished send's exit status + captured streams into a board status
/// and its class.
///
/// This is the send's honesty seam: a send that failed — or that quietly did LESS
/// than was asked — must never read as the neutral success. It has the same three
/// rows as the launch's ([`status_for_bg_launch`]), for the same reason:
///
/// | Exit | stderr (sanitized) | Status | Class |
/// | --- | --- | --- | --- |
/// | non-zero | anything | [`SEND_FAILED_PREFIX`] + claude's own reason, via [`status_for_failed_send`] | sticky |
/// | zero | NON-EMPTY | [`SEND_WARNED_PREFIX`] + that reason | sticky |
/// | zero | empty | [`status_for_send`]'s mapping of the payload (cost / `is_error` / neutral), unchanged | as that map classifies it |
///
/// The bottom row is the ordinary case and the top row is the loud one — claude
/// refusing to resume a session it holds as an agent exits `1` with the reason on
/// `stderr` and nothing on `stdout`.
///
/// The MIDDLE row is the one worth arguing. A zero exit is not proof the send did
/// what was asked: claude can print a reservation to stderr and still exit **0**,
/// the known case being the background-task sweep (`Background tasks still
/// running after <n>s; terminating.`), which kills the background agent the reply
/// was aimed at and then exits clean — reported, without this row, as a flawless
/// `sent — $0.0136`. The arm deliberately does NOT match that wording: ANY
/// surviving stderr warns, so the next zero-exit downgrade that is not this one is
/// surfaced too. A signature match would be hostage to a string snapback does not
/// own, which is exactly the fragility the FAIL-SOFT rule warns about; the
/// accepted cost is a noisier status line, and there is no quieting heuristic.
///
/// PRECEDENCE on a zero exit, stated rather than left to be inferred:
/// [`status_for_send`]'s own FAILURE verdict WINS OUTRIGHT. The middle row may
/// only replace a status that would have read as a SUCCESS — the priced row or the
/// neutral [`SEND_OK`] — because it exists to replace a FLATTERING status, never
/// to overwrite an already-honest one. An `is_error: true` payload therefore comes
/// back byte-identical to what it returns with a silent stderr.
///
/// It DEGRADES toward today's behaviour, never toward a fabricated failure:
/// "non-empty stderr" means what survives [`first_quotable_line`], so a blank,
/// whitespace-only or all-escape stderr falls through to the bottom row untouched,
/// and the quoted text can never carry a raw escape to the ratatui buffer
/// (TERMINAL-SAFE STYLING). Returns `(text, transient)`; failures AND the warning
/// are sticky (AGENTS.md STATUS-LINE OWNERSHIP). Pure and unit-tested — no process
/// is ever spawned.
#[must_use]
pub fn status_for_output(success: bool, stdout: &str, stderr: &str) -> (String, bool) {
    if !success {
        return (status_for_failed_send(stdout, stderr), false);
    }
    let (status, transient) = status_for_send(stdout);
    // `status_for_send` classifies its two SUCCESS rows (the priced one, the
    // neutral `SEND_OK`) transient and its one FAILURE row (`is_error: true`)
    // sticky, so this flag IS the "did that read as a success?" question — named
    // here rather than used as an anonymous boolean, because the precedence above
    // turns on the question, not on the class.
    let read_as_success = transient;
    if !read_as_success {
        return (status, transient); // already honest -> never re-worded as a warning
    }
    // Zero exit: a stderr that survives sanitizing is claude's reservation about a
    // send it nonetheless completed (see the table above).
    match first_quotable_line(stderr) {
        Some(line) => (format!("{SEND_WARNED_PREFIX}{line}"), false),
        None => (status, transient),
    }
}

/// Build the status for a NON-ZERO send exit, surfacing claude's OWN reason.
///
/// Preference order, so the most specific truthful message wins: a JSON
/// `is_error` payload's `result` (when `--output-format json` still printed one),
/// else the first non-empty `stderr` line (the common case — e.g. `Error: Session
/// <id> is currently running as a background agent (bg)…`), else a generic
/// fallback. The quoted text goes through [`first_quotable_line`] (ANSI/control
/// stripped, one line, length-capped) so a raw escape from claude's stderr can
/// never reach the ratatui buffer (TERMINAL-SAFE STYLING). Pure and unit-tested.
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
    match first_quotable_line(stderr) {
        Some(line) => format!("{SEND_FAILED_PREFIX}{line}"),
        None => SEND_FAILED_GENERIC.to_string(),
    }
}

/// The first line of `text` that survives sanitizing, with claude's duplicated
/// `Error: ` label stripped — `None` when nothing readable is left.
///
/// THREE of the FOUR sites that apply that quoting rule route through it —
/// [`status_for_failed_send`], [`status_for_output`]'s zero-exit warning, and
/// [`status_for_bg_launch`] — so the send and the launch render claude's own words
/// identically: one line, ANSI and control characters stripped (TERMINAL-SAFE
/// STYLING), length-capped. The FOURTH, [`status_for_stop`], does NOT: it still
/// holds an inline copy of the same sanitize -> strip-label -> first-non-empty
/// chain over its own two-stream fallback. So this is the rule's SHARED home, not
/// its ONLY one — a change here must be mirrored there until that copy is folded in.
///
/// The ORDER is load-bearing: sanitize FIRST, then strip the label. claude may
/// COLOR the line, so the label can sit behind an ANSI escape that has to be
/// removed before the strip can see it. Doing it the other way round leaves
/// `send failed: Error: …` on a colored line.
///
/// "Empty" therefore means what survives [`sanitize_status`], not what is
/// non-blank on the wire: a whitespace-only or all-escape `text` yields `None`, so
/// a caller gating on this can never fabricate a status out of nothing.
fn first_quotable_line(text: &str) -> Option<String> {
    text.lines()
        .map(sanitize_status)
        .map(|line| strip_error_prefix(&line).to_string())
        .find(|line| !line.is_empty())
}

/// Strip a leading `Error: ` label claude prefixes onto a stderr message, so the
/// status is not doubled up (`send failed: Error: …`). Expects an already-sanitized
/// line (see [`first_quotable_line`], which owns that ordering).
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

/// Completions a board session ended before it could READ, kept for the next
/// board on the same `App` (`crate::tui::App`, which owns one of these for its
/// whole life).
///
/// It exists because the channel a completion reports on lasts ONE board session
/// and the state it clears does not. `tui::run_inner` builds a new
/// [`crate::watch::EventLoop`] per board session and drops the old receiver at
/// every hand-off (Enter, `Ctrl-F`, Attach, `Ctrl-O`), while `lib::run` re-enters
/// the board on the SAME `App`. A quick reply's `claude -p` child routinely
/// outlives that seam, and its [`AppEvent::SendFinished`] is the ONLY thing that
/// clears `App::sending`. A lost one left the slot full until restart: the
/// `cooking…` tail stayed up, `Ctrl-X d` kept refusing that session, and `Ctrl-R`
/// refused on every row ([`reply_in_flight_refusal`]). The slot is deliberately
/// NOT cleared at the seam instead: the child may still be writing, and the slot
/// is what keeps `Ctrl-X d` off that transcript until the child has finished
/// (`delete::can_delete_target`).
///
/// ONE queue per `App`, and every clone is a handle on it (it is an `Arc`), so the
/// board and each send thread share it. Three operations take its lock, each
/// briefly:
///
/// * [`deliver`](Self::deliver), on the send thread: send on the board's channel,
///   and queue the event ONLY when that send fails because the receiver is gone;
/// * [`drain_then_drop`](Self::drain_then_drop), at the board's teardown: move
///   every buffered `SendFinished` into the queue, then drop the receiver;
/// * [`take`](Self::take), on the next board: one lock-and-take, released before
///   any event is handled.
///
/// The first two each hold the lock across their WHOLE step, and that is what
/// leaves no gap between them. A completion either reaches the channel's buffer
/// before the drain (and the drain moves it) or finds the receiver already
/// dropped (and its failed send queues it). It can never land in the buffer AFTER
/// the drain emptied it, because the receiver is dropped before the drain releases
/// the lock.
///
/// FAIL-SOFT: a poisoned lock is recovered, never propagated. The queue is a plain
/// `Vec` that a panicking holder cannot leave half-written, so its contents are
/// still kept and still taken.
#[derive(Debug, Clone, Default)]
pub struct UndeliveredEvents(Arc<Mutex<Vec<AppEvent>>>);

impl UndeliveredEvents {
    /// Deliver `event` on `tx`, or keep it when the board that owned `tx`'s
    /// receiver has gone.
    ///
    /// The normal path is the plain send it always was: the event goes onto the
    /// channel and the queue is left alone. ONLY a failed send, whose
    /// [`SendError`] hands the event back, queues it. The lock is held across the
    /// send, which never blocks on this unbounded channel, so this step cannot
    /// interleave with [`drain_then_drop`](Self::drain_then_drop).
    pub fn deliver(&self, tx: &Sender<AppEvent>, event: AppEvent) {
        let mut queue = self.lock();
        if let Err(SendError(event)) = tx.send(event) {
            queue.push(event);
        }
    }

    /// Tear a board session's `receiver` down without losing a completion it
    /// ACCEPTED but never read.
    ///
    /// The board's own `recv` loop has stopped by the time this runs, yet a
    /// `SendFinished` can still be sitting in the buffer: the hand-off key was read
    /// first, and a completion can land after it and before the receiver goes. That
    /// window includes the `EventLoop` drop itself, which joins the input reader
    /// BEFORE its receiver field drops. So this empties the buffer through
    /// `try_recv` (a non-blocking read), moves every `SendFinished` into the queue,
    /// discards every other event as teardown always did, and then drops
    /// `receiver`, join included, all under the lock (see the type's doc for why
    /// that leaves no gap). `receiver` is generic so a test can hand in a plain
    /// channel instead of a live `EventLoop`.
    pub fn drain_then_drop<R>(
        &self,
        receiver: R,
        mut try_recv: impl FnMut(&R) -> Option<AppEvent>,
    ) {
        let mut queue = self.lock();
        while let Some(event) = try_recv(&receiver) {
            if matches!(event, AppEvent::SendFinished { .. }) {
                queue.push(event);
            }
        }
        // Dropped HERE, while `queue` still holds the lock. Left to the end of the
        // function, the parameter would drop AFTER the guard: the lock would be
        // released first, and a `deliver` could then send successfully into a buffer
        // that is about to be thrown away.
        drop(receiver);
    }

    /// Every kept completion, oldest first, leaving the queue empty.
    ///
    /// ONE lock-and-take: the guard lives only for this one statement, so the lock
    /// is released before the caller handles any of the events. A send thread
    /// waiting to `deliver` is never held up behind a handler.
    #[must_use]
    pub fn take(&self) -> Vec<AppEvent> {
        std::mem::take(&mut *self.lock())
    }

    /// The queue's lock, recovered if poisoned (see the type's FAIL-SOFT note).
    fn lock(&self) -> MutexGuard<'_, Vec<AppEvent>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
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
/// panic. A send failure on the channel (the board that dispatched this went away)
/// is NOT ignored: the completion goes into `undelivered` instead, and the next
/// board on the same `App` replays it (see [`UndeliveredEvents`]).
///
/// When `req.stop_job` is set it FIRST runs `claude stop <job-id>` to deregister
/// the held background job (so the following `-p -r` is accepted); if the stop
/// itself fails the reply is NOT attempted (it would only be refused) and the stop
/// error is surfaced.
pub fn spawn_send(req: SendRequest, tx: Sender<AppEvent>, undelivered: UndeliveredEvents) {
    std::thread::spawn(move || {
        let (status, success) = run_send(&req);
        // Onto the board's channel while that board is up; into the queue the next
        // board empties once it is not.
        undelivered.deliver(
            &tx,
            AppEvent::SendFinished {
                session_id: req.session_id,
                status,
                success,
            },
        );
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
fn run_send(req: &SendRequest) -> (String, bool) {
    if let Some(job_id) = req.stop_job.as_deref() {
        // Deregister the held job so `-p -r` is accepted; ignore the outcome (see above).
        let _ = run_child(&build_stop_argv(job_id), &req.cwd);
    }
    match run_child(&req.argv, &req.cwd) {
        Ok((success, stdout, stderr)) => status_for_output(success, &stdout, &stderr),
        Err(()) => (SEND_SPAWN_FAILED.to_string(), false),
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
        let (status, success) = match run_child(&req.argv, &req.cwd) {
            Ok((success, stdout, stderr)) => status_for_stop(success, &stdout, &stderr),
            Err(()) => (STOP_SPAWN_FAILED.to_string(), false),
        };
        // A send failure means the receiver (TUI) has gone away; ignore it.
        let _ = tx.send(AppEvent::InterruptFinished {
            session_id: req.session_id,
            status,
            success,
        });
    });
}

/// Map a finished `claude stop` (exit status + captured streams) to a board status
/// and its class.
///
/// On a clean exit it is the neutral [`STOP_OK`] (transient). On a NON-ZERO exit
/// — notably stopping a job id that is already gone — claude's OWN reason is
/// surfaced (the first non-empty stderr, else stdout, line), sanitized for the
/// one-row status line (ANSI/control stripped, one line, length-capped —
/// TERMINAL-SAFE STYLING), so a failed stop never reads as success (sticky).
/// Reuses [`sanitize_status`]/[`strip_error_prefix`] so the interrupt and the send
/// map external errors identically. Pure and unit-tested.
#[must_use]
pub fn status_for_stop(success: bool, stdout: &str, stderr: &str) -> (String, bool) {
    if success {
        return (STOP_OK.to_string(), true);
    }
    let line = stderr
        .lines()
        .chain(stdout.lines())
        .map(sanitize_status)
        .map(|l| strip_error_prefix(&l).to_string())
        .find(|l| !l.is_empty());
    (
        match line {
            Some(line) => format!("{STOP_FAILED_PREFIX}{line}"),
            None => STOP_FAILED_GENERIC.to_string(),
        },
        false,
    )
}

// --- the signal route (Ctrl-K on a record with no stoppable job id) ---------

/// Neutral success for a DELIVERED SIGTERM, and worded for exactly that much.
///
/// It must never read as "the process died": `kill(2)` returns the moment the signal
/// is queued, nothing here waits, and SIGTERM is a request a process may take time to
/// honour. The row clearing is what tells the user it worked.
const SIGNAL_SENT: &str = "SIGTERM sent — not waiting for it to exit";

/// Neutral outcome for `ESRCH`: no process currently carries that pid, so the signal
/// had nothing to reach. Not a failure — it is the state the user was asking for,
/// reached without us — so it is transient rather than sticky. Unix-only, like the
/// `ESRCH` arm that produces it: off unix no syscall runs, so no errno can arrive.
#[cfg(unix)]
const SIGNAL_ALREADY_GONE: &str = "no process with that id — it is already gone";

/// Prefix a surfaced signal failure carries, so it never reads as success. Mirrors
/// [`STOP_FAILED_PREFIX`]'s role for `claude stop`.
const SIGNAL_FAILED_PREFIX: &str = "signal failed: ";

/// Fallback when the OS rejected the signal but left nothing readable to quote.
const SIGNAL_FAILED_GENERIC: &str = "signal failed — the OS rejected it";

/// Reason [`signal_target`] refuses a reported pid with: it cannot be represented as
/// a strictly positive `pid_t`. Reaches the user as [`SIGNAL_FAILED_PREFIX`] + this.
#[cfg(unix)]
const SIGNAL_PID_OUT_OF_RANGE: &str = "reported process id is out of range";

/// Reason [`signal_term`] refuses with OFF unix, where there is no `kill(2)` to call.
/// Reaches the user as [`SIGNAL_FAILED_PREFIX`] + this — a sticky failure, which is
/// the truth: nothing was sent, so the process is still running.
#[cfg(not(unix))]
const SIGNAL_UNSUPPORTED: &str = "sending a signal is not supported on this platform";

/// THE rule for which reported pids a signal may ever take, in two halves. The RANGE
/// half ([`positive_pid_t`]): the pid fits in a `pid_t` (`i32` on every unix) AND is
/// STRICTLY POSITIVE. The BOARD half: it is not `own_pid`, this board's own process id.
/// `None` for anything else — `0`, which `kill(2)` reads as "every process in my
/// group"; any number past `i32::MAX`, which no `pid_t` can hold; and `own_pid`.
///
/// **Why the board's own pid is refused.** snapback installs no SIGTERM handler, so a
/// SIGTERM to its own process ends the board on the spot, with raw mode and the
/// alternate screen still on: an exit that skips the terminal restore, which TERMINAL
/// SAFETY forbids. snapback is not a `claude` process, so a record naming its pid is
/// not describing it — the likeliest way to get one is a stale record whose number was
/// recycled onto the board. Refusing that one number loses nothing. Pid `1` stays
/// allowed: an unprivileged `kill(1, …)` fails with `EPERM`, a sticky failure the
/// board survives.
///
/// `own_pid` is a PARAMETER and is never read in here, so the rule stays pure: the
/// board captures its pid ONCE (`App::own_pid`) and every test states one. Both sides
/// are compared as the raw `u32`, before any narrowing, so no conversion can make two
/// different numbers equal. An `own_pid` past `i32::MAX` excludes nothing extra,
/// because the range half already refuses every such pid.
///
/// This is the single statement of the rule, and [`interrupt_gate`] reads it at the
/// keypress: a pid this refuses gets [`INTERRUPT_PID_UNUSABLE`], so no confirm opens
/// for a pid whose signal could only fail or end the board. The syscall's last check,
/// [`signal_target`], asks the RANGE half again, and asks it alone — its docs say why
/// the board half cannot reach it and why that is safe.
///
/// Pure and compiled on EVERY platform, returning a plain `i32` rather than a `pid_t`:
/// the gate must build everywhere, while `pid_t` and [`signal_target`] are unix-only.
#[must_use]
fn signallable_pid(pid: u32, own_pid: u32) -> Option<i32> {
    positive_pid_t(pid).filter(|_| pid != own_pid)
}

/// The RANGE half of [`signallable_pid`]: `pid` as a `pid_t` value if it fits AND is
/// STRICTLY POSITIVE, else `None`. Stated once here and read by both
/// [`signallable_pid`] (at the gate) and [`signal_target`] (right before `kill(2)`),
/// so the check that keeps a process group or a broadcast out of `kill(2)` still runs
/// twice without being written twice.
///
/// The parser accepts the whole `u32` range, so the conversion is `try_from` and never
/// `as`: `as` would WRAP `u32::MAX` to `-1` (a broadcast) and everything past
/// `i32::MAX` into a negative process GROUP. A pid past `i32::MAX` is unreachable from
/// a real kernel, so refusing it costs nothing and removes the only route by which a
/// sign could flip.
#[must_use]
fn positive_pid_t(pid: u32) -> Option<i32> {
    i32::try_from(pid).ok().filter(|target| *target > 0)
}

/// Narrow a reported `pid` to the `pid_t` [`signal_term`] may pass to `kill(2)` — or
/// REFUSE it, with an [`InvalidInput`](std::io::ErrorKind::InvalidInput) error carrying
/// [`SIGNAL_PID_OUT_OF_RANGE`], when [`positive_pid_t`] rejects it (it cannot be
/// represented as a STRICTLY POSITIVE one). That error has no errno because no syscall
/// ran, and [`signal_term`] returns it unchanged.
///
/// Split out as a pure function precisely BECAUSE the thing it protects cannot be
/// tested through [`signal_term`]: `kill(2)` reads a NEGATIVE argument as a process
/// GROUP, `0` as "every process in my group" and `-1` as a broadcast, so a test that
/// exercised the widening through the real syscall would be the very accident it is
/// meant to prevent. Here the rule is an assertion instead of a comment — and so is
/// the refusal: it is BUILT here, so its kind and its missing errno are tested here,
/// and no test ever has a reason to call [`signal_term`].
///
/// What it asks is the RANGE half of [`signallable_pid`] ([`positive_pid_t`]), which
/// [`interrupt_gate`] already applied before any confirm opened. Asking it again here is
/// DEFENCE IN DEPTH: the gate is not allowed to be the only thing between a record and
/// a process group. The step from its `i32` to `pid_t` is no conversion at all, because
/// `pid_t` IS `i32` on every unix; on a target where it were not, this would stop
/// compiling rather than narrow silently.
///
/// The rule's BOARD half (the board's own pid) is NOT asked again here, deliberately.
/// This step takes the pid alone because [`signal_term`] — the crate's one `unsafe`
/// block, kept exactly as reviewed — hands it the pid alone, and reading the board's
/// pid in here would make it impure. Nothing is lost: a process's id never changes
/// while it runs, so the gate's verdict on the board's pid cannot go stale the way a
/// reported pid's owner can, and the only pid that reaches this step is the one
/// [`signal_plan`] matched against the pid that gate already accepted.
///
/// Unix-only: `pid_t` is a unix type, and off unix [`signal_term`] refuses before any
/// pid would need narrowing.
#[cfg(unix)]
fn signal_target(pid: u32) -> Result<libc::pid_t, std::io::Error> {
    positive_pid_t(pid).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, SIGNAL_PID_OUT_OF_RANGE)
    })
}

/// Send `pid` a **SIGTERM**. The ONE impure step on the signal route, and the only
/// syscall in this crate.
///
/// Thin on purpose: every decision that led here is pure and tested elsewhere
/// ([`interrupt_gate`] chose the route, [`signal_plan`] re-verified the pid,
/// [`signal_target`] narrowed it — or refused it), so this does the syscall and maps
/// its errno — nothing else. It does not even build the out-of-range refusal: that
/// error comes whole from [`signal_target`] and is only propagated here. It returns
/// the [`std::io::Error`](std::io::Error) rather than a `String` because the ERRNO is
/// load-bearing: [`status_for_signal`] has to tell `ESRCH` ("already gone") from every
/// other failure, and a pre-formatted message could not answer that. That keeps the
/// judgement in the pure function and only the effect in here.
///
/// No test calls this, and none may: past [`signal_target`] its only step is the real
/// `kill(2)`, so a test driving it is one broken guard away from `kill(-1, SIGTERM)`,
/// a broadcast to every process this uid may signal. The refusal is asserted on
/// [`signal_target`] and the errno mapping on [`status_for_signal`], both pure.
///
/// Two rules the signature is built to make hard to break:
///
/// * **SIGTERM, never SIGKILL.** SIGTERM is catchable, so the process gets to run its
///   own shutdown — which matters most in the case this route cannot rule out: a
///   process snapback did not start. There is no `SIGKILL` constant anywhere in this
///   crate and no escalation ladder; a process that ignores SIGTERM stays running and
///   says so through the board's own status, which is the honest outcome.
/// * **The pid ITSELF, never a negative pid.** `kill(2)` reads a negative argument as
///   a PROCESS GROUP (and `0`/`-1` as broadcasts), so a sign slip here would signal
///   far more than the one process claude reported. `pid` arrives as `u32` — the
///   parser's own narrowing ([`crate::agents::ReportedAgent::pid`]) — and the only
///   conversion is [`signal_target`], which is pure, REJECTS rather than wraps, and
///   is unit-tested for strict positivity.
///
/// Unix-only. Off unix a same-signature fallback compiles in its place and REFUSES
/// rather than signals, so the driver's one call site builds on every target. The
/// split is two `#[cfg]` ITEMS rather than `#[cfg]` statements inside one body, so
/// this function — the crate's one `unsafe` block — stays exactly as reviewed.
#[cfg(unix)]
pub fn signal_term(pid: u32) -> Result<(), std::io::Error> {
    let target = signal_target(pid)?;
    // SAFETY: `kill` takes two scalars and touches no memory this side owns, so there
    // is no pointer, lifetime or aliasing obligation to uphold. `target` is a strictly
    // positive `pid_t` — the only `Ok` `signal_target` can return — so the call cannot
    // address a process group. Any failure is reported through `errno`, which is read
    // immediately below before anything else can overwrite it.
    let rc = unsafe { libc::kill(target, libc::SIGTERM) };
    if rc == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The off-unix stand-in for the unix [`signal_term`]: same signature, and it signals
/// NOTHING. There is no `kill(2)` (and no `pid_t`) to call here, so it refuses with an
/// [`Unsupported`](std::io::ErrorKind::Unsupported) error carrying
/// [`SIGNAL_UNSUPPORTED`], which [`status_for_signal`] shows as a sticky failure. No
/// errno, because no syscall ran. Shipped targets are darwin and linux only; this
/// exists so the crate still compiles elsewhere, the same compile-everywhere shape as
/// `resume::opener_argv`'s unsupported-target arm.
#[cfg(not(unix))]
pub fn signal_term(pid: u32) -> Result<(), std::io::Error> {
    let _ = pid; // unsupported target: nothing to signal
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        SIGNAL_UNSUPPORTED,
    ))
}

/// Map the result of [`signal_term`] to a board status and its class.
///
/// The signal sibling of [`status_for_stop`], and deliberately NOT a reuse of it:
/// that one takes `(success, stdout, stderr)` because a `claude` CHILD produces
/// streams, while a syscall produces an errno. Synthesizing empty streams to fit the
/// existing signature would fake a child that never ran; a sibling keeps one shape per
/// source of truth. It reuses [`sanitize_status`] so the OS error text is stripped of
/// ANSI/control characters and length-capped exactly like claude's (TERMINAL-SAFE
/// STYLING — no raw escape can reach the ratatui buffer). Pure and unit-tested.
///
/// Three arms, two of them neutral:
///
/// * **`Ok`** → [`SIGNAL_SENT`], TRANSIENT. It claims delivery and nothing more —
///   never that the process exited, which this code does not wait to observe.
/// * **`ESRCH`** → [`SIGNAL_ALREADY_GONE`], TRANSIENT. Nothing carries that pid, so
///   the signal had nothing to reach. That is the asked-for state, not an error.
/// * **anything else** (`EPERM`, and [`signal_target`]'s errno-less out-of-range
///   refusal) → STICKY, quoting the reason, because it means the process is still
///   running and the user needs to know the attempt did not take.
///
/// The `ESRCH` arm is unix-only (`#[cfg(unix)]`, like the `libc` constant it names).
/// Off unix [`signal_term`] never makes a syscall, so no errno can arrive, and its
/// `Unsupported` refusal lands on the sticky arm.
#[must_use]
pub fn status_for_signal(result: Result<(), std::io::Error>) -> (String, bool) {
    let Err(err) = result else {
        return (SIGNAL_SENT.to_string(), true);
    };
    #[cfg(unix)]
    if err.raw_os_error() == Some(libc::ESRCH) {
        return (SIGNAL_ALREADY_GONE.to_string(), true);
    }
    let reason = sanitize_status(&err.to_string());
    (
        if reason.is_empty() {
            SIGNAL_FAILED_GENERIC.to_string()
        } else {
            format!("{SIGNAL_FAILED_PREFIX}{reason}")
        },
        false,
    )
}

// --- background-agent launch (the new-session Ctrl-N draft pane) ------------

/// Neutral success: `claude --bg` exited clean AND printed nothing on stderr, so
/// the agent started with exactly what was asked for. Deliberately the NARROWEST
/// of the three outcomes — see [`status_for_bg_launch`] for why a zero exit alone
/// is not enough to claim it.
const BG_LAUNCH_OK: &str = "background agent started";

/// Prefix for a launch that STARTED (exit 0) but printed something on stderr.
///
/// Worded "started, but" rather than as a failure because both halves are true:
/// the agent IS running, and claude had a reservation about it. The known case is
/// an unrecognized `--agent <name>`, which claude warns about on stderr and then
/// starts the session WITHOUT that agent — a silent downgrade that
/// [`BG_LAUNCH_OK`] would misreport as a clean start.
const BG_LAUNCH_WARNED_PREFIX: &str = "started, but claude warned: ";

/// Prefix a surfaced launch failure carries, so it never reads as a start.
const BG_LAUNCH_FAILED_PREFIX: &str = "launch failed: ";

/// Fallback when the launch exited NON-ZERO but left nothing readable to quote.
const BG_LAUNCH_FAILED_GENERIC: &str =
    "launch failed — claude could not start the background agent";

/// Error status when the child could not even be spawned (no `claude` on PATH,
/// etc.). FAIL-SOFT: a board status, never a panic.
const BG_LAUNCH_SPAWN_FAILED: &str = "could not start claude to launch the background agent";

/// A confirmed background-agent launch handed from the draft pane (pure decision)
/// to the driver ([`crate::tui::run`]), which spawns it via [`spawn_bg_launch`].
///
/// The launch counterpart of [`SendRequest`], carrying a BOARD-LOCAL id instead of
/// a session one. A brand-new agent has no `sessionId` until claude mints one, and
/// the SHORT job id `claude --bg` reports is not that id, so nothing here tries to
/// reconcile the two — the agent reaches the board through the ordinary watcher →
/// reload path, exactly like a session started any other way.
///
/// What the completion still has to be told apart from is ANOTHER dispatch: the
/// draft card outlives its editor, so `launch_id` rides out with the request and
/// back on the event, and the handler closes only the card that matches (see
/// [`crate::tui::app::App::launching_draft`]).
#[derive(Debug, Clone)]
pub struct BgLaunchRequest {
    /// The dispatch this launch is, echoed back on
    /// [`AppEvent::BgLaunchFinished`]. Minted by
    /// [`crate::tui::app::App::dispatch_draft`].
    pub launch_id: u64,
    /// The full argv to spawn; `argv[0]` is the program (always `claude`).
    pub argv: Vec<String>,
    /// A valid directory to run the child in (the launch dir). Never the process cwd.
    pub cwd: PathBuf,
}

/// Build the `claude` argv that starts a BACKGROUND agent with a first prompt:
/// `claude --agent <name> --bg <prompt>`, or `claude --bg <prompt>` when `agent`
/// is `None`.
///
/// A DUMB pure formatter, the sibling of [`build_send_argv`] and
/// [`crate::resume::build_new_argv`] — no trimming or validation beyond
/// formatting (the empty-prompt guard lives at the call site). `--bg` makes
/// claude start the session as a background agent and return immediately, which
/// is what lets the board stay up; the prompt is the trailing positional argument
/// and stays ONE argv element, so a multiline draft reaches claude intact.
///
/// An empty / whitespace-only `agent` is treated exactly like `None` (never a
/// valueless `--agent`), matching [`crate::resume::build_new_argv`]'s guard.
///
/// NO permission flags are passed (`--permission-mode` / `--allowedTools`): a
/// launch INHERITS the user's existing settings, exactly like [`build_send_argv`]
/// and an ordinary interactive start.
#[must_use]
pub fn build_bg_launch_argv(agent: Option<&str>, prompt: &str) -> Vec<String> {
    let mut argv = vec!["claude".to_string()];
    if let Some(name) = agent {
        let name = name.trim();
        if !name.is_empty() {
            argv.push("--agent".to_string());
            argv.push(name.to_string());
        }
    }
    argv.push("--bg".to_string());
    argv.push(prompt.to_string());
    argv
}

/// Gate a background launch on the launch directory still existing, returning the
/// `cwd` to run the child in.
///
/// There is deliberately NO authoritative re-read here, and the reason is the same
/// one [`crate::resume::check_new`] gives: a session that does not exist yet has no
/// source file to read a `cwd` or a `sessionId` out of, so AUTHORITATIVE-FROM-FILE
/// has nothing to be authoritative about. The launch dir (already canonicalized
/// once in `App::launch_dir`) IS the working directory, and the only thing worth
/// checking is that it survived — a board left open while its worktree was deleted
/// must refuse with a status rather than spawn a child into a missing directory.
///
/// Pure (an `is_dir` probe, no process), so the refusal path is unit tested. A
/// `Result` rather than a plan enum because there is exactly one input to carry
/// back, mirroring [`crate::resume`]'s `attach_job_id` gate.
pub fn plan_bg_launch(launch_dir: &Path) -> Result<PathBuf, String> {
    if launch_dir.is_dir() {
        Ok(launch_dir.to_path_buf())
    } else {
        Err(format!(
            "The launch directory no longer exists:\n    {}\n\
             Cannot start a background agent there.",
            launch_dir.display()
        ))
    }
}

/// Map a finished background launch (exit status + captured streams) to a board
/// status and its class.
///
/// This is the launch's honesty seam. It shares its three-row shape with the
/// send's ([`status_for_output`]) — the same outcomes under a different noun,
/// because BOTH paths can fail silently on a zero exit; only the case that proves
/// it differs:
///
/// | Exit | stderr (sanitized) | Status | Class |
/// | --- | --- | --- | --- |
/// | non-zero | anything | [`BG_LAUNCH_FAILED_PREFIX`] + claude's own reason | sticky |
/// | zero | NON-EMPTY | [`BG_LAUNCH_WARNED_PREFIX`] + that reason | sticky |
/// | zero | empty | [`BG_LAUNCH_OK`] | transient |
///
/// The middle row is the whole point. `claude --agent <unknown-name> --bg` exits
/// **0**: it warns on stderr that it does not know the agent and then starts the
/// background session WITHOUT it. Reporting the neutral success there would be a
/// lie of exactly the kind this module exists to prevent — the user asked for an
/// agent, got a plain session, and would have been told it worked. So a zero exit
/// with anything on stderr is surfaced with its own warning-flavoured prefix: it
/// STARTED (true) and claude had something to say about it (also true), and the
/// status says both rather than picking the flattering half.
///
/// Both prefixed rows quote through [`first_quotable_line`] — the rule
/// [`status_for_stop`] applies too — so the launch, the send, and the stop all
/// render an external message identically: one line, ANSI and control characters
/// stripped (TERMINAL-SAFE STYLING), length-capped. Pure and unit-tested — no
/// process is ever spawned.
#[must_use]
pub fn status_for_bg_launch(success: bool, stdout: &str, stderr: &str) -> (String, bool) {
    if !success {
        return (
            match first_quotable_line(stderr).or_else(|| first_quotable_line(stdout)) {
                Some(line) => format!("{BG_LAUNCH_FAILED_PREFIX}{line}"),
                None => BG_LAUNCH_FAILED_GENERIC.to_string(),
            },
            false,
        );
    }
    // Zero exit: a clean stderr is the ONLY clean start (see the table above).
    match first_quotable_line(stderr) {
        Some(line) => (format!("{BG_LAUNCH_WARNED_PREFIX}{line}"), false),
        None => (BG_LAUNCH_OK.to_string(), true),
    }
}

/// Spawn a confirmed background launch on its OWN detached thread and deliver
/// exactly one [`AppEvent::BgLaunchFinished`] when it completes — the UI thread
/// never blocks.
///
/// The launch sibling of [`spawn_send`] / [`spawn_interrupt`]: same
/// fire-and-forget shape (run the child in `cwd` via [`Command::current_dir`],
/// never mutating the process cwd; null stdin so it can never read the board's
/// keystrokes; capture BOTH streams; `wait` to reap it), mapped through
/// [`status_for_bg_launch`]. Capturing stderr is load-bearing rather than
/// symmetric: it is the ONLY evidence of the zero-exit silent downgrade that seam
/// exists to catch.
///
/// FAIL-SOFT throughout: a spawn error yields a neutral error status rather than a
/// panic, and a failure to report back (the board went away) is ignored. Nothing
/// is recorded about WHICH agent was launched — the transcript's `agent-setting`
/// record already answers that and `store::preview` already renders it.
pub fn spawn_bg_launch(req: BgLaunchRequest, tx: Sender<AppEvent>) {
    std::thread::spawn(move || {
        let (status, success) = match run_child(&req.argv, &req.cwd) {
            Ok((success, stdout, stderr)) => status_for_bg_launch(success, &stdout, &stderr),
            Err(()) => (BG_LAUNCH_SPAWN_FAILED.to_string(), false),
        };
        // A send failure means the receiver (TUI) has gone away; ignore it. The
        // request's own id rides back so the board can tell WHICH dispatch this is.
        let _ = tx.send(AppEvent::BgLaunchFinished {
            launch_id: req.launch_id,
            status,
            success,
        });
    });
}

/// Phrasings NO user-facing string may contain, because each of them asserts
/// something about who owns the signalled process that the evidence cannot
/// support.
///
/// A `kind:"interactive"` record has been measured as TWO different processes, so
/// no single reading of that kind holds:
///
/// * `claude 2.1.278`: every record (11/11) was a `claude -p` child with no
///   terminal of its own, all `busy`, and two pty-backed TUIs did not register in
///   short NEGATIVE probes.
/// * `claude 2.1.280`: both records (2/2) were pty-backed TUIs, one `busy` and one
///   `idle`, so there registration is not keyed to a turn in flight.
///
/// The evidence and its limits (`ps` parentage on one machine) are in
/// `docs/agents/DOMAIN.md`, "What `kind: "interactive"` denotes". Nothing snapback
/// can observe proves who owns such a pid, whichever shape it is. The copy has to
/// be true under EITHER reading, so it may describe only what was observed.
///
/// ONE list for the whole crate, test-only: the confirm's render tests
/// (`tui::view`) and the refusal-copy test below both read it, so a phrasing
/// found to mislead is banned everywhere by one edit. `"own terminal"` is here
/// because a refusal once sent the user to open the session "in its own
/// terminal": the `claude -p` shape has no terminal, and in the TUI shape
/// snapback cannot tell where that terminal is or whose it is.
#[cfg(test)]
pub(crate) const OWNERSHIP_CLAIMS: [&str; 5] = [
    "your terminal",
    "another terminal",
    "the terminal that's running it",
    "own terminal",
    "snapback started",
];

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
        let (status, _) = status_for_send(raw);
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
        let (status, _) = status_for_send(raw);
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
            let (status, _) = status_for_send(raw);
            assert!(
                status.starts_with("sent"),
                "unreadable stdout must degrade to a neutral status, got {status:?} for {raw:?}"
            );
        }
    }

    /// A background agent record in a given `state`, carrying a stoppable job id.
    ///
    /// Pid-less by default, which is what the wire shows for a background record
    /// (0/159 at claude 2.1.278). A test that needs one appends
    /// [`ReportedAgent::with_pid`] — including the case that must prove the pid
    /// is IGNORED whenever a job id is also present.
    fn bg(state: &str, job_id: Option<&str>) -> ReportedAgent {
        ReportedAgent {
            kind: "background".to_string(),
            id: job_id.map(str::to_owned),
            state: Some(state.to_string()),
            status: None,
            pid: None,
            started_at_ms: None,
        }
    }

    /// The board's own pid in every gate and rule case that is NOT about it: a
    /// number no case record in this module reports, so those cases exercise the
    /// rest of the rule exactly as they did before the board half existed. The cases
    /// that ARE about it pass a record's own pid instead.
    const BOARD_PID: u32 = 4_242;

    /// `Ctrl-R`'s one-reply-at-a-time rule. With nothing in flight it stays out of
    /// the way. With a reply in flight it refuses, and the refusal names the session
    /// that reply is going to. The rule comes first and the name last, set off by a
    /// space, so a narrow status line cuts the label and keeps the rule. Like every
    /// refusal on this key it claims no owner for any process.
    #[test]
    fn a_reply_in_flight_refuses_the_next_one_and_names_its_session() {
        assert_eq!(
            reply_in_flight_refusal(None),
            None,
            "nothing in flight: the reply gate decides"
        );

        let name = "Fix the payment webhook retries";
        let refusal =
            reply_in_flight_refusal(Some(name)).expect("a reply in flight must refuse the next");
        assert!(
            refusal.starts_with(SEND_IN_FLIGHT_REFUSED),
            "the rule leads: {refusal:?}"
        );
        assert!(
            refusal.ends_with(&format!(" {name}")),
            "the in-flight session is named, last and set apart: {refusal:?}"
        );
        for claim in OWNERSHIP_CLAIMS {
            assert!(
                !refusal.to_lowercase().contains(claim),
                "{refusal:?} must not claim an owner ({claim:?})"
            );
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
    /// Not held → refuse (nothing to stop); `done` → stop immediately; every other
    /// live state → confirm. Stop paths carry the SHORT job id.
    ///
    /// With no stoppable job id the record's `pid` decides between the last rows: a
    /// pid a signal could take → confirm-then-signal, a pid no signal could ever take
    /// (`0`, or past `i32::MAX`) → refuse without a confirm, no pid → refuse. The case
    /// tables at the end pin the BOUNDARY between the two stop mechanisms, which is
    /// the part a later edit is most likely to erode: `claude stop <job-id>` wins
    /// whenever a job id exists, and the pid is not even looked at there — not even a
    /// pid the signal route would refuse.
    #[test]
    fn interrupt_gate_routes_by_agent_state() {
        // Not held at all -> nothing to stop.
        assert_eq!(
            interrupt_gate(None, BOARD_PID),
            InterruptGate::Refuse(INTERRUPT_NOT_LIVE)
        );

        // done -> stop immediately (harmless), carrying the job id.
        assert_eq!(
            interrupt_gate(Some(&bg("done", Some("job-1"))), BOARD_PID),
            InterruptGate::StopNow {
                job_id: "job-1".to_string()
            }
        );

        // A terminal (stopped/failed) agent is already over, so it stops
        // immediately like `done` rather than confirming.
        for terminal in ["stopped", "failed"] {
            assert_eq!(
                interrupt_gate(Some(&bg(terminal, Some("job-1"))), BOARD_PID),
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
                interrupt_gate(Some(&bg(live, Some("job-2"))), BOARD_PID),
                InterruptGate::Confirm {
                    job_id: "job-2".to_string()
                },
                "{live:?} must confirm before stopping"
            );
        }

        // Live with NEITHER a stoppable job id NOR a pid -> refuse: there is
        // nothing on the record left to act on. (`bg` is pid-less by default.)
        assert_eq!(
            interrupt_gate(Some(&bg("working", None)), BOARD_PID),
            InterruptGate::Refuse(INTERRUPT_NO_JOB_ID),
            "no job id and no pid -> nothing here to stop"
        );
        assert_eq!(
            interrupt_gate(Some(&bg("done", Some("   "))), BOARD_PID),
            InterruptGate::Refuse(INTERRUPT_NO_JOB_ID),
            "a blank job id is not stoppable, and there is no pid to fall back to"
        );

        // The pid route, and the boundary that keeps it narrow. A reported pid is
        // the handle of LAST resort: read ONLY where no job id exists, IGNORED
        // wherever one does, and never inferred from (or inferring) `kind`.
        let pid = 29628; // a real pid from the 2.1.278 capture
        for (job_id, state, expected) in [
            // No job id -> the pid is the only handle, so confirm then signal.
            (None, "working", InterruptGate::ConfirmSignal { pid }),
            // ...and it confirms UNCONDITIONALLY: even a record that reports
            // itself finished gets the guard, because a reported bucket cannot
            // prove which process is wearing this pid now. No StopNow here.
            (None, "done", InterruptGate::ConfirmSignal { pid }),
            // A blank job id is no job id, so the pid still decides.
            (Some("   "), "working", InterruptGate::ConfirmSignal { pid }),
            // THE ANTI-WIDENING CASES: a job id is present, so the delegated verb
            // wins on BOTH of its arms and the pid is never looked at.
            (
                Some("job-5"),
                "working",
                InterruptGate::Confirm {
                    job_id: "job-5".to_string(),
                },
            ),
            (
                Some("job-5"),
                "done",
                InterruptGate::StopNow {
                    job_id: "job-5".to_string(),
                },
            ),
        ] {
            assert_eq!(
                interrupt_gate(Some(&bg(state, job_id).with_pid(pid)), BOARD_PID),
                expected,
                "a pid with job_id={job_id:?} in state {state:?} must route to {expected:?}"
            );
        }

        // The pid route opens its confirm only for a pid a signal could take. The
        // edges of that range still confirm...
        for pid in [1, i32::MAX as u32] {
            assert_eq!(
                interrupt_gate(Some(&bg("working", None).with_pid(pid)), BOARD_PID),
                InterruptGate::ConfirmSignal { pid },
                "pid {pid} is a strictly positive pid_t, so it must still confirm"
            );
        }
        // ...and just outside it, a pid IS on the record but no `kill(2)` could ever
        // take it, so the gate refuses in its OWN words (never "no process id", which
        // would be false) instead of opening a confirm that could only fail.
        for pid in [0, i32::MAX as u32 + 1, u32::MAX] {
            for (job_id, state) in [(None, "working"), (None, "done"), (Some("   "), "working")] {
                assert_eq!(
                    interrupt_gate(Some(&bg(state, job_id).with_pid(pid)), BOARD_PID),
                    InterruptGate::Refuse(INTERRUPT_PID_UNUSABLE),
                    "pid {pid} (job_id={job_id:?}, state {state:?}) can never be signalled, \
                     so no confirm may open for it"
                );
            }
            // ANTI-WIDENING, again: a job id still wins, whatever the pid, so the new
            // refusal must never steal a record the delegated verb can stop.
            for (state, expected) in [
                (
                    "working",
                    InterruptGate::Confirm {
                        job_id: "job-5".to_string(),
                    },
                ),
                (
                    "done",
                    InterruptGate::StopNow {
                        job_id: "job-5".to_string(),
                    },
                ),
            ] {
                assert_eq!(
                    interrupt_gate(Some(&bg(state, Some("job-5")).with_pid(pid)), BOARD_PID),
                    expected,
                    "a job id must win over pid {pid} in state {state:?}"
                );
            }
        }
    }

    /// The one rule for which pids a signal may take, asserted on EVERY platform
    /// (the gate reads it everywhere; `signal_target`, unix-only, reads its range
    /// half last): exactly the strictly positive values a `pid_t` can hold, returned
    /// unchanged, MINUS the board's own pid — one number, never a range around it.
    #[test]
    fn signallable_pid_is_the_strictly_positive_pid_t_range_minus_the_boards_own() {
        // Pid 1 stays in: an unprivileged `kill(1, …)` fails with EPERM, a sticky
        // failure the board survives, which is accepted.
        for pid in [1, 2, 29628, i32::MAX as u32 - 1, i32::MAX as u32] {
            assert_eq!(
                signallable_pid(pid, BOARD_PID).map(i64::from),
                Some(i64::from(pid)),
                "pid {pid} is a strictly positive pid_t and must pass through unchanged"
            );
        }
        // `0` is "every process in my group" to `kill(2)`; past `i32::MAX` no `pid_t`
        // exists, and `as` would have WRAPPED these into process groups or `-1`.
        for pid in [0, i32::MAX as u32 + 1, u32::MAX - 1, u32::MAX] {
            assert_eq!(
                signallable_pid(pid, BOARD_PID),
                None,
                "pid {pid} can never be signalled and must be rejected"
            );
        }

        // The BOARD half: the board's own pid is refused wherever it sits in the
        // range, edges included, and its neighbours still pass.
        for own in [1, 2, 29628, i32::MAX as u32 - 1, i32::MAX as u32] {
            assert_eq!(
                signallable_pid(own, own),
                None,
                "pid {own} is the board's own, and a SIGTERM to it would end the board \
                 without its terminal restore, so it must be rejected"
            );
            for neighbour in [own - 1, own + 1] {
                assert_eq!(
                    signallable_pid(neighbour, own),
                    positive_pid_t(neighbour),
                    "pid {neighbour} is not the board's own {own}, so only the range \
                     half may decide it"
                );
            }
        }
        // A board pid no `pid_t` could hold excludes nothing extra: the range half
        // already refuses every such pid, and every in-range pid still passes.
        assert_eq!(signallable_pid(29628, u32::MAX), Some(29628));
        assert_eq!(signallable_pid(u32::MAX, u32::MAX), None);
    }

    /// `Ctrl-K` never offers to SIGTERM the board's OWN process. snapback installs no
    /// SIGTERM handler, so that signal would end the board with raw mode and the
    /// alternate screen still on. snapback is not a `claude` process, so a record
    /// naming its pid is not describing it (most likely a stale record whose number
    /// was recycled onto the board). The gate refuses it before any confirm opens, in
    /// the words it already uses for a pid no signal could take.
    ///
    /// Asserted on the pure gate with the board's pid as a parameter, so nothing reads
    /// the test runner's own pid and nothing is signalled. The same record with a
    /// DIFFERENT pid still confirms, so the refusal is pinned to the one number and not
    /// to the route; and a job id still wins over the board's pid, so the new refusal
    /// cannot steal a record the delegated `claude stop` can take.
    #[test]
    fn interrupt_gate_refuses_the_boards_own_pid_and_still_confirms_any_other() {
        let own = 29628; // a real pid from the 2.1.278 capture, standing in for the board's

        // Every record shape the pid arm reads: no job id, or a blank one.
        for (job_id, state) in [(None, "working"), (None, "done"), (Some("   "), "working")] {
            assert_eq!(
                interrupt_gate(Some(&bg(state, job_id).with_pid(own)), own),
                InterruptGate::Refuse(INTERRUPT_PID_UNUSABLE),
                "a record naming the board's own pid (job_id={job_id:?}, state {state:?}) \
                 must refuse, so no confirm opens"
            );
        }

        // Any other real pid on the same record still confirms — pid 1 included,
        // whose EPERM is the accepted outcome.
        for other in [own - 1, own + 1, 1] {
            assert_eq!(
                interrupt_gate(Some(&bg("working", None).with_pid(other)), own),
                InterruptGate::ConfirmSignal { pid: other },
                "pid {other} is not the board's own {own}, so it must still confirm"
            );
        }

        // ANTI-WIDENING: a job id wins even when the record's pid is the board's own.
        assert_eq!(
            interrupt_gate(Some(&bg("working", Some("job-5")).with_pid(own)), own),
            InterruptGate::Confirm {
                job_id: "job-5".to_string()
            },
            "a job id must win over the board's own pid"
        );
        assert_eq!(
            interrupt_gate(Some(&bg("done", Some("job-5")).with_pid(own)), own),
            InterruptGate::StopNow {
                job_id: "job-5".to_string()
            },
            "a job id must win over the board's own pid"
        );
    }

    /// The refusals a reported record with NO job id can reach say only what was
    /// observed, and point only at moves that do not refuse that same record.
    ///
    /// `INTERRUPT_NO_JOB_ID` (`Ctrl-K` with neither a job id nor a pid),
    /// `INTERRUPT_PID_UNUSABLE` (`Ctrl-K` with no job id and a pid no signal could
    /// take), `SEND_LIVE_REFUSED` (`Ctrl-R`, which refuses every no-job-id record
    /// before it looks at a bucket) and `resume::ATTACH_NO_JOB_ID` (Attach) are the
    /// words a `kind:"interactive"` row meets. Each must hold under BOTH shapes
    /// that kind has been measured as: a `claude -p` child (every record, 11/11, at
    /// `claude 2.1.278`) and a pty-backed TUI, busy or idle (both records, 2/2, at
    /// `claude 2.1.280`; see `docs/agents/DOMAIN.md`, "What `kind: "interactive"`
    /// denotes"). So:
    ///
    /// * none may claim an owner (`OWNERSHIP_CLAIMS`);
    /// * none may call the session "interactive". That is claude's `kind` token, and
    ///   saying the session IS interactive picks the TUI reading;
    /// * none may send the user to a move that refuses the same record. Attach is
    ///   the one that did: `SEND_LIVE_REFUSED` pointed a no-job-id row at Attach,
    ///   which refuses it with `ATTACH_NO_JOB_ID`;
    /// * `SEND_LIVE_REFUSED` may name `Ctrl-K`, but may not PROMISE it: `Ctrl-K`
    ///   signals only a record that carries a pid, and refuses one that does not.
    ///
    /// Each must also still SAY something: the observation that put the user here,
    /// and (where one exists) the move that works in every world, Fork.
    ///
    /// Violations are COLLECTED rather than asserted one by one, so a single run
    /// names every string that still misleads, not only the first.
    #[test]
    fn the_no_job_id_refusals_claim_no_owner_and_name_no_move_that_refuses() {
        let attach_no_job_id = crate::resume::ATTACH_NO_JOB_ID;
        let mut violations = Vec::new();

        for (name, copy) in [
            ("INTERRUPT_NO_JOB_ID", INTERRUPT_NO_JOB_ID),
            ("INTERRUPT_PID_UNUSABLE", INTERRUPT_PID_UNUSABLE),
            ("SEND_LIVE_REFUSED", SEND_LIVE_REFUSED),
            ("ATTACH_NO_JOB_ID", attach_no_job_id),
        ] {
            let lower = copy.to_lowercase();
            for claim in OWNERSHIP_CLAIMS {
                if lower.contains(claim) {
                    violations.push(format!("{name} claims an owner ({claim:?}): {copy:?}"));
                }
            }
            if lower.contains("interactive") {
                violations.push(format!(
                    "{name} picks a reading (\"interactive\"): {copy:?}"
                ));
            }
        }

        // Attach refuses every record with no job id, so no refusal a no-job-id
        // record reaches may send the user there.
        for (name, copy) in [
            ("INTERRUPT_NO_JOB_ID", INTERRUPT_NO_JOB_ID),
            ("INTERRUPT_PID_UNUSABLE", INTERRUPT_PID_UNUSABLE),
            ("SEND_LIVE_REFUSED", SEND_LIVE_REFUSED),
        ] {
            if copy.contains("Attach") {
                violations.push(format!("{name} names Attach, which refuses it: {copy:?}"));
            }
        }
        // `Ctrl-K` is a next move to OFFER, never one to promise.
        for promise in ["will", "end it", "ends it", "kill"] {
            if SEND_LIVE_REFUSED.to_lowercase().contains(promise) {
                violations.push(format!(
                    "SEND_LIVE_REFUSED promises what Ctrl-K may refuse ({promise:?}): \
                     {SEND_LIVE_REFUSED:?}"
                ));
            }
        }

        // What each one must still say.
        for (name, copy, required) in [
            (
                "INTERRUPT_NO_JOB_ID",
                INTERRUPT_NO_JOB_ID,
                "no attachable job",
            ),
            ("INTERRUPT_NO_JOB_ID", INTERRUPT_NO_JOB_ID, "no process id"),
            (
                "INTERRUPT_PID_UNUSABLE",
                INTERRUPT_PID_UNUSABLE,
                "no attachable job",
            ),
            (
                "INTERRUPT_PID_UNUSABLE",
                INTERRUPT_PID_UNUSABLE,
                "cannot be signalled",
            ),
            ("SEND_LIVE_REFUSED", SEND_LIVE_REFUSED, "Ctrl-K"),
            ("SEND_LIVE_REFUSED", SEND_LIVE_REFUSED, "Ctrl-F"),
            ("ATTACH_NO_JOB_ID", attach_no_job_id, "no attachable job"),
            ("ATTACH_NO_JOB_ID", attach_no_job_id, "Ctrl-F"),
        ] {
            if !copy.contains(required) {
                violations.push(format!("{name} must say {required:?}: {copy:?}"));
            }
        }
        // A pid IS on the record `INTERRUPT_PID_UNUSABLE` answers, so it must never
        // borrow `INTERRUPT_NO_JOB_ID`'s "no process id".
        if INTERRUPT_PID_UNUSABLE.contains("no process id") {
            violations.push(format!(
                "INTERRUPT_PID_UNUSABLE denies the pid the record carries: \
                 {INTERRUPT_PID_UNUSABLE:?}"
            ));
        }

        assert!(
            violations.is_empty(),
            "refusal copy that misleads:\n{}",
            violations.join("\n")
        );
    }

    /// The confirm-time re-verification: all FOUR answers, and only one of them
    /// signals.
    ///
    /// This is the guard against pid reuse, so each refusal is asserted on its own
    /// reason rather than on "not Signal" — a refusal that fired for the wrong cause
    /// would still look green under a weaker assertion, and the three causes have
    /// three different next moves for the user.
    #[test]
    fn signal_plan_re_verifies_the_captured_pid_against_a_fresh_record() {
        let captured = 29628; // a real pid from the 2.1.278 capture

        // The ONE signalling answer: still reported, still no stoppable job, still
        // this pid.
        assert_eq!(
            signal_plan(captured, Some(&bg("working", None).with_pid(captured))),
            SignalPlan::Signal { pid: captured },
            "an unchanged record is the only thing that may be signalled"
        );

        // Gone: it ended while the confirm sat open. Its pid is now the likeliest of
        // all to have been recycled, so this must never fall through to a signal.
        assert_eq!(
            signal_plan(captured, None),
            SignalPlan::Refuse(SIGNAL_RECORD_GONE)
        );

        // It gained a stoppable job id — the delegated verb, which always wins, so a
        // matching pid does not buy a signal when a `claude stop` is available.
        assert_eq!(
            signal_plan(
                captured,
                Some(&bg("working", Some("job-9")).with_pid(captured))
            ),
            SignalPlan::Refuse(SIGNAL_NOW_HAS_JOB),
            "a re-routable record must re-route, not signal"
        );
        // Both stale at once — a job id AND a moved pid. Either check alone would
        // refuse, so this row is not about safety; it pins the ORDER, and the order is
        // about which refusal the user reads. `SIGNAL_NOW_HAS_JOB` names a next move
        // (`Ctrl-K` again, on the delegated verb) where `SIGNAL_PID_MOVED` is a dead
        // end, so the re-routable answer must win whenever both apply.
        assert_eq!(
            signal_plan(
                captured,
                Some(&bg("working", Some("job-9")).with_pid(captured + 1))
            ),
            SignalPlan::Refuse(SIGNAL_NOW_HAS_JOB),
            "with both stale, the refusal that names a next move must win"
        );
        // ...and a BLANK job id is still no job id, so it keeps signalling rather
        // than refusing for the wrong reason (the same rule `interrupt_gate` used to
        // put us on this route at all).
        assert_eq!(
            signal_plan(
                captured,
                Some(&bg("working", Some("   ")).with_pid(captured))
            ),
            SignalPlan::Signal { pid: captured }
        );

        // The pid moved — a different number, or none at all. Both are "the fresh
        // record does not name this pid", which is the question being asked.
        assert_eq!(
            signal_plan(captured, Some(&bg("working", None).with_pid(captured + 1))),
            SignalPlan::Refuse(SIGNAL_PID_MOVED),
            "a replaced record must not be signalled on the old pid"
        );
        assert_eq!(
            signal_plan(captured, Some(&bg("working", None))),
            SignalPlan::Refuse(SIGNAL_PID_MOVED),
            "a record that stopped reporting a pid names nothing to signal"
        );
    }

    /// The two steady buckets part ways AT THE GATES, and that split is the point:
    /// a badge that merely LOOKS at rest is not licence to stop a job without
    /// asking.
    ///
    /// `Ended` is claude reporting a terminal token, so both gates act on it
    /// unprompted. `WorkingButIdle` is snapback INFERRING the same thing from a
    /// `state`/`status` contradiction whose documented false positive is a healthy
    /// agent caught mid-flip — so it routes with the LIVE states instead: the reply
    /// gate refuses it, and the interrupt gate confirms first. Granting it `Ended`'s
    /// treatment would stop live work on a guess, with no undo.
    #[test]
    fn the_interrupted_bucket_gates_as_live_while_ended_gates_as_over() {
        // The contradiction shape: a working `state` its own `status` calls `idle`.
        let interrupted = ReportedAgent {
            kind: "background".to_string(),
            id: Some("job-9".to_string()),
            state: Some("working".to_string()),
            status: Some("idle".to_string()),
            pid: None,
            started_at_ms: None,
        };
        assert_eq!(
            agents::classify(&interrupted),
            AgentActivity::WorkingButIdle,
            "the fixture must really reach the bucket under test"
        );

        // Ctrl-R refuses it exactly like a plain `working` agent...
        assert_eq!(
            reply_gate(Some(&interrupted)),
            ReplyGate::Refuse(SEND_LIVE_REFUSED),
            "an INFERRED rest is not proof the turn ended -> refuse like working"
        );
        // ...and Ctrl-K keeps its confirmation guard rather than stopping outright.
        assert_eq!(
            interrupt_gate(Some(&interrupted), BOARD_PID),
            InterruptGate::Confirm {
                job_id: "job-9".to_string()
            },
            "the confirm is the safety net for a bucket that cannot prove its cause"
        );

        // The contrast that makes the split legible: a REPORTED terminal token is
        // claude's own answer, so the same two gates act without asking.
        let ended = bg("stopped", Some("job-9"));
        assert_eq!(agents::classify(&ended), AgentActivity::Ended);
        assert_eq!(
            reply_gate(Some(&ended)),
            ReplyGate::StopThenReply {
                job_id: "job-9".to_string()
            }
        );
        assert_eq!(
            interrupt_gate(Some(&ended), BOARD_PID),
            InterruptGate::StopNow {
                job_id: "job-9".to_string()
            }
        );
    }

    /// A clean stop is the neutral success; a NON-ZERO stop surfaces claude's own
    /// reason (never a false `"stopped"`), with the duplicated `Error:` label
    /// stripped; an empty failure degrades to the generic message.
    #[test]
    fn status_for_stop_maps_success_and_failure() {
        assert_eq!(status_for_stop(true, "", ""), (STOP_OK.to_string(), true));

        let (status, _) = status_for_stop(false, "", "Error: No job matching 70933ea6");
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

        assert_eq!(
            status_for_stop(false, "   \n", "  \n"),
            (STOP_FAILED_GENERIC.to_string(), false)
        );
    }

    /// The one rule `kill(2)` gives no second chance on: whatever `signal_target`
    /// hands back is STRICTLY POSITIVE, so a signal can never reach a process GROUP —
    /// and every other pid is REFUSED with the error `signal_term` returns unchanged.
    ///
    /// Asserted on the pure narrowing rather than through `signal_term`, because the
    /// failure mode is exactly what a test must not perform: `u32::MAX as i32` is
    /// `-1`, and `kill(-1, SIGTERM)` is a broadcast to every process this uid may
    /// signal. Proving it here costs no syscall, and since `signal_target` BUILDS the
    /// refusal, the refusal is proven here too — no test ever calls `signal_term`.
    /// Unix-only, like `signal_target` itself.
    #[cfg(unix)]
    #[test]
    fn signal_target_never_yields_a_group_or_broadcast() {
        // A real reported pid passes through unchanged.
        assert_eq!(signal_target(29628).ok(), Some(29628));
        assert_eq!(signal_target(1).ok(), Some(1));

        // A refusal is the range check's OWN error: `InvalidInput`, and no errno,
        // because no syscall ran to produce one.
        let assert_refused = |pid: u32| {
            let Err(err) = signal_target(pid) else {
                panic!("pid {pid} is not a strictly positive pid_t and must be refused");
            };
            assert_eq!(
                err.kind(),
                std::io::ErrorKind::InvalidInput,
                "pid {pid}: the refusal must be the range check, not an errno"
            );
            assert_eq!(
                err.raw_os_error(),
                None,
                "pid {pid}: no syscall ran, so there is no errno to report"
            );
        };

        // Zero is "every process in my group" to `kill`, so it is not a target. The
        // parser can produce it: a `"pid": 0` on the wire reads as `Some(0)`.
        assert_refused(0);

        // Past `i32::MAX` there is no `pid_t` to use. `as` would WRAP these into
        // negative numbers — process groups — which is the accident being excluded.
        assert_refused(u32::MAX);
        assert_refused(i32::MAX as u32 + 1);

        // The property, over the whole boundary neighbourhood rather than the cases
        // above alone: nothing non-positive ever comes out.
        for pid in [
            0,
            1,
            2,
            29628,
            i32::MAX as u32 - 1,
            i32::MAX as u32,
            i32::MAX as u32 + 1,
            u32::MAX - 1,
            u32::MAX,
        ] {
            if let Ok(target) = signal_target(pid) {
                assert!(
                    target > 0,
                    "pid {pid} narrowed to {target}, which `kill` would read as a \
                     process group or a broadcast"
                );
            }
        }
    }

    /// The signal route's status seam: each of the three arms, and the honesty rule
    /// that shapes them — a delivered SIGTERM is all that may be claimed, because
    /// nothing waits to see the process exit.
    ///
    /// Driven by `io::Error::from_raw_os_error`, so the errno arms are exercised
    /// WITHOUT signalling anything (the suite never touches a real process). The
    /// out-of-range refusal is built by hand the same way, never obtained by calling
    /// `signal_term`. Unix-only: its errno arms name `libc` constants.
    #[cfg(unix)]
    #[test]
    fn status_for_signal_maps_the_three_errno_arms() {
        use std::io::{Error, ErrorKind};

        // Delivered: neutral and TRANSIENT, and it must not overclaim.
        let (status, ok) = status_for_signal(Ok(()));
        assert_eq!((status.as_str(), ok), (SIGNAL_SENT, true));
        for overclaim in ["killed", "died", "exited", "ended", "stopped"] {
            assert!(
                !status.contains(overclaim),
                "a sent SIGTERM must not claim the process {overclaim}: {status}"
            );
        }

        // ESRCH: nothing carries that pid. The state the user wanted, so it is
        // neutral + transient — NOT a failure to act on.
        let (status, ok) = status_for_signal(Err(Error::from_raw_os_error(libc::ESRCH)));
        assert_eq!((status.as_str(), ok), (SIGNAL_ALREADY_GONE, true));
        assert!(
            !status.starts_with(SIGNAL_FAILED_PREFIX),
            "an already-gone process is not a failure: {status}"
        );

        // Any other errno: STICKY, quoting the OS's own reason, because the process
        // is still running and the attempt did not take. EPERM is the real-world
        // case — a pid claude reported that this uid may not signal.
        let (status, ok) = status_for_signal(Err(Error::from_raw_os_error(libc::EPERM)));
        assert!(!ok, "a rejected signal must be sticky: {status}");
        assert!(
            status.starts_with(SIGNAL_FAILED_PREFIX),
            "a rejected signal must read as a failure: {status}"
        );
        assert!(
            !status.contains("sent") && !status.contains("gone"),
            "a rejected signal must never read as either neutral arm: {status}"
        );
        assert!(
            status.len() > SIGNAL_FAILED_PREFIX.len(),
            "it must quote the OS reason, not just the prefix: {status}"
        );

        // The out-of-range refusal: an `InvalidInput` with NO errno, built by hand
        // exactly as `signal_target` builds it. Having no errno must not make it read
        // as neutral — the process was never signalled, so it is still running.
        let (status, ok) = status_for_signal(Err(Error::new(
            ErrorKind::InvalidInput,
            SIGNAL_PID_OUT_OF_RANGE,
        )));
        assert!(!ok, "a refused signal is sticky, not neutral: {status}");
        assert!(
            status.starts_with(SIGNAL_FAILED_PREFIX),
            "a refused signal must read as a failure: {status}"
        );
        assert!(
            status.ends_with(SIGNAL_PID_OUT_OF_RANGE),
            "a refused signal must quote the range check's reason: {status}"
        );

        // The OS text goes through `sanitize_status`, so neither an escape sequence
        // nor a second line in it can reach the ratatui buffer (TERMINAL-SAFE
        // STYLING). The out-of-range refusal just above lands on this same arm.
        let (status, ok) = status_for_signal(Err(Error::new(
            ErrorKind::InvalidInput,
            "bad \u{1b}[31mpid\u{1b}[0m and a\nsecond line",
        )));
        assert!(!ok);
        assert!(status.starts_with(SIGNAL_FAILED_PREFIX));
        assert!(
            status.contains("bad pid"),
            "the reason must survive sanitizing: {status}"
        );
        assert!(
            !status.contains('\u{1b}') && !status.contains('['),
            "no escape sequence may reach the buffer: {status:?}"
        );
        assert!(
            !status.contains('\n'),
            "the one-row status line stays one row: {status:?}"
        );

        // Nothing readable at all still reads as a failure rather than as success.
        assert_eq!(
            status_for_signal(Err(Error::other(""))),
            (SIGNAL_FAILED_GENERIC.to_string(), false)
        );
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
        let (status, _) = status_for_output(false, "", stderr);
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
        assert_eq!(ok, ("sent — $0.0136".to_string(), true));
    }

    /// A non-zero exit with NO readable stdout/stderr degrades to the generic
    /// failure — still never a false success — and an `is_error` JSON payload is
    /// preferred over stderr when present.
    #[test]
    fn failed_send_fallbacks_and_is_error_precedence() {
        assert_eq!(
            status_for_output(false, "", "   \n  \n"),
            (SEND_FAILED_GENERIC.to_string(), false)
        );
        assert_eq!(
            status_for_output(false, "not json", ""),
            (SEND_FAILED_GENERIC.to_string(), false)
        );
        let from_json = status_for_output(
            false,
            r#"{"is_error":true,"result":"tool exploded"}"#,
            "some stderr noise",
        );
        assert_eq!(from_json, ("send failed: tool exploded".to_string(), false));
    }

    /// A raw ANSI escape / control chars from claude's stderr must never reach the
    /// status verbatim (TERMINAL-SAFE STYLING): the sequence is stripped, leaving
    /// only the readable text, whitespace collapsed.
    #[test]
    fn failed_send_strips_ansi_and_control_chars_from_the_reason() {
        let stderr = "\u{1b}[33mError: it \t broke\u{1b}[39m\nsecond line";
        let (status, _) = status_for_output(false, "", stderr);
        assert_eq!(status, "send failed: it broke");
        assert!(
            !status.contains('\u{1b}') && !status.contains('['),
            "no escape residue may remain: {status:?}"
        );
    }

    /// THE zero-exit silent downgrade, pinned. `claude -p` can print a reservation
    /// to stderr and still exit **0** — the background-task sweep terminates the
    /// agent this very reply was aimed at and then exits clean — so a status built
    /// from the exit code and stdout alone reports a flawless `sent — $…` over a
    /// reply whose agent was just killed. A zero exit with anything on stderr must
    /// say so instead.
    #[test]
    fn a_zero_exit_with_stderr_reports_sent_but_warned_never_a_clean_send() {
        let stderr = "Background tasks still running after 600s; terminating.";
        let stdout = r#"{"type":"result","is_error":false,"total_cost_usd":0.0136}"#;
        let (status, transient) = status_for_output(true, stdout, stderr);

        assert!(
            status.starts_with(SEND_WARNED_PREFIX),
            "a warned send must read as sent-but, got {status:?}"
        );
        assert!(
            status.contains("Background tasks still running"),
            "it must quote claude's own warning: {status}"
        );
        assert_ne!(
            (status.as_str(), transient),
            ("sent — $0.0136", true),
            "a zero exit is NOT enough to claim a clean, priced send"
        );
        assert!(
            !transient,
            "a warning is sticky, like every other non-success: {status}"
        );
        // It is not a FAILURE either — the reply itself did land.
        assert!(
            !status.starts_with(SEND_FAILED_PREFIX),
            "a warned send is not a failed one: {status}"
        );
    }

    /// Decision 1 pinned against the rejected signature match: the arm keys off
    /// "claude said something on stderr", NOT on the sweep's wording. An UNRELATED
    /// stderr line on a zero exit warns exactly the same, so narrowing this to a
    /// substring match on the known text later turns this test red.
    #[test]
    fn any_stderr_on_a_zero_exit_warns_not_only_the_known_wording() {
        let unrelated = "Deprecation notice: the pineapple flag moves to --fruit next release";
        let (status, transient) = status_for_output(
            true,
            r#"{"is_error":false,"total_cost_usd":0.5}"#,
            unrelated,
        );
        assert_eq!(
            status,
            format!("{SEND_WARNED_PREFIX}{unrelated}"),
            "any stderr warns — the arm must not be hostage to one wording"
        );
        assert!(!transient, "the warning is sticky: {status}");
    }

    /// The fabricated-failure guard, and the most important test here: "empty"
    /// stderr means what survives [`sanitize_status`], so blank, whitespace-only
    /// AND all-escape stderr alike fall through to today's success mapping
    /// byte-identically. The new arm degrades toward the current behaviour, never
    /// toward a failure the send never had.
    #[test]
    fn a_stderr_that_sanitizes_to_nothing_keeps_todays_success_mapping() {
        let priced = r#"{"type":"result","is_error":false,"total_cost_usd":0.0136}"#;
        let neutral = "not json at all";
        for quiet in [
            "",
            "   ",
            "  \n \n",
            "\u{1b}[0m",
            "\u{1b}[33m\u{1b}[39m\n\t",
        ] {
            assert_eq!(
                status_for_output(true, priced, quiet),
                ("sent — $0.0136".to_string(), true),
                "a stderr that sanitizes to nothing keeps the priced success: {quiet:?}"
            );
            assert_eq!(
                status_for_output(true, neutral, quiet),
                (SEND_OK.to_string(), true),
                "...and keeps the neutral success too: {quiet:?}"
            );
        }
    }

    /// PRECEDENCE: the warning arm exists to replace a FLATTERING status, never to
    /// overwrite an admitted failure. A zero exit whose payload carries `is_error:
    /// true` is already honest, so it comes back byte-identical to today even with
    /// a non-empty stderr sitting alongside it.
    #[test]
    fn an_is_error_payload_keeps_its_failure_over_the_zero_exit_warning() {
        let stdout = r#"{"type":"result","is_error":true,"total_cost_usd":0.01,
                         "result":"tool blew up"}"#;
        assert_eq!(
            status_for_output(
                true,
                stdout,
                "Background tasks still running after 600s; terminating."
            ),
            (SEND_ERROR.to_string(), false),
            "an already-honest failure must not be re-worded as a warning"
        );
        // The stderr changes nothing here, which is the whole point: the verdict is
        // identical to the one the same payload gets with a silent stderr.
        assert_eq!(
            status_for_output(true, stdout, ""),
            (SEND_ERROR.to_string(), false)
        );
    }

    /// The shared quoting helper sanitizes BEFORE stripping the `Error: ` label —
    /// claude may color the line, so the label can sit behind an ANSI escape that
    /// must be removed before the strip can see it. Pinned on the send's WARNING
    /// path too, since that is the path the new prefix opened: no escape residue
    /// may reach the ratatui buffer (TERMINAL-SAFE STYLING).
    #[test]
    fn a_warned_send_sanitizes_before_stripping_the_error_label() {
        let noisy = "\u{1b}[33mError: it \t broke\u{1b}[39m\nsecond line";
        let (status, transient) = status_for_output(true, "{}", noisy);
        assert_eq!(status, format!("{SEND_WARNED_PREFIX}it broke"));
        assert!(!transient, "the warning is sticky: {status}");
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

    // --- background-agent launch --------------------------------------------

    /// A background launch builds `claude --agent <name> --bg <prompt>`, and
    /// `claude --bg <prompt>` with no agent — the prompt as the trailing
    /// positional, no permission flags (the launch inherits the user's settings).
    #[test]
    fn bg_launch_argv_is_claude_bg_with_the_prompt_last() {
        assert_eq!(
            build_bg_launch_argv(Some("planner"), "ship the thing").join(" "),
            "claude --agent planner --bg ship the thing"
        );
        assert_eq!(
            build_bg_launch_argv(None, "ship the thing").join(" "),
            "claude --bg ship the thing"
        );
        let argv = build_bg_launch_argv(Some("planner"), "ship the thing");
        assert!(
            !argv
                .iter()
                .any(|a| a == "--permission-mode" || a == "--allowedTools"),
            "a launch must pass no permission flags: {argv:?}"
        );
        // The prompt is the LAST element, so `--bg` can never swallow it as a value.
        assert_eq!(argv.last().map(String::as_str), Some("ship the thing"));
    }

    /// A blank / whitespace agent pick must never emit a valueless `--agent`; it
    /// collapses to the no-agent invocation, exactly like
    /// `resume::build_new_argv`'s guard.
    #[test]
    fn bg_launch_argv_treats_a_blank_agent_as_none() {
        assert_eq!(
            build_bg_launch_argv(Some(""), "hi").join(" "),
            "claude --bg hi"
        );
        assert_eq!(
            build_bg_launch_argv(Some("   "), "hi").join(" "),
            "claude --bg hi"
        );
    }

    /// A prompt with spaces / newlines stays ONE argv element (never re-split), so
    /// a multiline draft reaches claude intact — the launch mirror of
    /// `argv_keeps_a_multiline_message_as_a_single_argument`.
    #[test]
    fn bg_launch_argv_keeps_a_multiline_message_as_a_single_argument() {
        let argv = build_bg_launch_argv(Some("planner"), "line one\nline two");
        assert_eq!(argv.last().map(String::as_str), Some("line one\nline two"));
        assert_eq!(argv.len(), 5, "no extra args from the newline: {argv:?}");
        let bare = build_bg_launch_argv(None, "line one\nline two");
        assert_eq!(bare.last().map(String::as_str), Some("line one\nline two"));
        assert_eq!(bare.len(), 3, "no extra args from the newline: {bare:?}");
    }

    /// THE honesty case: `claude --agent <unknown> --bg` exits **ZERO**, warns on
    /// stderr, and starts the agent WITHOUT the requested agent. A status that
    /// read as a clean start there would tell the user they got something they did
    /// not get, so a zero exit with a non-empty stderr must surface that stderr.
    #[test]
    fn a_zero_exit_with_stderr_reports_started_but_warned_never_a_clean_start() {
        let stderr = "Warning: unknown agent \"typo-name\"; starting without it";
        let status = status_for_bg_launch(true, "job 70933ea6 started", stderr);

        assert!(
            status.0.starts_with(BG_LAUNCH_WARNED_PREFIX),
            "a warned launch must read as started-but, got {status:?}"
        );
        assert!(
            status.0.contains("unknown agent"),
            "it must quote claude's own warning: {status:?}"
        );
        assert_ne!(
            status,
            (BG_LAUNCH_OK.to_string(), true),
            "a zero exit is NOT enough to claim a clean start"
        );
        // The user must still learn the agent IS running — this is not a failure.
        assert!(
            !status.0.starts_with(BG_LAUNCH_FAILED_PREFIX),
            "a warned launch is not a failed one: {status:?}"
        );
    }

    /// The other two rows of the truth table: a clean exit with a silent stderr is
    /// the neutral success, and a NON-ZERO exit surfaces claude's own reason (never
    /// a false start), with the duplicated `Error:` label stripped and an empty
    /// failure degrading to the generic message.
    #[test]
    fn status_for_bg_launch_maps_clean_success_and_failure() {
        // Zero exit, nothing on stderr -> the neutral success. Whitespace-only
        // stderr is "nothing" too (it survives no sanitize).
        assert_eq!(
            status_for_bg_launch(true, "started", ""),
            (BG_LAUNCH_OK.to_string(), true)
        );
        assert_eq!(
            status_for_bg_launch(true, "started", "  \n \n"),
            (BG_LAUNCH_OK.to_string(), true)
        );

        // Non-zero -> claude's reason, prefixed as a failure.
        let (status, _) = status_for_bg_launch(false, "", "Error: not a git repository");
        assert!(
            status.starts_with(BG_LAUNCH_FAILED_PREFIX),
            "a failed launch must read as a failure: {status}"
        );
        assert!(
            status.contains("not a git repository"),
            "it must quote claude's own reason: {status}"
        );
        assert!(
            !status.contains("Error:"),
            "the Error: label is stripped: {status}"
        );
        assert!(
            !status.contains("started"),
            "a failed launch must NEVER read as started: {status}"
        );

        // Non-zero with only stdout to quote still surfaces it, and a wholly silent
        // failure degrades to the generic message rather than a success.
        assert_eq!(
            status_for_bg_launch(false, "it blew up", ""),
            (format!("{BG_LAUNCH_FAILED_PREFIX}it blew up"), false)
        );
        assert_eq!(
            status_for_bg_launch(false, "  \n", " \n"),
            (BG_LAUNCH_FAILED_GENERIC.to_string(), false)
        );
    }

    /// A raw ANSI escape / control chars from claude's stderr must never reach the
    /// status verbatim (TERMINAL-SAFE STYLING) — on the WARNING path too, not just
    /// the failure one, since that is the path the new prefix opened.
    #[test]
    fn bg_launch_strips_ansi_and_control_chars_from_both_prefixed_paths() {
        let noisy = "\u{1b}[33mWarning: it \t broke\u{1b}[39m\nsecond line";
        let warned = status_for_bg_launch(true, "", noisy);
        assert_eq!(
            warned,
            (format!("{BG_LAUNCH_WARNED_PREFIX}Warning: it broke"), false)
        );
        let failed = status_for_bg_launch(false, "", noisy);
        assert_eq!(
            failed,
            (format!("{BG_LAUNCH_FAILED_PREFIX}Warning: it broke"), false)
        );
        for status in [&warned, &failed] {
            assert!(
                !status.0.contains('\u{1b}') && !status.0.contains('['),
                "no escape residue may remain: {status:?}"
            );
        }
    }

    /// The launch gate is pure existence over the LAUNCH DIR — there is no source
    /// file to re-read, so an existing dir proceeds with itself as the cwd and a
    /// deleted one refuses with a board status rather than spawning into nothing.
    #[test]
    fn plan_bg_launch_gates_on_the_launch_dir_existing() {
        let existing = std::env::temp_dir();
        assert!(existing.is_dir(), "temp_dir should exist");
        assert_eq!(plan_bg_launch(&existing), Ok(existing.clone()));

        let missing = PathBuf::from("/no/such/snapback/launch/dir/anywhere");
        assert!(!missing.exists(), "test path must not exist");
        match plan_bg_launch(&missing) {
            Err(message) => assert!(message.contains("no longer exists"), "{message}"),
            Ok(cwd) => panic!("a missing launch dir must refuse, got {cwd:?}"),
        }
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

    // --- the undelivered-completion queue ------------------------------------
    //
    // Every test below drives `UndeliveredEvents` over a REAL `mpsc` channel and
    // hands it a completion it built itself. None goes near `spawn_send`, so no
    // thread runs a `claude` child.

    /// A quick reply's completion for `session_id`, as `spawn_send` builds it.
    fn finished(session_id: &str, success: bool) -> AppEvent {
        AppEvent::SendFinished {
            session_id: session_id.to_string(),
            status: format!("status for {session_id}"),
            success,
        }
    }

    /// What each event IS, in order: a `SendFinished` by its session id, anything
    /// else as `"<other>"`. `AppEvent` has no `PartialEq`, so this is how an order
    /// and a filter are compared.
    fn finished_ids(events: &[AppEvent]) -> Vec<String> {
        events
            .iter()
            .map(|event| match event {
                AppEvent::SendFinished { session_id, .. } => session_id.clone(),
                _ => "<other>".to_string(),
            })
            .collect()
    }

    /// Task 10.3: a completion whose board has gone (the receiver was DROPPED at a
    /// hand-off) is kept in the queue, whole, instead of being thrown away with the
    /// failed send.
    #[test]
    fn a_completion_whose_board_is_gone_is_queued_instead_of_dropped() {
        let queue = UndeliveredEvents::default();
        let (tx, rx) = std::sync::mpsc::channel();
        drop(rx);

        queue.deliver(&tx, finished("gone", false));

        let kept = queue.take();
        assert_eq!(
            finished_ids(&kept),
            ["gone"],
            "a completion the channel refused must be queued, not dropped"
        );
        assert!(
            matches!(
                &kept[0],
                AppEvent::SendFinished { status, success: false, .. } if status == "status for gone"
            ),
            "the queued completion keeps its status and its class: {kept:?}"
        );
    }

    /// Task 10.3: while the board is up, the completion goes onto its channel
    /// exactly as before, and nothing is queued, so a live board never sees it
    /// twice.
    #[test]
    fn a_completion_whose_board_is_up_is_delivered_and_not_queued() {
        let queue = UndeliveredEvents::default();
        let (tx, rx) = std::sync::mpsc::channel();

        queue.deliver(&tx, finished("live", true));

        let delivered = rx.try_recv().expect("the completion is on the channel");
        assert_eq!(finished_ids(&[delivered]), ["live"]);
        assert!(
            queue.take().is_empty(),
            "a delivered completion must not also be queued"
        );
    }

    /// Stands in for the board's `EventLoop`: a real receiver, plus a record of
    /// whether the queue's lock was HELD at the instant it dropped.
    struct WatchedReceiver {
        rx: std::sync::mpsc::Receiver<AppEvent>,
        queue: UndeliveredEvents,
        locked_at_drop: std::rc::Rc<std::cell::Cell<Option<bool>>>,
    }

    impl Drop for WatchedReceiver {
        fn drop(&mut self) {
            let held = matches!(
                self.queue.0.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            );
            self.locked_at_drop.set(Some(held));
        }
    }

    /// Task 10.3, the teardown window: a completion the dying board's channel
    /// ACCEPTED but never read is moved into the queue, and every other buffered
    /// event is discarded as teardown always did. That includes the other
    /// completion kinds, which this queue does not carry yet.
    ///
    /// Then the no-gap half: the receiver is gone once the drain returns, and it
    /// dropped while the queue's lock was still held. Only that ordering stops a
    /// `deliver` from slipping a completion into the buffer between the drain and
    /// the drop.
    #[test]
    fn the_teardown_drain_keeps_only_buffered_completions_and_drops_the_receiver_under_the_lock() {
        let queue = UndeliveredEvents::default();
        let (tx, rx) = std::sync::mpsc::channel();
        for event in [
            AppEvent::Tick,
            finished("first", true),
            AppEvent::SessionsChanged,
            AppEvent::InterruptFinished {
                session_id: "stopped".to_string(),
                status: "stopped".to_string(),
                success: true,
            },
            finished("second", false),
        ] {
            tx.send(event).expect("the receiver is still up");
        }
        let locked_at_drop = std::rc::Rc::new(std::cell::Cell::new(None));
        let receiver = WatchedReceiver {
            rx,
            queue: queue.clone(),
            locked_at_drop: std::rc::Rc::clone(&locked_at_drop),
        };

        queue.drain_then_drop(receiver, |receiver| receiver.rx.try_recv().ok());

        assert_eq!(
            finished_ids(&queue.take()),
            ["first", "second"],
            "only the buffered SendFinished events are kept, oldest first"
        );
        assert!(
            tx.send(AppEvent::Tick).is_err(),
            "the receiver is gone once the drain returns"
        );
        assert_eq!(
            locked_at_drop.get(),
            Some(true),
            "the receiver must drop while the queue's lock is held, or a completion \
             can land in its buffer after the drain"
        );
    }

    /// Task 10.2's FAIL-SOFT rule: a holder that panicked with the lock poisons it,
    /// and the queue still keeps a completion and hands every one back. Nothing
    /// panics.
    #[test]
    fn a_poisoned_queue_still_keeps_and_hands_back_its_completions() {
        let queue = UndeliveredEvents::default();
        let (tx, rx) = std::sync::mpsc::channel();
        drop(rx);
        queue.deliver(&tx, finished("before", true));

        let holder = queue.clone();
        let poisoner = std::thread::spawn(move || {
            let _held = holder.0.lock().expect("not poisoned yet");
            panic!("poison the undelivered queue on purpose");
        });
        assert!(
            poisoner.join().is_err(),
            "precondition: the holder panicked"
        );
        assert!(queue.0.is_poisoned(), "precondition: the lock is poisoned");

        queue.deliver(&tx, finished("after", false));
        assert_eq!(
            finished_ids(&queue.take()),
            ["before", "after"],
            "a poisoned queue still keeps and hands back every completion"
        );
        assert!(queue.take().is_empty(), "the take emptied it");
    }
}
