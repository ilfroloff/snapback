//! Session deletion: pure guards plus the thin FS remove driver.
//!
//! This is snapback's FIRST store MUTATION path. Everywhere else the Claude
//! store under `~/.claude/projects/` is treated as read-only, hostile input;
//! HARD delete is the one gated exception (behind a confirm modal and the WRITER
//! guard below). The gate follows the pure-core / thin-driver split
//! (PATTERNS §3): [`can_delete`], [`status_for_delete`] and [`toggle_hidden`] are
//! pure, fully unit-tested decisions; [`remove`] is the thin, impure FS driver
//! that performs the unlink and spawns no process.
//!
//! A confirm may target the selected session ALONE or its whole fork lineage, but
//! that is the caller's fan-out: every function here still decides about, or acts
//! on, exactly ONE session, and a lineage is just the loop the caller runs.
//!
//! [`remove`] is SUBAGENT-EXCLUSION-safe BY CONSTRUCTION (AGENTS.md SUBAGENT
//! EXCLUSION): it only ever targets the session it is HANDED — that session's own
//! `<id>.jsonl` file and the sibling `<id>/` directory derived from that file's own parent + stem.
//! It never constructs, matches, or descends into a `subagents/` path any other
//! way, and never another session's directory.

use std::collections::HashSet;

use crate::agents::{self, AgentActivity, ReportedAgent};
use crate::store::Session;

/// User-facing refusal returned by [`can_delete`] when claude reports the target
/// as an OPEN INTERACTIVE session.
///
/// This is the arm the guard exists for: an interactive session is a claude
/// window someone is typing in, so its next keystroke appends to the very
/// transcript being unlinked — a writer is present by definition. Worded like the
/// resume-gate refusals ([`crate::resume::ATTACH_NOT_LIVE`]): it states what was
/// OBSERVED and points at the next move rather than diagnosing. Soft-hide has no
/// such guard — it is reversible and touches no bytes on disk.
pub const DELETE_INTERACTIVE_REFUSAL: &str = "claude has this session open interactively — \
     close that window first, then hard-delete.";

/// User-facing refusal returned by [`can_delete`] when claude still reports the
/// target as a running agent.
///
/// The copy claims neither a KIND nor an activity, because the arm covers more
/// than either would state. Three buckets reach it:
///
/// * [`AgentActivity::Working`] — a turn really is in flight.
/// * [`AgentActivity::Idle`] — the agent is up, between turns. Nothing is being
///   written this instant, but claude is holding it and may write next.
/// * [`AgentActivity::Other`] — the qualifier was unreadable or absent, so
///   NOTHING is known about what it is doing.
///
/// Only the first is literally *working*; saying so for the other two would claim
/// more than the signal carries. The KIND is the same discipline one field over:
/// this is also the message for a record whose `kind` DRIFTED or arrived empty
/// (see [`can_delete`]'s background arm), so calling it a "background agent"
/// would assert a shape the record never stated. What holds across every one of
/// those worlds is that claude still lists the session as an active agent, so
/// that — and only that — is what the message reports (PATTERNS §1, the same
/// "say only what you observed" rule behind [`crate::resume::ATTACH_NOT_LIVE`]).
pub const DELETE_RUNNING_REFUSAL: &str = "claude still reports this session as a running \
     agent — let it finish or stop it in Claude Code, then hard-delete.";

/// User-facing refusal returned by [`can_delete_target`] when SNAPBACK ITSELF has a
/// quick reply in flight to the target.
///
/// The THIRD writer, and the only one [`can_delete`] structurally cannot see. It
/// says "snapback" rather than "claude" because that is what was observed: the
/// child doing the writing is one snapback spawned, and telling the user to close a
/// claude window would point at the wrong thing entirely.
pub const DELETE_SENDING_REFUSAL: &str = "snapback is still sending a reply to this session — \
     wait for it to land, then hard-delete.";

/// Pure WRITER guard for a HARD delete: refuse while claude holds a writer on the
/// transcript, allow it otherwise.
///
/// `reported` is the target's record from the caller's FRESHLY probed active list
/// (`App::live_agents_now`), read at the moment of the confirm — the same
/// re-ask-at-hand-off posture the resume gate uses, never a stale poll. `None`
/// means claude did not report the session at all.
///
/// The question is **"is anything writing this file?"**, NOT "does claude know
/// this session?". Bare MEMBERSHIP was the old rule and it was far too wide:
/// claude's active list is dominated by PARKED background agents (measured on one
/// machine: 72 of 74 records were background and stopped, most untouched for
/// days, with ZERO open file descriptors on their transcripts — claude appends by
/// RE-OPENING the path, so a parked agent has no write in flight to corrupt).
/// Refusing all of them made hard delete unusable for ~97% of the rows claude
/// reports. What remains for a parked agent is RESURRECTION, not corruption:
/// attaching and replying later re-creates the file holding only the new lines.
/// That is a consequence the confirm modal STATES, not a reason to refuse.
///
/// The arms:
///
/// * `None` — claude is not holding it ⇒ `Ok(())`.
/// * `Some` INTERACTIVE ⇒ [`DELETE_INTERACTIVE_REFUSAL`]. The one arm that must
///   never widen.
/// * `Some` BACKGROUND — every other `kind`, INCLUDING a drifted or empty one:
///   an unrecognized kind falls to this arm rather than the permissive `None`,
///   because this arm carries its own conservative matrix (below) while `None`
///   deletes outright. The bucket then decides: the RESTING ones
///   ([`AgentActivity::NeedsInput`], [`AgentActivity::WorkingButIdle`],
///   [`AgentActivity::Done`], [`AgentActivity::Ended`]) are parked and deletable,
///   while [`AgentActivity::Working`], [`AgentActivity::Idle`] and
///   [`AgentActivity::Other`] are refused with [`DELETE_RUNNING_REFUSAL`]. An
///   unknown or absent qualifier therefore fails toward REFUSING: an irreversible
///   unlink must not be authorized by a signal that could not be read.
///
/// The two FINISHED arms, [`AgentActivity::Done`] and [`AgentActivity::Ended`],
/// are in the ALLOW half for one reason: the run is OVER, so there is no turn in
/// flight and no writer to race. `Ended` is claude reporting a terminal
/// `stopped`/`failed` token outright; `Done` is it reporting a clean completion.
/// Both are REPORTED facts, not inferences, so neither falls under the
/// "unreadable qualifier fails toward refusing" rule that keeps
/// [`AgentActivity::Other`] out. The rest of the codebase reads them the same
/// way: [`agents::is_active`] rests both, and both send gates treat them
/// identically ([`crate::send::reply_gate`] stops-then-replies,
/// [`crate::send::interrupt_gate`] stops at once) precisely BECAUSE the run has
/// finished.
///
/// **A correction worth keeping, because this arm was once thought unreachable.**
/// An earlier revision of this comment claimed the BARE list `reported` comes from
/// carries no `done`, making the `Done` arm purely defensive — a finished session
/// would arrive UNREPORTED and be allowed by the `None` arm instead. That was an
/// ABSENCE inferred from two samples, and it was wrong: claude keeps a `done`
/// background job in its ACTIVE list for a while before reaping it (see
/// [`crate::agents`]'s module docs and `QUALIFIER_DONE`). So the arm is LIVE, not
/// contingent, and the old note's own warning stands vindicated — had the arm been
/// deleted as dead-code cleanup, that reaping window would have silently started
/// REFUSING every just-finished session. Do not re-derive an arm's reachability
/// here from sampling; judge it by what the bucket MEANS for a writer.
///
/// **Deliberately NOT expressed over [`agents::is_active`]**, even though the two
/// matrices nearly agree — they differ only at [`AgentActivity::Idle`], which
/// `is_active` calls resting and this guard refuses. `is_active` answers a
/// COSMETIC question (does a list badge's dot pulse), and a future retune of that
/// pulse must never silently widen an IRREVERSIBLE gate. Two small predicates
/// over the same enum, each written where its own consequence lives, is the
/// point; sharing one would couple a repaint to an unlink.
///
/// Returns `Err(user-facing message)` so the caller can hand it straight to
/// `set_status`, or `Ok(())` when the delete may proceed.
pub fn can_delete(reported: Option<&ReportedAgent>) -> Result<(), String> {
    let Some(agent) = reported else {
        // Not in claude's active list -> nothing is holding the file open.
        return Ok(());
    };
    if agent.kind == agents::KIND_INTERACTIVE {
        return Err(DELETE_INTERACTIVE_REFUSAL.to_string());
    }
    // Background (and any drifted kind): the activity bucket decides.
    match agents::classify(agent) {
        AgentActivity::NeedsInput
        | AgentActivity::WorkingButIdle
        | AgentActivity::Done
        | AgentActivity::Ended => Ok(()),
        AgentActivity::Working | AgentActivity::Idle | AgentActivity::Other => {
            Err(DELETE_RUNNING_REFUSAL.to_string())
        }
    }
}

/// The FULL writer guard a hard delete must pass: [`can_delete`]'s claude-side
/// verdict, plus the one writer that verdict structurally cannot see — snapback's
/// OWN in-flight quick reply.
///
/// **Why a second fact is needed at all.** [`can_delete`] answers "is CLAUDE
/// holding a writer?" off a freshly probed active list, and that list is
/// deliberately made to NOT contain a session being quick-replied to: before
/// running `claude -p -r <id>`, [`crate::send`] first `claude stop`s the held job
/// precisely so `-p -r` is accepted (see `run_send`). For the whole span of that
/// send the target is therefore ABSENT from claude's active list while a `claude`
/// child snapback spawned is appending to its transcript — the exact writer this
/// module exists to protect, arriving through the one door the probe cannot see.
/// `can_delete`'s `None` arm reads "not in claude's active list ⇒ nothing is
/// holding the file open", which is true of claude and false of snapback.
///
/// The send runs on a DETACHED thread and the board stays fully interactive
/// meanwhile — no key is blocked during a send — so `Ctrl-X d` really is reachable
/// in that window. This is not a theoretical race.
///
/// `reply_in_flight` is the caller's answer to "is a quick reply to THIS id still
/// running", from [`crate::tui::app::App::sending_to`]. It is checked FIRST because
/// it is the more specific fact: when snapback is mid-send the claude-side verdict
/// is `Ok` by construction, so consulting it first would report nothing at all.
///
/// Kept as a COMPOSITION rather than a wider [`can_delete`] on purpose, for the
/// reason that function's own docs give for not reusing [`agents::is_active`]: two
/// small predicates over separate facts, each written where its consequence lives.
/// The questions have different sources (claude's probe vs. snapback's own state)
/// and different remedies, so they keep different messages.
pub fn can_delete_target(
    reported: Option<&ReportedAgent>,
    reply_in_flight: bool,
) -> Result<(), String> {
    if reply_in_flight {
        return Err(DELETE_SENDING_REFUSAL.to_string());
    }
    can_delete(reported)
}

/// Board copy for a target that was ALREADY GONE from the board when the pass
/// reached it — the leftover of [`status_for_delete`]'s reconciliation, reported
/// on its own when the pass had a single target.
///
/// The ids a confirm acts on are captured when the modal OPENS, and a
/// `SessionsChanged` reload can drop a row while it sits there, so this is a real
/// state rather than a defensive branch. Nothing was unlinked and nothing was
/// refused, so silence would read as a successful delete — on the one action the
/// user must never be left guessing about. It states what was OBSERVED (the row
/// was not there) and not a cause the pass cannot know: the file may have been
/// removed elsewhere, or a previous pass may have taken it.
const DELETE_TARGET_GONE: &str = "that session was already gone from the board — \
     nothing was deleted.";

/// The board status a finished HARD delete reports, as a pure function of what
/// the pass actually did: how many targets it had, how many transcripts it
/// removed, which members [`can_delete`] REFUSED, and which ones the filesystem
/// FAILED to remove.
///
/// `None` means "say nothing" — a clean single delete speaks through the row
/// leaving the board.
///
/// It RECONCILES the tally against `targets` rather than trusting it: every id
/// handed in must have been removed, refused, or failed, and whatever is left
/// over was gone from the board before the pass reached it (the ids are captured
/// when the confirm OPENS; a reload can drop a member while it is open). Those
/// leftovers are counted and reported — `, N already gone` in a lineage,
/// [`DELETE_TARGET_GONE`] on its own for a single target — because a status that
/// dropped them would under-report the target count and let "2 deleted" stand for
/// a family of three. `saturating_sub` keeps the arithmetic total: a
/// double-counted member yields no phantom tail rather than an underflow panic.
///
/// Two shapes, because one target and a lineage want different answers:
///
/// * **One target** — the refusal (else the FS error, else the vanished notice)
///   IS the whole story, so it is reported VERBATIM. A count would bury the
///   reason behind a tally.
/// * **A lineage** — a per-member verdict cannot be shown one at a time, so the
///   SPLIT is reported. Refusals, FS errors and vanished rows are counted
///   SEPARATELY and never merged: an unlink that failed on a permission error is
///   not a skipped running agent, and a row that had already left the board is
///   neither, so reporting either as the other would be a false claim about why
///   the file is still there. "running" is the honest umbrella over both refusal
///   arms — an open interactive window and a still-reported agent are both claude
///   still holding the session.
///
/// Pure so the copy is testable without a store, a probe, or a terminal
/// (PATTERNS §3).
pub(crate) fn status_for_delete(
    targets: usize,
    removed: usize,
    refusals: &[String],
    errors: &[String],
) -> Option<String> {
    // Every target lands in exactly one bucket; the remainder never reached the
    // filesystem because its row had already left the board.
    let gone = targets.saturating_sub(removed + refusals.len() + errors.len());
    if targets <= 1 {
        return refusals
            .first()
            .or_else(|| errors.first())
            .cloned()
            .or_else(|| (gone > 0).then(|| DELETE_TARGET_GONE.to_string()));
    }
    let mut status = format!("{removed} deleted");
    if !refusals.is_empty() {
        status.push_str(&format!(", {} skipped (running)", refusals.len()));
    }
    if !errors.is_empty() {
        status.push_str(&format!(", {} failed to remove", errors.len()));
    }
    if gone > 0 {
        status.push_str(&format!(", {gone} already gone"));
    }
    Some(status)
}

/// Flip the hidden state of a whole GROUP of session ids together, pivoting on
/// `pivot`'s current membership: when `pivot` is currently visible, HIDE every id
/// in `members`; when it is already hidden, EXPOSE them all. Returns the NEW hidden
/// state (`true` = now hidden).
///
/// A background-fork lineage must hide and expose as ONE unit — otherwise hiding a
/// folded head would just drop it and let the fold re-head to a surviving fork, so
/// the lineage never leaves the board. A rootless singleton passes `members` of
/// length one (itself). Pivoting on one id (rather than each member's own state)
/// resolves a partially-hidden group cleanly to the pivot's opposite. The caller
/// owns the side effects — persist via `hidden::save_hidden` and re-filter —
/// keeping this decision pure and trivially testable.
pub fn toggle_hidden(set: &mut HashSet<String>, members: &[String], pivot: &str) -> bool {
    let hide = !set.contains(pivot);
    for id in members {
        if hide {
            set.insert(id.clone());
        } else {
            set.remove(id);
        }
    }
    hide
}

/// Thin, impure HARD-delete driver: unlink the session's `<id>.jsonl` transcript
/// and, when it is present, remove the sibling `<encoded-cwd>/<id>/` directory
/// that holds its subagent transcripts.
///
/// SUBAGENT EXCLUSION: the id directory is derived ONLY from this session's own
/// file — the file's PARENT joined with its file STEM, so `<encoded-cwd>/<id>.jsonl`
/// yields exactly `<encoded-cwd>/<id>/`. Removal can therefore never reach a
/// `subagents/` path by any other route, and never another session's directory.
/// The directory is removed only when it actually exists (a session with no
/// subagents simply has none). Spawns no process.
///
/// Returns the first FS error (e.g. the transcript already vanished from the
/// live store); the caller keeps the board up and reports it, matching the
/// fail-soft posture elsewhere.
pub(crate) fn remove(session: &Session) -> std::io::Result<()> {
    std::fs::remove_file(&session.file)?;

    // Derive the sibling id dir from the file's OWN parent + stem — the only
    // path removal may ever target besides the file itself. `file_stem` on
    // `<id>.jsonl` is `<id>`, so this resolves to exactly `<encoded-cwd>/<id>/`.
    if let (Some(parent), Some(stem)) = (session.file.parent(), session.file.file_stem()) {
        let id_dir = parent.join(stem);
        if id_dir.is_dir() {
            std::fs::remove_dir_all(&id_dir)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A unique, isolated temp dir under `std::env::temp_dir()` — NEVER the real
    /// `~/.claude/projects` store. Mirrors the `snapback-<tag>-<pid>-<nanos>`
    /// convention used across the crate's tests.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "snapback-delete-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// A synthetic reported record carrying only what the guard reads: the
    /// `kind` it is judged by and the `state`/`status` pair `classify` buckets.
    fn agent(kind: &str, state: Option<&str>, status: Option<&str>) -> ReportedAgent {
        ReportedAgent {
            kind: kind.to_string(),
            id: None,
            state: state.map(str::to_owned),
            status: status.map(str::to_owned),
            name: None,
        }
    }

    /// Build a minimal `Session` pointing at `file`. `remove` only reads
    /// `session.file`, so the other fields carry inert placeholders.
    fn session_at(file: PathBuf) -> Session {
        Session {
            file,
            session_id: "sess-x".to_string(),
            cwd: PathBuf::from("/tmp/project"),
            git_branch: None,
            timestamp: None,
            repo: "project".to_string(),
            label: "label".to_string(),
            root_uuid: None,
            msg_count: 0,
            content_index: String::new(),
        }
    }

    // --- can_delete: the writer matrix ------------------------------------

    /// The WHOLE guard matrix, arm by arm, asserting the EXACT refusal const
    /// wherever it refuses.
    ///
    /// The rule under test is "is a writer present?", NOT "does claude know this
    /// session?" — so the interesting half is everything claude REPORTS that is
    /// nonetheless deletable. A parked background agent (needs-input, interrupted)
    /// holds no open handle on its transcript; refusing it was the defect, and it
    /// is the majority of claude's active list. The `done` row is the one that is
    /// NOT drawn from that list — see its own note below.
    ///
    /// Table-driven so a mutant that collapses two arms into one cannot survive:
    /// each row names its own bucket, and both refusal texts appear as distinct
    /// expectations.
    #[test]
    fn can_delete_allows_parked_agents_and_refuses_every_writer() {
        // (case, record, expected verdict)
        let cases: [(&str, Option<ReportedAgent>, Result<(), &str>); 18] = [
            // Claude does not report it at all -> nothing can be writing it.
            ("not reported", None, Ok(())),
            // An INTERACTIVE session is a window someone is typing in: its next
            // keystroke appends here. Refused whatever the qualifier says —
            // `kind` is judged BEFORE the bucket, which is what makes even a
            // `done` interactive record refuse.
            (
                "interactive idle",
                Some(agent("interactive", None, Some("idle"))),
                Err(DELETE_INTERACTIVE_REFUSAL),
            ),
            (
                "interactive busy",
                Some(agent("interactive", None, Some("busy"))),
                Err(DELETE_INTERACTIVE_REFUSAL),
            ),
            (
                "interactive with no qualifier at all",
                Some(agent("interactive", None, None)),
                Err(DELETE_INTERACTIVE_REFUSAL),
            ),
            (
                "interactive that reported done",
                Some(agent("interactive", Some("done"), None)),
                Err(DELETE_INTERACTIVE_REFUSAL),
            ),
            // PARKED background agents: stopped, waiting on the user. The bulk of
            // claude's active list, and deletable.
            (
                "background blocked",
                Some(agent("background", Some("blocked"), None)),
                Ok(()),
            ),
            (
                "background blocked with an idle status",
                Some(agent("background", Some("blocked"), Some("idle"))),
                Ok(()),
            ),
            (
                "background waiting",
                Some(agent("background", Some("waiting"), None)),
                Ok(()),
            ),
            // The interrupted shape: a working state claude's own status calls
            // idle. Nothing is churning, so nothing is writing.
            (
                "background working with an idle status (interrupted)",
                Some(agent("background", Some("working"), Some("idle"))),
                Ok(()),
            ),
            // Reported completion. A LIVE arm, not the contingency an earlier
            // revision of `can_delete`'s doc comment took it for: claude keeps a
            // `done` background job in its active list until it reaps it, so a
            // just-finished session really does arrive here as a record rather
            // than as the "not reported" row above.
            (
                "background done",
                Some(agent("background", Some("done"), None)),
                Ok(()),
            ),
            // The TERMINAL tokens claude reports outright: the run is over, so
            // there is no turn in flight and no writer to race. These are
            // REPORTED facts, not inferences, so they do not fall under the
            // fail-soft "unreadable qualifier refuses" rule below.
            (
                "background stopped",
                Some(agent("background", Some("stopped"), None)),
                Ok(()),
            ),
            (
                "background failed",
                Some(agent("background", Some("failed"), None)),
                Ok(()),
            ),
            // A turn really is in flight.
            (
                "background working",
                Some(agent("background", Some("working"), None)),
                Err(DELETE_RUNNING_REFUSAL),
            ),
            (
                "background busy",
                Some(agent("background", Some("busy"), None)),
                Err(DELETE_RUNNING_REFUSAL),
            ),
            // Up between turns: claude holds it and may write next. This is the
            // ONE arm where the guard deliberately disagrees with `is_active`,
            // which calls `Idle` resting because a resting BADGE must not pulse.
            (
                "background idle",
                Some(agent("background", Some("idle"), None)),
                Err(DELETE_RUNNING_REFUSAL),
            ),
            // FAIL-SOFT toward REFUSING: an unreadable or absent qualifier says
            // nothing, and nothing may not authorize an irreversible unlink.
            (
                "background with an unknown qualifier",
                Some(agent("background", Some("compacting"), None)),
                Err(DELETE_RUNNING_REFUSAL),
            ),
            (
                "background with no qualifier at all",
                Some(agent("background", None, None)),
                Err(DELETE_RUNNING_REFUSAL),
            ),
            // KIND drift falls to the background arm, never to the permissive
            // `None` arm: it keeps that arm's conservative matrix, so an
            // unreadable kind with an unreadable qualifier still refuses.
            (
                "a drifted kind with no qualifier",
                Some(agent("", None, None)),
                Err(DELETE_RUNNING_REFUSAL),
            ),
        ];

        for (case, reported, expected) in cases {
            let verdict = can_delete(reported.as_ref());
            match expected {
                Ok(()) => assert!(
                    verdict.is_ok(),
                    "{case}: nothing is writing this transcript, so it must be deletable \
                     (got {verdict:?})"
                ),
                Err(message) => assert_eq!(
                    verdict.as_ref().err().map(String::as_str),
                    Some(message),
                    "{case}: must refuse with this exact message"
                ),
            }
        }
    }

    /// The two refusals must stay DISTINCT and non-empty: they are the whole
    /// user-facing difference between "close your claude window" and "an agent is
    /// still running", and a single-target delete shows one of them verbatim.
    #[test]
    fn the_two_refusals_are_distinct_user_facing_messages() {
        assert_ne!(
            DELETE_INTERACTIVE_REFUSAL, DELETE_RUNNING_REFUSAL,
            "an open window and a running agent need different next moves"
        );
        assert!(!DELETE_INTERACTIVE_REFUSAL.is_empty());
        assert!(!DELETE_RUNNING_REFUSAL.is_empty());
        // The running refusal covers Working, Idle AND Other, so it must not
        // claim the agent is *working* — only that claude still reports it.
        assert!(
            !DELETE_RUNNING_REFUSAL.contains("is working"),
            "the copy must not assert work for the idle/unknown arms it also covers"
        );
        // It is ALSO the message for a record whose `kind` drifted or arrived
        // empty (the "a drifted kind with no qualifier" row above), so naming a
        // kind would assert a shape the record never stated. Pinned as a
        // property of the copy because that arm is invisible from the message
        // itself: a reader only sees it refuse.
        assert!(
            can_delete(Some(&agent("", None, None)))
                .err()
                .is_some_and(|refusal| refusal == DELETE_RUNNING_REFUSAL),
            "an unstated kind lands on this very message"
        );
        assert!(
            !DELETE_RUNNING_REFUSAL.contains("background"),
            "the copy must not assert a KIND for the drifted/empty-kind arm it also covers"
        );
    }

    /// SNAPBACK'S OWN in-flight quick reply is a writer too, and it is the one
    /// `can_delete` structurally CANNOT see.
    ///
    /// The regression this pins is a race between two features, not a bad arm:
    /// before running `claude -p -r <id>`, `send::run_send` first `claude stop`s
    /// the held job so `-p -r` is accepted. For the whole span of that send the
    /// target is therefore ABSENT from claude's active list — `can_delete` sees
    /// `None` and reads it as "nothing is holding the file open" — while a child
    /// snapback spawned appends to that very transcript. The send runs detached
    /// and no key is blocked meanwhile, so `Ctrl-X d` is genuinely reachable in
    /// that window.
    ///
    /// So the composed guard must refuse on the in-flight fact ALONE, with the
    /// claude-side verdict at its most permissive (`None`), and must say so in its
    /// own words: pointing the user at a claude window would name the wrong writer.
    #[test]
    fn can_delete_target_refuses_snapbacks_own_in_flight_reply_the_probe_cannot_see() {
        // The exact shape of the hazard: claude reports NOTHING (the send
        // deregistered the job on its way in), yet a reply is still landing.
        assert_eq!(
            can_delete_target(None, true).err().as_deref(),
            Some(DELETE_SENDING_REFUSAL),
            "an in-flight reply must refuse even though claude's list is empty"
        );
        // ...and `can_delete` alone would have ALLOWED that very same record,
        // which is precisely why the second fact has to be consulted.
        assert_eq!(
            can_delete(None),
            Ok(()),
            "the claude-side guard is permissive here by construction"
        );

        // With nothing in flight the composed guard is exactly `can_delete`,
        // adding no refusals of its own.
        assert_eq!(can_delete_target(None, false), Ok(()));
        assert_eq!(
            can_delete_target(Some(&agent("background", Some("blocked"), None)), false),
            Ok(())
        );

        // The in-flight fact is checked FIRST: it must not be masked by, nor mask
        // the wording of, a claude-side refusal that also applies.
        assert_eq!(
            can_delete_target(Some(&agent("interactive", None, None)), true)
                .err()
                .as_deref(),
            Some(DELETE_SENDING_REFUSAL),
            "the more specific writer owns the message"
        );
        assert_eq!(
            can_delete_target(Some(&agent("background", Some("working"), None)), false)
                .err()
                .as_deref(),
            Some(DELETE_RUNNING_REFUSAL),
            "without a send in flight the claude-side verdict still stands"
        );

        // A THIRD distinct message: the remedy is to wait, not to close a window
        // or stop an agent, so it may not be confused with either.
        assert_ne!(DELETE_SENDING_REFUSAL, DELETE_INTERACTIVE_REFUSAL);
        assert_ne!(DELETE_SENDING_REFUSAL, DELETE_RUNNING_REFUSAL);
        assert!(
            !DELETE_SENDING_REFUSAL.contains("claude has"),
            "the writer is snapback's own child; do not point at a claude window"
        );
    }

    // --- status_for_delete -------------------------------------------------

    /// The board copy for a finished pass: a single target speaks its reason
    /// verbatim, a lineage reports the SPLIT, and a refusal is never conflated
    /// with an FS failure.
    #[test]
    fn status_for_delete_reports_a_reason_alone_and_a_lineage_as_a_split() {
        let refusal = vec![DELETE_RUNNING_REFUSAL.to_string()];
        let error = vec!["permission denied".to_string()];

        // One target, deleted cleanly -> the row leaving the board is the message.
        assert_eq!(status_for_delete(1, 1, &[], &[]), None);
        // One target, refused -> the refusal verbatim, not a tally.
        assert_eq!(
            status_for_delete(1, 0, &refusal, &[]).as_deref(),
            Some(DELETE_RUNNING_REFUSAL)
        );
        // One target, FS failure -> that error verbatim.
        assert_eq!(
            status_for_delete(1, 0, &[], &error).as_deref(),
            Some("permission denied")
        );

        // A lineage always reports what it did, even when it all went.
        assert_eq!(
            status_for_delete(4, 4, &[], &[]).as_deref(),
            Some("4 deleted")
        );
        // A MIXED lineage reports the split.
        assert_eq!(
            status_for_delete(4, 3, &refusal, &[]).as_deref(),
            Some("3 deleted, 1 skipped (running)")
        );
        // An FS failure is counted SEPARATELY: reporting it as a skipped running
        // agent would be a false claim about why the file is still there.
        assert_eq!(
            status_for_delete(4, 3, &[], &error).as_deref(),
            Some("3 deleted, 1 failed to remove")
        );
        assert_eq!(
            status_for_delete(4, 2, &refusal, &error).as_deref(),
            Some("2 deleted, 1 skipped (running), 1 failed to remove")
        );
    }

    /// A target that was already GONE from the board is ACCOUNTED FOR rather than
    /// silently dropped: `targets` is reconciled against what actually happened,
    /// so every id handed in is either removed, refused, failed, or reported gone.
    ///
    /// This is a real state, not a defensive branch — the confirm captures its ids
    /// when the modal OPENS and a reload can drop a member while it sits there.
    /// Without the reconciliation a 3-member lineage reported "2 deleted" with the
    /// third neither removed, refused, nor mentioned anywhere.
    #[test]
    fn status_for_delete_accounts_for_a_target_that_left_the_board() {
        let refusal = vec![DELETE_RUNNING_REFUSAL.to_string()];
        let error = vec!["permission denied".to_string()];

        // The reported case: three targets, two removed, one gone.
        assert_eq!(
            status_for_delete(3, 2, &[], &[]).as_deref(),
            Some("2 deleted, 1 already gone")
        );
        // It composes with the other buckets and stays a SEPARATE count — a row
        // that had already left is neither a running skip nor an FS failure.
        assert_eq!(
            status_for_delete(4, 1, &refusal, &error).as_deref(),
            Some("1 deleted, 1 skipped (running), 1 failed to remove, 1 already gone")
        );
        // A SINGLE target that vanished says so. Silence would read as a delete:
        // no row left the board on our account, and the pass touched no bytes.
        assert_eq!(
            status_for_delete(1, 0, &[], &[]).as_deref(),
            Some(DELETE_TARGET_GONE)
        );
        // A fully accounted pass grows no phantom tail, in either shape.
        assert_eq!(
            status_for_delete(3, 3, &[], &[]).as_deref(),
            Some("3 deleted")
        );
        assert_eq!(status_for_delete(1, 1, &[], &[]), None);
        // Nothing asked for, nothing to say — and no underflow when the buckets
        // somehow outnumber the targets.
        assert_eq!(status_for_delete(0, 0, &[], &[]), None);
        assert_eq!(
            status_for_delete(2, 3, &[], &[]).as_deref(),
            Some("3 deleted")
        );
    }

    // --- toggle_hidden (Task 2.2) -----------------------------------------

    #[test]
    fn toggle_hidden_flips_a_whole_group_pivoting_on_the_selected_id() {
        let mut ids: HashSet<String> = HashSet::new();
        let members = vec!["s1".to_string(), "s2".to_string(), "s3".to_string()];

        // Pivot s1 is visible → the first toggle hides the WHOLE lineage.
        assert!(
            toggle_hidden(&mut ids, &members, "s1"),
            "hiding a visible pivot reports the new state as hidden = true"
        );
        assert!(
            members.iter().all(|m| ids.contains(m)),
            "every lineage member is hidden together, not just the pivot"
        );

        // Pivot s1 is now hidden → the second toggle exposes the whole lineage.
        assert!(
            !toggle_hidden(&mut ids, &members, "s1"),
            "exposing a hidden pivot reports the new state as hidden = false"
        );
        assert!(
            members.iter().all(|m| !ids.contains(m)),
            "every lineage member is exposed together"
        );

        // A singleton (members = [pivot]) still round-trips.
        let solo = vec!["only".to_string()];
        assert!(toggle_hidden(&mut ids, &solo, "only"));
        assert!(ids.contains("only"));
        assert!(!toggle_hidden(&mut ids, &solo, "only"));
        assert!(!ids.contains("only"));

        // A PARTIALLY-hidden group resolves uniformly to the pivot's opposite:
        // s2 pre-hidden, visible pivot s1 → hide all (both end hidden).
        let mut mixed: HashSet<String> = HashSet::from(["s2".to_string()]);
        let group = vec!["s1".to_string(), "s2".to_string()];
        assert!(toggle_hidden(&mut mixed, &group, "s1"));
        assert!(mixed.contains("s1") && mixed.contains("s2"));
    }

    // --- remove (Tasks 2.3 + 2.4) -----------------------------------------

    #[test]
    fn remove_deletes_the_transcript_and_its_sibling_id_dir() {
        let base = unique_temp_dir("with-dir");
        // Lay out `<encoded-cwd>/<id>.jsonl` alongside the sibling
        // `<id>/subagents/agent-*.jsonl` that hard delete must also clear.
        let project = base.join("-Users-me-project");
        let id = "sess-remove-1";
        let file = project.join(format!("{id}.jsonl"));
        let subagents = project.join(id).join("subagents");
        std::fs::create_dir_all(&subagents).expect("create the subagents fixture dir");
        std::fs::write(&file, "{}\n").expect("write the transcript file");
        std::fs::write(subagents.join("agent-1.jsonl"), "{}\n")
            .expect("write a subagent transcript");

        let id_dir = project.join(id);
        assert!(
            file.is_file() && id_dir.is_dir(),
            "the fixture is laid out before removal"
        );

        remove(&session_at(file.clone())).expect("remove unlinks the file and the id dir");

        assert!(!file.exists(), "the transcript file is gone");
        assert!(
            !id_dir.exists(),
            "the sibling <id>/ dir (subagents included) is gone"
        );
        assert!(
            project.is_dir(),
            "removal targets only this id's paths, never the encoded-cwd dir"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn remove_deletes_only_the_file_when_there_is_no_sibling_dir() {
        let base = unique_temp_dir("no-dir");
        let project = base.join("-Users-me-solo");
        std::fs::create_dir_all(&project).expect("create the project dir");
        let id = "sess-solo-1";
        let file = project.join(format!("{id}.jsonl"));
        std::fs::write(&file, "{}\n").expect("write the transcript file");
        // No `<id>/` sibling dir exists; remove must tolerate its absence.

        remove(&session_at(file.clone())).expect("remove tolerates a missing sibling dir");

        assert!(!file.exists(), "the transcript file is gone");
        assert!(project.is_dir(), "the project dir is left untouched");

        let _ = std::fs::remove_dir_all(&base);
    }
}
