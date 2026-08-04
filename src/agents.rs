//! Agent detection via `claude agents --json`, in TWO distinct readings.
//!
//! `claude -r <id>` REFUSES to plain-resume a session that is currently running
//! as a background/interactive agent ("Session <id> is currently running as a
//! background agent (bg). Use `claude agents` to find and attach to it, or add
//! --fork-session to branch off a copy."). `claude agents --json` prints a JSON
//! ARRAY of agents and exits without needing a TTY ("for scripting; does not
//! require a TTY"), which is the only machine-readable window onto that decision.
//!
//! The two readings answer different questions and MUST NOT be conflated:
//!
//! * **[`reported_agents`] — `--json --all` — the BOARD's signal.** Polled ~5s
//!   off-thread while the board is active (skipped entirely once it has been
//!   idle past `AGENTS_IDLE_AFTER`); drives badges, colors, the pulse and the
//!   preview banner via [`classify`]. The bare command lists currently-active
//!   agents AND recently finished ones (claude keeps a `done` background job in
//!   the active list for a while before reaping it), so a just-wrapped-up
//!   session is briefly observable without `--all`. What `--all` adds is the
//!   FULL history — every `done` agent, including those already reaped from
//!   the active list — so a finished session keeps its badge instead of
//!   vanishing the moment claude drops it.
//! * **[`live_agents`] — `--json`, NO `--all` — the HAND-OFF's signal.** A
//!   one-shot probe at hand-off; MEMBERSHIP in what it returns is liveness,
//!   structurally, because the bare command IS claude's active list. It returns
//!   the RECORDS, so the same authoritative read also carries each live agent's
//!   attach job [`id`](ReportedAgent::id) — and the `kind` + qualifier that
//!   [`classify`]'s ONE non-display consumer reads, the hard-delete writer guard
//!   ([`crate::delete::can_delete`]). That guard judges a target off THIS probe,
//!   never off the polled `--all` map.
//!
//! The durable finding behind the split: **`--all`'s `state: "done"` means "the
//! agent reported completion", NOT "claude will permit `-r`".** The two can
//! disagree transiently — an agent can report `done` while claude still holds the
//! session, and a session can go live between two polls — and **claude is the
//! only authority** on its own refusal. So the resume gate probes claude's active
//! list AT HAND-OFF rather than inferring liveness from a polled snapshot that is
//! up to ~5.3s stale while active, and unboundedly stale while idle past
//! `AGENTS_IDLE_AFTER`. Inferring `state != "done"` ⇒ live agrees in steady
//! state but is a GUESS about claude's gate; membership in the bare list is not.
//!
//! Both readings parse FAIL-SOFT: a missing binary, a non-zero exit, non-JSON
//! output, or schema drift all collapse to an EMPTY set — never a panic — so the
//! board degrades to plain behavior when the signal is unavailable. Note the two
//! DEGRADE IN OPPOSITE DIRECTIONS, deliberately; see [`live_agents`].
//!
//! The split is drawn at the QUESTION, not at the caller: **every hand-off
//! re-asks claude, and nothing hands off on polled data.** The `--all` map is a
//! display snapshot and may never decide anything — not liveness, and not the
//! attach target either. Both were once read from it; both are now read from
//! [`live_agents`].
//!
//! [`reported_agents`] MUST only run OFF the UI thread (see
//! [`crate::watch::EventLoop::spawn_agents_poller`]); [`live_agents`] is a
//! one-shot at hand-off (see its own note). The pure parser
//! ([`parse_agents_json`]) and both argv builders are unit tested without
//! spawning anything.

use std::collections::HashMap;
use std::process::Command;

use serde_json::Value;

/// The `kind` value for an agent claude runs in the BACKGROUND — the job shape
/// that carries an attachable [`id`](ReportedAgent::id) and a `state`.
const KIND_BACKGROUND: &str = "background";

/// The `kind` value for a session claude holds open INTERACTIVELY: a window
/// someone is typing in.
///
/// Public because it is a WRITER-presence signal, not merely a badge label:
/// [`crate::delete::can_delete`] refuses a HARD delete on this kind, since the
/// next keystroke in that window appends to the very transcript being unlinked.
/// Declared here, beside the qualifier vocabulary, so the one module that names
/// claude's wire tokens stays the only one — the guard must never re-spell it.
pub const KIND_INTERACTIVE: &str = "interactive";

/// The `state`/`status` value reported for an agent that has STOPPED and is
/// waiting on the user to answer. Worth translating: read bare, "blocked"
/// sounds like a fault the agent hit rather than a prompt waiting for you. By
/// far the most common `state` in practice (see DOMAIN.md's sampled
/// distribution).
const QUALIFIER_BLOCKED: &str = "blocked";

/// The `state`/`status` value for an agent that has stopped and is waiting on
/// the user. Buckets with [`QUALIFIER_BLOCKED`] because it means the same thing
/// to the user — the session wants them — and therefore reads STEADY: a pulsing
/// dot here would claim work is in flight while the agent sits doing nothing.
const QUALIFIER_WAITING: &str = "waiting";

/// The `state`/`status` value for a live agent that is up but not working a
/// turn right now.
const QUALIFIER_IDLE: &str = "idle";

/// The `state`/`status` value for an agent actively working a turn.
const QUALIFIER_WORKING: &str = "working";

/// The `state`/`status` value for an agent actively working a turn — the other
/// spelling of [`QUALIFIER_WORKING`], reported by a different code path in
/// claude. Same bucket: the two are one concept under two tokens, and splitting
/// them would give the same agent a different badge depending on which token the
/// wire happened to use.
const QUALIFIER_BUSY: &str = "busy";

/// The `state`/`status` value for an agent that has REPORTED completion.
///
/// A just-finished agent is briefly visible in the bare `claude agents --json`
/// too (claude keeps a `done` background job in the active list for a while before
/// reaping it), but `--all` is what keeps EVERY `done` agent observable — including
/// those already reaped — so the board does not lose a finished session's badge the
/// moment claude drops it. That is why [`agents_argv`] passes `--all`.
///
/// It means the agent SAID it finished — NOT that claude will permit `-r`. Those
/// two can disagree transiently, and claude is the authority on its own refusal,
/// so this value colors a BADGE and never gates a hand-off: the gates ask
/// [`live_agents`] instead. That membership is what matters — a just-`done`
/// background job is STILL a registered agent (until reaped), so `claude -p -r`
/// refuses it outright, exactly like a working one. The quick-reply gate
/// ([`crate::send::reply_gate`]) therefore does not send straight into it either:
/// it first `claude stop`s the job — safe precisely BECAUSE the run is over — and
/// only then replies in place.
const QUALIFIER_DONE: &str = "done";

/// The `state`/`status` value for a background agent claude has STOPPED — its run
/// is over.
///
/// A TERMINAL state, so it buckets as [`AgentActivity::Ended`]: it badges STEADY
/// (nothing is in flight to animate) and is deliberately NOT green ([`QUALIFIER_DONE`]'s
/// "finished cleanly" reading), since a stopped job ended without that clean-finish
/// connotation. Recognizing it — rather than letting it fall through to the fail-soft
/// [`AgentActivity::Other`], which pulses — is the whole point of this bucket: a dead
/// job must not read as live. The raw token still passes through VERBATIM so the row
/// shows what claude reported.
///
/// Distinct from the OTHER steady background bucket,
/// [`AgentActivity::WorkingButIdle`]: this one is claude REPORTING a terminal token
/// outright, while that one is inferred from a `state`/`status` contradiction claude
/// never reconciled. Different causes, different colors, both steady.
const QUALIFIER_STOPPED: &str = "stopped";

/// The `state`/`status` value for a background agent whose run FAILED.
///
/// Buckets with [`QUALIFIER_STOPPED`] as [`AgentActivity::Ended`] for the same
/// reason — the job has ENDED, so a pulse would falsely claim work is in flight —
/// and its raw token likewise passes through verbatim so the user sees claude's own
/// word (`failed`) rather than a relabel.
const QUALIFIER_FAILED: &str = "failed";

/// User-facing copy for [`AgentActivity::NeedsInput`] — the FIRST of the two
/// translated buckets (the other is [`INTERRUPTED_COPY`]). Phrased as what the
/// SESSION wants ("needs input"), so BOTH the preview banner
/// ([`friendly_status`]) AND the board list row
/// ([`crate::tui::view::render_list`], via [`qualifier_copy`]) tell the user why
/// it stopped instead of restating the raw token ([`QUALIFIER_BLOCKED`] or
/// [`QUALIFIER_WAITING`]).
const NEEDS_INPUT_COPY: &str = "needs input";

/// User-facing copy for [`AgentActivity::WorkingButIdle`] — the SECOND
/// translated bucket, and the only one with no single claude token behind it:
/// there is no `interrupted` qualifier on the wire, so this names the CONTRADICTION
/// snapback detected (a working `state` its own `status` calls `idle`) rather
/// than restating either half of the pair (which would read as a bare, endless
/// `working`).
///
/// The word is the product owner's, chosen to match claude's own vocabulary —
/// "interrupted" is the term claude uses for a background agent that was stopped,
/// so it is the phrasing users already recognize. It deliberately reads as a
/// CAUSE the raw signal cannot prove; the accepted false-positive risk (a healthy
/// agent briefly at `working`/`idle`) is documented in DOMAIN.md, and the
/// internal enum name stays descriptive ([`AgentActivity::WorkingButIdle`]) so
/// the code never asserts that cause.
const INTERRUPTED_COPY: &str = "interrupted";

/// The slice of a REPORTED agent the board UI needs, joined to a session by the
/// full `sessionId`.
///
/// "Reported", not "live": under `--all` this also carries agents that reported
/// completion (see [`QUALIFIER_DONE`]), so holding one of these says claude told
/// us about the session, NOT that it is running now. Ask [`live_agents`]
/// for that.
///
/// Kept intentionally small and read all-optional: only `kind` is required to
/// render a badge, and every field is pulled out fail-soft so schema drift in
/// `claude agents --json` never discards the whole record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportedAgent {
    /// `kind` field: `"background"` | `"interactive"` (rendered `bg` / `live`).
    pub kind: String,
    /// `id` field: the agent-view JOB id — the SHORT id (`claude agents --json`'s
    /// own `id`, e.g. `ca56b543`), NOT the full `sessionId`. This is what
    /// `claude attach <id>` matches. Only BACKGROUND agents carry it; an
    /// INTERACTIVE session has no attachable job, so `id` is `None` there. Read
    /// fail-soft like every other optional field.
    ///
    /// **Read from [`live_agents`]' records ONLY — never from
    /// [`reported_agents`]'.** Both readings parse into this same struct, so the
    /// field exists on both; but an `id` off the `--all` map is a ~5.3s-stale
    /// (unboundedly stale while idle past `AGENTS_IDLE_AFTER`) snapshot of a
    /// job that may have ended, and spawning `claude attach` with it
    /// is an authoritative decision made from stale data — the same bug shape as
    /// gating liveness on that map. The attach target comes from the probe that
    /// also confirmed the agent is live, in one read.
    pub id: Option<String>,
    /// `state` field (e.g. `"blocked"` for a background agent), if present.
    pub state: Option<String>,
    /// `status` field (e.g. `"idle"`), if present.
    pub status: Option<String>,
    /// `name` field, if present.
    pub name: Option<String>,
}

impl ReportedAgent {
    /// Compact kind label for the badge: `bg` for a background agent, `live` for
    /// an interactive one, else the raw kind (so schema drift still shows
    /// *something* rather than nothing).
    #[must_use]
    pub fn kind_label(&self) -> &str {
        match self.kind.as_str() {
            KIND_BACKGROUND => "bg",
            KIND_INTERACTIVE => "live",
            other => other,
        }
    }

    /// A short dim qualifier from `state` (preferred) or `status`, if any — e.g.
    /// `blocked` / `idle` — shown after the kind label.
    #[must_use]
    pub fn qualifier(&self) -> Option<&str> {
        self.state.as_deref().or(self.status.as_deref())
    }
}

/// What a reported agent is doing, bucketed from its `state`/`status` qualifier.
///
/// This enum is the SINGLE interpretation of that undocumented value set: the
/// preview banner ([`friendly_status`]), the list-badge pulse ([`is_active`]),
/// the badge color ([`crate::tui::view::badge_color`]) and the hard-delete
/// writer guard ([`crate::delete::can_delete`]) all map from it, so a schema
/// drift is a one-line change in [`classify`] rather than a hunt for raw string
/// matches scattered across the UI.
///
/// **The boundary, stated precisely — the buckets are no longer display-only:**
///
/// * No bucket answers **"live?"**. Liveness is never inferred from a qualifier;
///   it is [`live_agents`]' MEMBERSHIP answer, straight from claude.
/// * No bucket gates **RESUME or ATTACH**. Those ride that same membership, and
///   nothing here may widen them.
/// * The buckets DO decide non-display questions, in exactly two places. Both
///   classify a RECORD THE PROBE JUST RETURNED, so liveness is still membership
///   and only the "what is it doing" question is bucketed:
///   * **"is a WRITER present?"** — [`crate::delete::can_delete`], which reads
///     the bucket before a HARD delete unlinks the transcript. That unlink is
///     IRREVERSIBLE, so retuning a bucket is no longer only a repaint.
///   * **"how should a stop be routed?"** — the `Ctrl-R` / `Ctrl-K` gates
///     ([`crate::send::reply_gate`], [`crate::send::interrupt_gate`]).
///
/// [`AgentActivity::WorkingButIdle`] is the sharp edge, and the two consumers
/// deliberately read it DIFFERENTLY. In the send gates it is granted no action of
/// its own — it rides with the LIVE states, so nothing acts on a guess. In the
/// writer guard it is an ALLOW arm, because the question there is narrower ("is a
/// write in flight?") and claude appends by re-opening the path. So widening this
/// bucket does not merely steady a dot: it makes transcripts deletable that were
/// refused before. Judge any retune here against BOTH matrices as well as the
/// badge.
///
/// Payload-free on purpose — a variant answers "which bucket", never "what did
/// the wire say". The raw qualifier stays available on the [`ReportedAgent`]
/// itself (via [`ReportedAgent::qualifier`]) for anything that must show it
/// VERBATIM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentActivity {
    /// Stopped, waiting on the user ([`QUALIFIER_BLOCKED`] /
    /// [`QUALIFIER_WAITING`]).
    NeedsInput,
    /// Live but not working a turn ([`QUALIFIER_IDLE`]).
    Idle,
    /// Actively working a turn ([`QUALIFIER_WORKING`] / [`QUALIFIER_BUSY`]).
    Working,
    /// claude reports a working `state` its own `status` contradicts as `idle`;
    /// the single bucket snapback authors rather than passes through verbatim.
    /// It gates no resume and no attach — but it IS an ALLOW arm of
    /// [`crate::delete::can_delete`]'s writer guard, so retuning it moves an
    /// irreversible unlink and not just a dot (see [`AgentActivity`]).
    ///
    /// The internal name stays DESCRIPTIVE — it names exactly what was observed
    /// (a working `state` contradicted by an `idle` `status`) and asserts no
    /// cause, unlike the user-facing word. It renders gray + STEADY (see
    /// [`is_active`]): the pulse would claim work is in flight while claude's own
    /// `status` says nothing is churning. Keyed on `state`, so it fires only for
    /// background agents (interactive sessions report no `state`).
    WorkingButIdle,
    /// Reported completion ([`QUALIFIER_DONE`]). Badges green and steady, and it
    /// does NOT decide whether `-r` is permitted.
    ///
    /// Not display-only either: it is a LIVE ALLOW arm of
    /// [`crate::delete::can_delete`]'s writer guard, because the BARE list this
    /// module's docs describe holds a `done` job until claude reaps it. So
    /// retuning what lands in this bucket moves an IRREVERSIBLE unlink and not
    /// just a dot, exactly as it does for [`AgentActivity::WorkingButIdle`]; that
    /// guard's doc comment owns why the arm once read as unreachable and is not.
    Done,
    /// A TERMINAL background state — the job has ENDED, whether cleanly stopped or
    /// failed ([`QUALIFIER_STOPPED`] / [`QUALIFIER_FAILED`]). Badges STEADY
    /// (nothing is in flight to animate) and DIM/neutral rather than green, and its
    /// raw token passes through VERBATIM so the row still reads `stopped` / `failed`.
    ///
    /// Distinct from [`Done`](Self::Done) (which reports a CLEAN completion and
    /// badges green) and from [`Other`](Self::Other) (the fail-soft default that
    /// counts as ACTIVE): these two tokens are KNOWN terminals, so recognizing them
    /// is exactly what stops a dead job from pulsing as if it were still live.
    ///
    /// Also distinct from [`WorkingButIdle`](Self::WorkingButIdle), the OTHER
    /// steady background bucket: this one is claude REPORTING a terminal token
    /// outright, that one is INFERRED from a contradiction claude never reconciled.
    /// Both rest steady; they differ in cause and in color (dim here, working gray
    /// there), so neither replaces the other.
    Ended,
    /// An unrecognized qualifier, or none at all. FAIL-SOFT: never dropped and
    /// never guessed at — callers pass the raw value through untouched.
    Other,
}

/// Bucket a reported agent's qualifier into an [`AgentActivity`].
///
/// Pure. Reuses [`ReportedAgent::qualifier`]'s `state`-then-`status` precedence
/// rather than re-deriving it, so both seams can never disagree about which
/// field won. Anything outside the known values — including a missing qualifier
/// — is [`AgentActivity::Other`], matching the module's fail-soft posture toward
/// `claude agents --json` schema drift.
///
/// The ONE exception reads the raw `state`/`status` PAIR rather than the
/// collapsed qualifier: [`AgentActivity::WorkingButIdle`] fires when a working
/// `state` is contradicted by an `idle` `status`, and it is checked BEFORE the
/// qualifier match — which would otherwise collapse the pair to `state=working`
/// and hide the contradiction as a plain [`AgentActivity::Working`].
/// [`ReportedAgent::qualifier`] itself is left untouched, so every other seam
/// still speaks through the same collapsed precedence.
///
/// That early return cannot shadow the other resting-terminal bucket,
/// [`AgentActivity::Ended`]: the two are DISJOINT by construction. The
/// contradiction check requires `state` to be [`QUALIFIER_WORKING`] /
/// [`QUALIFIER_BUSY`], while [`AgentActivity::Ended`]'s tokens
/// ([`QUALIFIER_STOPPED`] / [`QUALIFIER_FAILED`]) can never be either — so a
/// `stopped` job whose `status` also reads `idle` still buckets as `Ended`, and
/// both buckets stay reachable regardless of arm order.
#[must_use]
pub fn classify(agent: &ReportedAgent) -> AgentActivity {
    // Surface claude's own self-contradiction before the qualifier collapses it:
    // a background agent that died at startup keeps reporting a working `state`
    // while its `status` reads `idle`. It reaches no liveness, resume or attach
    // decision — but it DOES reach the hard-delete writer guard, which ALLOWS on
    // this bucket (see `WorkingButIdle`), so widening this branch widens an
    // irreversible unlink.
    if matches!(
        agent.state.as_deref(),
        Some(QUALIFIER_WORKING | QUALIFIER_BUSY)
    ) && agent.status.as_deref() == Some(QUALIFIER_IDLE)
    {
        return AgentActivity::WorkingButIdle;
    }
    match agent.qualifier() {
        Some(QUALIFIER_BLOCKED | QUALIFIER_WAITING) => AgentActivity::NeedsInput,
        Some(QUALIFIER_IDLE) => AgentActivity::Idle,
        Some(QUALIFIER_WORKING | QUALIFIER_BUSY) => AgentActivity::Working,
        Some(QUALIFIER_DONE) => AgentActivity::Done,
        Some(QUALIFIER_STOPPED | QUALIFIER_FAILED) => AgentActivity::Ended,
        _ => AgentActivity::Other,
    }
}

/// The display PHRASE for a reported agent's qualifier — the shared translation
/// both consumers speak, so the undocumented `state`/`status` value set is turned
/// into user-facing copy in exactly ONE place.
///
/// `None` when the agent has no qualifier at all (nothing to say). Otherwise it
/// translates the TWO authored buckets and passes every other bucket's raw
/// qualifier through VERBATIM (fail-soft: an unknown value is shown, never hidden
/// or relabeled):
///
/// * [`AgentActivity::NeedsInput`] — BOTH of its spellings ([`QUALIFIER_BLOCKED`]
///   and [`QUALIFIER_WAITING`]) collapse to [`NEEDS_INPUT_COPY`], since the BUCKET
///   is the thing being named and two spellings of "it wants you" must not read as
///   two different states.
/// * [`AgentActivity::WorkingButIdle`] → [`INTERRUPTED_COPY`], because that bucket
///   has no single wire token to pass through — it is the CONTRADICTION between a
///   working `state` and an `idle` `status`, which read verbatim would be a bare,
///   endless `working`.
///
/// Its two consumers are the preview banner ([`friendly_status`], which fuses the
/// phrase onto the kind label) and the board list row
/// ([`crate::tui::view::render_list`], which draws the phrase as its own span so
/// it can weight [`AgentActivity::NeedsInput`] louder than the rest). Both read
/// the SAME phrase here, so they can never disagree about what a qualifier says.
///
/// Reuses [`classify`] and [`ReportedAgent::qualifier`] rather than re-matching
/// the raw tokens, so this seam can never drift from the badge color or the pulse
/// about which field won or what bucket it meant.
#[must_use]
pub fn qualifier_copy(agent: &ReportedAgent) -> Option<&str> {
    let qualifier = agent.qualifier()?;
    Some(match classify(agent) {
        AgentActivity::NeedsInput => NEEDS_INPUT_COPY,
        // The one bucket with no wire token to pass through: it names the
        // state/status CONTRADICTION, not either half of it (see `INTERRUPTED_COPY`).
        AgentActivity::WorkingButIdle => INTERRUPTED_COPY,
        AgentActivity::Idle
        | AgentActivity::Working
        | AgentActivity::Done
        | AgentActivity::Ended
        | AgentActivity::Other => qualifier,
    })
}

/// A one-line, human-readable status for a reported agent: the kind label (`bg`
/// / `live`) plus its qualifier — e.g. `bg needs input`, `live working`.
///
/// Pure, and the phrase comes from [`qualifier_copy`] so the value set is
/// interpreted in exactly one place (shared with the board list row). An agent
/// with no qualifier at all renders the kind label alone.
#[must_use]
pub fn friendly_status(agent: &ReportedAgent) -> String {
    let label = agent.kind_label();
    let Some(phrase) = qualifier_copy(agent) else {
        return label.to_string(); // Nothing to qualify -> the label stands alone.
    };
    format!("{label} {phrase}")
}

/// Whether a reported agent is actively WORKING (versus waiting or finished),
/// deciding if its list-badge dot pulses.
///
/// Pure, and derived from [`classify`] so "active" can never disagree with the
/// banner about what the qualifier meant. The RESTING buckets are steady:
/// [`AgentActivity::NeedsInput`] (stopped, waiting on the user),
/// [`AgentActivity::Idle`] (up, but not working a turn),
/// [`AgentActivity::WorkingButIdle`] (claude's own `status` says nothing is
/// churning, so a pulse would claim work that is not in flight),
/// [`AgentActivity::Done`] (finished cleanly), and [`AgentActivity::Ended`]
/// (stopped or failed — the job is over) — in none of these is anything happening
/// to animate.
///
/// [`AgentActivity::Other`] — an unknown or absent qualifier — counts as ACTIVE
/// on purpose: schema drift fails toward showing activity rather than silently
/// hiding a busy session behind a steady dot. That fail-soft default is PRESERVED
/// for genuinely-unknown tokens; [`AgentActivity::Ended`] only removes the two
/// KNOWN terminal tokens (`stopped` / `failed`) from it, so a truly dead job no
/// longer pulses while real drift still does.
#[must_use]
pub fn is_active(agent: &ReportedAgent) -> bool {
    match classify(agent) {
        AgentActivity::NeedsInput
        | AgentActivity::Idle
        | AgentActivity::WorkingButIdle
        | AgentActivity::Done
        | AgentActivity::Ended => false,
        AgentActivity::Working | AgentActivity::Other => true,
    }
}

/// Parse the raw stdout of `claude agents --json[ --all]` into a map keyed by
/// full `sessionId`.
///
/// Shared by BOTH readings ([`reported_agents`] and [`live_agents`]): the
/// wire shape is identical, only the flags and the question differ, so there is
/// exactly ONE parser and no second place for schema drift to be handled
/// differently.
///
/// FAIL-SOFT by construction: non-JSON or a non-array top level yields an empty
/// map; an element without a string `sessionId` is skipped; every other field is
/// read with a default/optional so an unexpected shape never discards the record
/// or panics. This is the ONLY place the wire shape is interpreted.
///
/// Last-one-wins per `sessionId`: two records sharing a session id collapse to
/// whichever came last in the array. Observed not to happen (`--all` reported no
/// duplicate ids), and deliberately not engineered around — see DOMAIN.md for
/// the accepted risk if that ever changes.
#[must_use]
pub fn parse_agents_json(raw: &str) -> HashMap<String, ReportedAgent> {
    // Named for what it holds, not for what a caller wants it to mean: under
    // `--all` this accumulates agents that reported completion too. Calling it
    // `live` would re-plant the exact assumption that made the resume gate a
    // TOCTOU race — only `live_agents`' reading may claim liveness.
    let mut reported = HashMap::new();
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return reported; // Not JSON at all -> no signal.
    };
    let Some(array) = value.as_array() else {
        return reported; // Top level is not the documented array -> no signal.
    };
    for element in array {
        let Some(session_id) = element.get("sessionId").and_then(Value::as_str) else {
            continue; // No join key -> unusable record, skip (never fatal).
        };
        let str_field = |key: &str| element.get(key).and_then(Value::as_str).map(str::to_owned);
        reported.insert(
            session_id.to_owned(),
            ReportedAgent {
                kind: str_field("kind").unwrap_or_default(),
                id: str_field("id"),
                state: str_field("state"),
                status: str_field("status"),
                name: str_field("name"),
            },
        );
    }
    reported
}

/// The argv shared by BOTH readings: the program, the subcommand, and `--json`.
///
/// Declared once so the two builders below cannot drift on the program name or
/// the subcommand — only on the ONE flag that actually distinguishes them.
/// `--json` is load-bearing for both: it selects the TTY-free, machine-readable
/// array [`parse_agents_json`] reads, and without it the output is a human table
/// this module cannot parse.
const AGENTS_ARGV: [&str; 3] = ["claude", "agents", "--json"];

/// The flag that widens `claude agents --json` from "currently active" to
/// "every agent it knows, including finished ones".
///
/// The single difference between [`agents_argv`] and [`live_agents_argv`], and
/// the entire reason those are two functions: it flips what MEMBERSHIP of the
/// result means.
const AGENTS_ALL_FLAG: &str = "--all";

/// The shared prefix as an owned argv, ready to extend.
fn agents_argv_base() -> Vec<String> {
    AGENTS_ARGV.iter().map(|part| (*part).to_string()).collect()
}

/// Build the argv of the BOARD's reported-agents poll
/// (`claude agents --json --all`); `argv[0]` is the program.
///
/// Pure and separate from the spawn for the same reason as
/// [`crate::resume::build_argv`]: the exact invocation is the contract with an
/// external CLI, so it must be assertable WITHOUT spawning `claude` (which the
/// test suite never does). [`reported_agents`] builds its `Command` from this and
/// nothing else, so the flags are pinned by `agents_argv_is_claude_agents_json_all`.
///
/// [`AGENTS_ALL_FLAG`] is what keeps the `done` bucket RELIABLY observable — the
/// bare command lists a finished job only until claude reaps it from the active
/// list, so dropping the flag would make a session's `done` badge vanish the moment
/// claude drops it. It is not a nicety; it is the flag the durable `done` badge
/// exists on.
#[must_use]
fn agents_argv() -> Vec<String> {
    let mut argv = agents_argv_base();
    argv.push(AGENTS_ALL_FLAG.to_string());
    argv
}

/// Build the argv of the GATE's liveness probe (`claude agents --json`, with NO
/// [`AGENTS_ALL_FLAG`]); `argv[0]` is the program.
///
/// The absent flag IS the contract, and it is pinned by
/// `live_agents_argv_is_claude_agents_json_without_all`. Adding `--all` here
/// would be silent and catastrophic: the result would include every FINISHED
/// agent, so membership would report those sessions as live and Enter would
/// divert each one into the Attach/Fork overlay instead of resuming it —
/// breaking the board's primary interaction for the large majority of rows,
/// while every classifier test kept passing.
#[must_use]
fn live_agents_argv() -> Vec<String> {
    agents_argv_base()
}

/// Decide what a finished agents shell-out MEANS: the parsed records when it
/// exited zero, an EMPTY map when it did not.
///
/// Pure, and split out of [`run_agents`] exactly as
/// [`crate::resume::status_for_exit`] is split out of [`crate::resume::launch`]:
/// "a non-zero exit is no signal" is a DECISION, and a decision left inside the
/// impure wrapper is only reachable by spawning `claude` — which the suite never
/// does. Taking the status as a plain `bool` rather than an `Output` is what
/// makes it assertable at all.
///
/// The status is checked BEFORE the parse, and that order is the contract: a
/// failed run's stdout is not a reading even when it happens to parse cleanly, so
/// it must never reach [`parse_agents_json`] — which remains the ONLY place the
/// wire shape is interpreted.
#[must_use]
fn agents_from_output(success: bool, stdout: &str) -> HashMap<String, ReportedAgent> {
    if !success {
        return HashMap::new(); // Non-zero exit -> treat as "no signal".
    }
    parse_agents_json(stdout)
}

/// Run an agents shell-out and parse it, or yield EMPTY on any failure (missing
/// binary, non-zero exit, unreadable / non-JSON output).
///
/// The one impure step both readings share, and it owns the SPAWN alone — what
/// the result means is [`agents_from_output`]'s pure decision. Output is CAPTURED
/// (no TTY inherited), so it never contends with an interactive `claude` on the
/// terminal. Never panics; every error path returns an empty map.
fn run_agents(argv: &[String]) -> HashMap<String, ReportedAgent> {
    let output = match Command::new(&argv[0]).args(&argv[1..]).output() {
        Ok(output) => output,
        Err(_) => return HashMap::new(), // `claude` not on PATH, spawn failed, etc.
    };
    agents_from_output(
        output.status.success(),
        &String::from_utf8_lossy(&output.stdout),
    )
}

/// Shell out to [`agents_argv`] (`claude agents --json --all`) and return the
/// REPORTED set — live or finished — for the board to render.
///
/// The returned map is a DISPLAY signal only: membership does not mean the
/// session is running, and no qualifier in it decides whether `claude -r` will be
/// permitted. Ask [`live_agents`] for that.
///
/// MUST be called off the UI thread — it spawns a child process (see
/// [`crate::watch::EventLoop::spawn_agents_poller`], which polls it ~5s while
/// active and skips the poll entirely once idle past `AGENTS_IDLE_AFTER`).
#[must_use]
pub fn reported_agents() -> HashMap<String, ReportedAgent> {
    run_agents(&agents_argv())
}

/// Probe claude's ACTIVE agent list (`claude agents --json`, no `--all`) and
/// return the agents it is holding open RIGHT NOW, keyed by full `sessionId`.
///
/// The authoritative answer to EVERY hand-off-shaped question, from ONE read:
///
/// * **"Will `claude -r <id>` refuse?"** — MEMBERSHIP, and the reason that is
///   enough is structural: the bare command IS claude's active list, so there is
///   no bucket to infer from and nothing to be uncertain about.
/// * **"What does `claude attach` take?"** — the matched record's own short
///   agent-view [`id`](ReportedAgent::id).
/// * **"Is anything WRITING this transcript?"** — the matched record's `kind`
///   and its [`classify`] bucket, read by [`crate::delete::can_delete`] before a
///   HARD delete unlinks the file. Membership alone cannot answer this one: most
///   of this list is PARKED, so the record's own fields decide.
///
/// It returns the RECORDS rather than bare ids precisely so the questions past
/// membership have an authoritative answer. The parse has the `id` in hand either
/// way; discarding it would force the attach path back onto [`reported_agents`]'
/// map — an authoritative decision made from a ~5.3s-stale (unboundedly stale
/// while idle past `AGENTS_IDLE_AFTER`) snapshot, which is the exact class of
/// bug that moving the liveness gate here fixed. One shell-out, one parse,
/// every answer, no second notion of "which agent is this".
///
/// [`reported_agents`]' `--all` map cannot answer any of them: it is up to
/// ~5.3s stale while the board is active, and unboundedly stale once idle
/// past `AGENTS_IDLE_AFTER`, and its `done` qualifier means "the agent
/// reported completion", not "claude will permit `-r`".
///
/// **FAIL-SOFT toward "not live", and that direction is deliberate.** Any failure
/// (missing binary, non-zero exit, bad JSON, schema drift) yields an EMPTY map ⇒
/// "not live" ⇒ a plain resume ⇒ **claude's own check still backstops it**, and
/// the user sees claude's real message instead of our guess. Degrading toward
/// "let claude decide" is correct, because claude is the authority.
///
/// That direction also decides the ATTACH path, where it collapses two premises
/// into one: an empty map means "the agent finished" and "we could not ask" ALIKE.
/// Both must refuse — with no authoritative `id`, `claude attach` would be handed
/// a dead or absent job — so the refusal is worded for what was OBSERVED (claude
/// did not report it) rather than for a cause the probe cannot distinguish; see
/// [`crate::resume::ATTACH_NOT_LIVE`].
///
/// This REVERSES the direction of the deleted `is_live`, which failed toward
/// "live" for drift and unknown qualifiers. That was right for a CLASSIFIER —
/// facing an unrecognized bucket, the safe assumption was "might be running". It
/// is wrong here: with membership there is no bucket to be uncertain about, and
/// the only remaining error is "we could not ask", which claude itself catches
/// one step later. Never panics.
///
/// Called ONE-SHOT at each hand-off, not on the poll cadence — see the notes at
/// its call sites in [`crate::tui::update`] (the Enter gate and the Attach
/// hand-off) for why that does not violate the off-UI-thread rule.
///
/// Its only non-doc caller is [`crate::tui::app`]'s `default_live_probe`, which
/// is `#[cfg(not(test))]`-gated so the probe default under test panics rather
/// than spawning `claude` (the suite never spawns it). That leaves this with
/// zero callers under the `lib test` target ALONE — the bin runtime path calls
/// it on every Enter — hence retained + `dead_code` allowed narrowly here
/// (rather than module- or crate-wide).
#[allow(dead_code)]
#[must_use]
pub fn live_agents() -> HashMap<String, ReportedAgent> {
    run_agents(&live_agents_argv())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic `ReportedAgent` carrying only what the classifier reads, so
    /// each test below states just the kind + qualifier source it cares about.
    fn agent(kind: &str, state: Option<&str>, status: Option<&str>) -> ReportedAgent {
        ReportedAgent {
            kind: kind.to_string(),
            id: None,
            state: state.map(str::to_owned),
            status: status.map(str::to_owned),
            name: None,
        }
    }

    /// `qualifier_copy` is the shared translation the preview banner AND the
    /// board list row both speak, so it is pinned directly here rather than only
    /// through `friendly_status`: the TWO authored buckets (`NeedsInput`, both
    /// spellings from either qualifier source; and `WorkingButIdle`) translate,
    /// every other bucket passes its raw token through verbatim, and an agent with
    /// no qualifier at all yields `None`.
    #[test]
    fn qualifier_copy_translates_the_authored_buckets_and_passes_the_rest_through() {
        // NeedsInput is translated — both spellings, from either qualifier
        // source, collapse to the same actionable copy.
        assert_eq!(
            qualifier_copy(&agent("background", Some("blocked"), None)),
            Some("needs input")
        );
        assert_eq!(
            qualifier_copy(&agent("interactive", None, Some("waiting"))),
            Some("needs input")
        );

        // WorkingButIdle is the SECOND translated bucket: its working/idle
        // contradiction has no wire token, so it reads as the product word rather
        // than either half of the pair.
        assert_eq!(
            qualifier_copy(&agent("background", Some("working"), Some("idle"))),
            Some("interrupted")
        );

        // Every other KNOWN bucket keeps its raw token verbatim.
        assert_eq!(
            qualifier_copy(&agent("background", Some("idle"), None)),
            Some("idle")
        );
        assert_eq!(
            qualifier_copy(&agent("background", Some("working"), None)),
            Some("working")
        );
        assert_eq!(
            qualifier_copy(&agent("interactive", None, Some("busy"))),
            Some("busy")
        );
        assert_eq!(
            qualifier_copy(&agent("background", Some("done"), None)),
            Some("done")
        );

        // FAIL-SOFT: schema drift passes through untouched rather than being
        // dropped or relabeled.
        assert_eq!(
            qualifier_copy(&agent("background", Some("compacting"), None)),
            Some("compacting")
        );

        // No `state` and no `status` -> nothing to translate.
        assert_eq!(qualifier_copy(&agent("background", None, None)), None);

        // Inherited state-over-status precedence: `blocked` (state) wins over
        // `idle` (status), so the copy is the NeedsInput translation, not `idle`.
        assert_eq!(
            qualifier_copy(&agent("background", Some("blocked"), Some("idle"))),
            Some("needs input")
        );
    }

    /// Task 1.3: `blocked` is the ONE translated value — it must reach the user
    /// as actionable copy, from EITHER qualifier source.
    #[test]
    fn blocked_classifies_as_needs_input_and_renders_friendly_copy() {
        let from_state = agent("background", Some("blocked"), None);
        assert_eq!(classify(&from_state), AgentActivity::NeedsInput);
        assert_eq!(friendly_status(&from_state), "bg needs input");

        // Same bucket when the value arrives via the `status` fallback.
        let from_status = agent("interactive", None, Some("blocked"));
        assert_eq!(classify(&from_status), AgentActivity::NeedsInput);
        assert_eq!(friendly_status(&from_status), "live needs input");

        // `qualifier`'s state-then-status precedence flows through `classify`:
        // this is the real shape `claude agents --json` reports for a waiting
        // background agent (state `blocked` alongside status `idle`), and the
        // state must win.
        let both = agent("background", Some("blocked"), Some("idle"));
        assert_eq!(classify(&both), AgentActivity::NeedsInput);
        assert_eq!(friendly_status(&both), "bg needs input");
    }

    /// `waiting` means the same thing to the user as `blocked` — the session
    /// wants them — so it buckets as `NeedsInput` and renders the SAME translated
    /// copy. Pinned from both qualifier sources.
    ///
    /// The steadiness of this bucket is the lie this table fixes; the pulse half
    /// is pinned in `each_bucket_maps_to_its_pulse` and, on drawn cells, in
    /// `tui::view`.
    #[test]
    fn waiting_classifies_as_needs_input_and_renders_friendly_copy() {
        let from_state = agent("background", Some("waiting"), None);
        assert_eq!(classify(&from_state), AgentActivity::NeedsInput);
        assert_eq!(friendly_status(&from_state), "bg needs input");

        let from_status = agent("interactive", None, Some("waiting"));
        assert_eq!(classify(&from_status), AgentActivity::NeedsInput);
        assert_eq!(
            friendly_status(&from_status),
            "live needs input",
            "`waiting` is a NeedsInput qualifier, so it must read as the bucket's \
             copy rather than as its own raw token"
        );
    }

    /// The KNOWN working/idle values bucket correctly yet still render their raw
    /// token — only the `NeedsInput` bucket is translated.
    #[test]
    fn idle_and_working_classify_to_their_buckets_and_render_verbatim() {
        let idle = agent("background", Some("idle"), None);
        assert_eq!(classify(&idle), AgentActivity::Idle);
        assert_eq!(friendly_status(&idle), "bg idle");

        let working = agent("background", Some("working"), None);
        assert_eq!(classify(&working), AgentActivity::Working);
        assert_eq!(friendly_status(&working), "bg working");

        // `busy` is the other spelling of the same bucket, here via the `status`
        // fallback (no `state` at all) — but it still renders its OWN token.
        let busy = agent("interactive", None, Some("busy"));
        assert_eq!(classify(&busy), AgentActivity::Working);
        assert_eq!(friendly_status(&busy), "live busy");
    }

    /// `done` is a bucket of its own: an agent that REPORTED completion, rendered
    /// verbatim and steady — there is no work left to animate. `--all` is what
    /// keeps EVERY `done` agent observable, but a just-finished one is briefly in
    /// the bare list too, until claude reaps it (see `QUALIFIER_DONE`).
    ///
    /// It says nothing about whether `claude -r` will be permitted, and nothing
    /// here asks: that is `live_agents`' answer, pinned at the gate in
    /// `tui::update`.
    #[test]
    fn done_classifies_as_done_renders_verbatim_and_is_not_active() {
        let from_state = agent("background", Some("done"), None);
        assert_eq!(classify(&from_state), AgentActivity::Done);
        assert_eq!(friendly_status(&from_state), "bg done");
        assert!(
            !is_active(&from_state),
            "a finished agent has nothing to pulse"
        );

        let from_status = agent("interactive", None, Some("done"));
        assert_eq!(classify(&from_status), AgentActivity::Done);
        assert_eq!(friendly_status(&from_status), "live done");
        assert!(!is_active(&from_status));
    }

    /// `stopped` and `failed` are TERMINAL: the job has ended, so both bucket as
    /// `Ended`, render their raw token verbatim, and are STEADY — the exact
    /// opposite of the `Other` fail-soft default that made them pulse before.
    ///
    /// This is the regression this fix pins: a dead background job must not wear a
    /// pulsing "live" badge. Both spellings are checked from either qualifier
    /// source, and the raw token is passed through untouched (never translated).
    #[test]
    fn stopped_and_failed_classify_as_ended_render_verbatim_and_are_not_active() {
        for token in ["stopped", "failed"] {
            let from_state = agent("background", Some(token), None);
            assert_eq!(
                classify(&from_state),
                AgentActivity::Ended,
                "{token:?} is a terminal state -> Ended"
            );
            assert_eq!(friendly_status(&from_state), format!("bg {token}"));
            assert!(
                !is_active(&from_state),
                "a job that has ended has nothing to pulse: {token:?}"
            );

            // Identical via the `status` fallback (no `state` at all).
            let from_status = agent("interactive", None, Some(token));
            assert_eq!(classify(&from_status), AgentActivity::Ended);
            assert_eq!(friendly_status(&from_status), format!("live {token}"));
            assert!(!is_active(&from_status));
        }
    }

    /// FAIL-SOFT: schema drift in the undocumented value set must pass through
    /// to the user untouched rather than being dropped or guessed at.
    #[test]
    fn an_unknown_qualifier_is_other_and_passes_through_verbatim() {
        let from_state = agent("background", Some("compacting"), None);
        assert_eq!(classify(&from_state), AgentActivity::Other);
        assert_eq!(friendly_status(&from_state), "bg compacting");

        let from_status = agent("interactive", None, Some("awaiting-tool"));
        assert_eq!(classify(&from_status), AgentActivity::Other);
        assert_eq!(friendly_status(&from_status), "live awaiting-tool");
    }

    /// A record with neither field: still `Other`, and the banner degrades to
    /// the kind label alone (no dangling separator, no invented status).
    #[test]
    fn a_missing_qualifier_is_other_and_renders_the_kind_label_alone() {
        let bare = agent("background", None, None);
        assert_eq!(classify(&bare), AgentActivity::Other);
        assert_eq!(friendly_status(&bare), "bg");

        // Kind drift too: an unknown kind keeps `kind_label`'s raw pass-through.
        let drifted = agent("future-kind", None, None);
        assert_eq!(friendly_status(&drifted), "future-kind");
    }

    /// Which buckets count as WORKING, over both qualifier sources. This is
    /// `is_active`'s own contract — that the resting buckets are steady — kept
    /// inline with the function it pins.
    ///
    /// The badge PAIRING (that a bucket's color and its pulse agree, so gray is
    /// only honest because it moves) is a rendering claim and is asserted where
    /// the palette lives, in `tui::view`'s
    /// `each_bucket_maps_to_its_badge_color_and_pulse`.
    #[test]
    fn each_bucket_maps_to_its_pulse() {
        // (qualifier, expected active/pulsing)
        let cases = [
            // Waiting on the user -> steady.
            (Some("blocked"), false),
            // The other "it wants you" spelling -> steady for the same reason.
            // A pulse here would claim work is in flight while nothing runs.
            (Some("waiting"), false),
            // Up but not working -> steady.
            (Some("idle"), false),
            // Quietly working -> the pulse is what marks it active.
            (Some("working"), true),
            // ...and its other spelling must pulse identically.
            (Some("busy"), true),
            // Finished -> steady: there is no work left to animate.
            (Some("done"), false),
            // Terminal states -> steady: the job has ENDED (stopped or failed),
            // so a pulse would falsely claim work is in flight. These are the
            // rows this fix adds; they pulsed as `Other` before it.
            (Some("stopped"), false),
            (Some("failed"), false),
            // FAIL-SOFT: schema drift tracks the working bucket...
            (Some("compacting"), true),
            // ...and so does a record with no qualifier at all. Neither may
            // hide activity behind a steady dot.
            (None, true),
        ];

        for (qualifier, active) in cases {
            // Sourced from `state`...
            let from_state = agent("background", qualifier, None);
            assert_eq!(
                is_active(&from_state),
                active,
                "state={qualifier:?} should have active={active}"
            );

            // ...and identically via the `status` fallback (no `state` at all),
            // so the pulse never depends on WHICH field the wire used.
            let from_status = agent("interactive", None, qualifier);
            assert_eq!(
                is_active(&from_status),
                active,
                "status={qualifier:?} should have active={active}"
            );
        }

        // The joint-read bucket can't ride the single-qualifier loop above: it
        // needs BOTH a working `state` AND an `idle` `status`. It rests STEADY —
        // a pulse would claim work claude's own `status` says is not running.
        assert!(
            !is_active(&agent("background", Some("working"), Some("idle"))),
            "working/idle is claude's stalled-agent shape; it must not pulse"
        );
    }

    /// The joint-read bucket end to end: ONLY a working `state` CONTRADICTED by
    /// an `idle` `status` is `WorkingButIdle`. It reads `interrupted` (the second
    /// translated bucket) and rests steady, while every neighbouring shape stays
    /// exactly what it was — the contradiction is the whole trigger, so dropping
    /// either half falls back to a plain `Working` or `Idle`.
    #[test]
    fn working_state_with_idle_status_is_interrupted_and_steady() {
        // The stalled-agent shape claude never reconciles: working state that its
        // own status calls idle.
        let interrupted = agent("background", Some("working"), Some("idle"));
        assert_eq!(classify(&interrupted), AgentActivity::WorkingButIdle);
        assert_eq!(friendly_status(&interrupted), "bg interrupted");
        assert!(
            !is_active(&interrupted),
            "the pulse would claim work claude's own status says is not running"
        );

        // `busy` is the other spelling of a working state, so it triggers too.
        let interrupted_busy = agent("background", Some("busy"), Some("idle"));
        assert_eq!(classify(&interrupted_busy), AgentActivity::WorkingButIdle);
        assert_eq!(friendly_status(&interrupted_busy), "bg interrupted");

        // Neither half alone is the contradiction, so each falls back to the
        // ordinary bucket it always had:
        // - a working state with NO idle status is a plain, PULSING Working...
        let working_none = agent("background", Some("working"), None);
        assert_eq!(classify(&working_none), AgentActivity::Working);
        assert!(is_active(&working_none));
        // ...including a working state whose status is some OTHER value: the
        // trigger is `idle` specifically, not merely "status present".
        let working_busy = agent("background", Some("working"), Some("busy"));
        assert_eq!(classify(&working_busy), AgentActivity::Working);
        assert!(is_active(&working_busy));
        // - ...and an idle status with NO working state is a plain, steady Idle.
        let idle_only = agent("interactive", None, Some("idle"));
        assert_eq!(classify(&idle_only), AgentActivity::Idle);
        assert_eq!(friendly_status(&idle_only), "live idle");
        assert!(!is_active(&idle_only));
    }

    /// The two RESTING background buckets are DISTINCT and both REACHABLE — the
    /// invariant that a merge of their two histories is most likely to break by
    /// dropping one, collapsing them into one, or ordering `classify`'s arms so the
    /// earlier one shadows the later.
    ///
    /// They answer different questions: `Ended` is claude REPORTING a terminal
    /// token, `WorkingButIdle` is a contradiction claude never reconciled. Both
    /// hold steady, and the fail-soft ACTIVE default must survive BOTH of them so
    /// genuine schema drift still pulses.
    #[test]
    fn ended_and_interrupted_are_distinct_resting_buckets_and_unknown_still_pulses() {
        // Distinct buckets, not two names for one.
        assert_ne!(
            AgentActivity::Ended,
            AgentActivity::WorkingButIdle,
            "the two resting buckets must not be collapsed into one"
        );

        // Both KNOWN terminal tokens reach `Ended` — neither is shadowed by the
        // contradiction check that runs before the qualifier match.
        for token in ["stopped", "failed"] {
            let ended = agent("background", Some(token), None);
            assert_eq!(classify(&ended), AgentActivity::Ended, "{token:?} -> Ended");
            assert!(!is_active(&ended), "{token:?} has ended; it must be steady");
            // Passed through VERBATIM: `Ended` is not one of the translated buckets.
            assert_eq!(qualifier_copy(&ended), Some(token));
        }

        // The contradiction reaches `WorkingButIdle` — not shadowed by the
        // qualifier match, which would otherwise collapse it to a plain `Working`.
        let interrupted = agent("background", Some("working"), Some("idle"));
        assert_eq!(classify(&interrupted), AgentActivity::WorkingButIdle);
        assert!(!is_active(&interrupted), "interrupted must be steady too");
        assert_eq!(qualifier_copy(&interrupted), Some(INTERRUPTED_COPY));

        // ORDER-PROOF: the two triggers are disjoint, so a terminal token wins even
        // when the record ALSO carries the `idle` status the contradiction keys on.
        // If the contradiction check ever widened to swallow this shape, a stopped
        // job would silently read `interrupted` in the working gray instead.
        let stopped_and_idle = agent("background", Some("stopped"), Some("idle"));
        assert_eq!(
            classify(&stopped_and_idle),
            AgentActivity::Ended,
            "a reported terminal token outranks the inferred contradiction"
        );

        // The FAIL-SOFT default survives both new buckets: a genuinely unknown
        // qualifier is still `Other`, still passed through, and still PULSES, so
        // schema drift never hides a busy session behind a steady dot.
        let unknown = agent("background", Some("compacting"), None);
        assert_eq!(classify(&unknown), AgentActivity::Other);
        assert!(
            is_active(&unknown),
            "an unknown qualifier must keep the active default and pulse"
        );
        assert_eq!(qualifier_copy(&unknown), Some("compacting"));
    }

    /// The BOARD poll's exact invocation, pinned without spawning `claude` (the
    /// suite never does) — the same way `resume`'s hand-off argvs are pinned.
    ///
    /// `--all` is the load-bearing half and the reason this test exists: the bare
    /// command drops a finished job once claude reaps it from the active list, so
    /// without the flag a `done` session's badge and banner vanish the moment it is
    /// reaped — the `done` bucket stops RELIABLY reaching `classify`. Every `done`
    /// assertion in this module would still pass, because they feed `classify`
    /// synthetic agents rather than the wire; only this assertion sees the flag.
    /// `--json` is pinned alongside it because the human table it otherwise prints
    /// is not what `parse_agents_json` reads.
    #[test]
    fn agents_argv_is_claude_agents_json_all() {
        assert_eq!(
            agents_argv().join(" "),
            "claude agents --json --all",
            "the reported-agents shell-out must pass --all (else the `done` \
             bucket is unobservable) and --json (else the output is unparseable)"
        );
    }

    /// The GATE probe's exact invocation — and the ABSENCE of `--all` is the
    /// whole contract, so this test exists to fail the moment someone "unifies"
    /// the two argvs.
    ///
    /// `--all` here would be silent and catastrophic: the probe's answer is a
    /// MEMBERSHIP test, so including finished agents would report every one of
    /// them as live and divert Enter into the Attach/Fork overlay instead of
    /// resuming — for the large majority of rows. Nothing else in the suite would
    /// notice, because every other test feeds synthetic sets rather than the wire.
    /// Asserted twice on purpose: the exact string pins the whole invocation, and
    /// the explicit flag scan states the one thing that must never come back, so
    /// the failure names the actual defect.
    #[test]
    fn live_agents_argv_is_claude_agents_json_without_all() {
        assert_eq!(
            live_agents_argv().join(" "),
            "claude agents --json",
            "the liveness probe must ask for claude's ACTIVE list only"
        );
        assert!(
            !live_agents_argv().iter().any(|arg| arg == AGENTS_ALL_FLAG),
            "--all must NEVER reach the liveness probe: it would make every \
             finished session test as live and break plain resume for all of them"
        );
    }

    /// The two readings differ by EXACTLY the one flag, and share everything
    /// else. Pins the factoring itself: the shared prefix has one source of
    /// truth, so neither argv can drift on the program or the subcommand while
    /// still asserting its own flags above.
    #[test]
    fn the_two_agent_argvs_differ_only_by_the_all_flag() {
        let poll = agents_argv();
        let probe = live_agents_argv();
        assert_eq!(
            poll.len(),
            probe.len() + 1,
            "the poll argv is the probe argv plus exactly one flag"
        );
        assert_eq!(
            &poll[..probe.len()],
            &probe[..],
            "both readings must share one prefix"
        );
        assert_eq!(poll.last().map(String::as_str), Some(AGENTS_ALL_FLAG));
    }

    /// Task VERIFY-1: garbage / empty / invalid / non-array JSON must all yield
    /// an EMPTY reported set and never panic.
    #[test]
    fn garbage_empty_and_invalid_json_yield_an_empty_reported_set() {
        for raw in [
            "",
            "   ",
            "not json at all",
            "{",
            "null",
            "42",
            "\"a bare string\"",
            "{\"sessionId\":\"x\"}", // a JSON OBJECT, not the documented array
            "[1, 2, 3]",             // array of non-objects
        ] {
            let reported = parse_agents_json(raw);
            assert!(
                reported.is_empty(),
                "expected an empty reported set for {raw:?}, got {reported:?}"
            );
        }
    }

    /// Task VERIFY-2 (parse side): reported agents are keyed by their FULL
    /// `sessionId`, with kind/state/status extracted.
    #[test]
    fn parses_reported_agents_keyed_by_full_session_id() {
        let raw = r#"[
            {"sessionId":"11111111-2222-3333-4444-555555555555","kind":"background","state":"blocked","status":"idle","pid":42,"id":"11111111","name":"bg-one"},
            {"sessionId":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee","kind":"interactive","status":"busy"}
        ]"#;
        let reported = parse_agents_json(raw);
        assert_eq!(reported.len(), 2);

        let bg = reported
            .get("11111111-2222-3333-4444-555555555555")
            .expect("background agent present under its full sessionId");
        // Full struct equality also exercises every field (incl. `name` and the
        // short agent-view `id` that `claude attach` matches).
        assert_eq!(
            bg,
            &ReportedAgent {
                kind: "background".to_string(),
                id: Some("11111111".to_string()),
                state: Some("blocked".to_string()),
                status: Some("idle".to_string()),
                name: Some("bg-one".to_string()),
            }
        );
        assert_eq!(bg.kind_label(), "bg");
        assert_eq!(bg.qualifier(), Some("blocked"));

        let inter = reported
            .get("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
            .expect("interactive agent present under its full sessionId");
        assert_eq!(inter.kind_label(), "live");
        // No `state` -> qualifier falls back to `status`.
        assert_eq!(inter.qualifier(), Some("busy"));
        // An interactive session exposes no agent-view job `id` -> not
        // attachable (the gate the Attach hand-off relies on).
        assert_eq!(inter.id, None);
    }

    /// A well-formed one-record agents array. Shared by the success/failure pair
    /// below so that the EXIT STATUS is the only thing differing between them.
    const ONE_AGENT_JSON: &str =
        r#"[{"sessionId":"sess-1","kind":"background","state":"working","id":"job-1"}]"#;

    /// The success path: a zero exit hands stdout to the shared parser and the
    /// records come back — including the attach job `id`, since this seam is what
    /// `live_agents` reads them through.
    #[test]
    fn a_successful_run_parses_its_stdout_into_records() {
        let agents = agents_from_output(true, ONE_AGENT_JSON);
        assert_eq!(agents.len(), 1);

        let agent = agents
            .get("sess-1")
            .expect("the record is keyed by its full sessionId");
        assert_eq!(agent.kind_label(), "bg");
        assert_eq!(agent.id.as_deref(), Some("job-1"));
        assert_eq!(agent.qualifier(), Some("working"));
    }

    /// A NON-ZERO exit is "no signal" — and the stdout here is VALID JSON on
    /// purpose, which is the entire point of the case.
    ///
    /// Garbage stdout would pass this for the WRONG reason: the parser rejects it
    /// anyway, so an empty result would prove nothing about the status check and
    /// would stay green with the check deleted. Only parseable stdout makes the
    /// status the sole thing that can produce the empty map.
    ///
    /// The direction is load-bearing for `live_agents`, whose caller reads
    /// MEMBERSHIP as liveness: without this check, a failed run's stdout could
    /// report sessions as live and divert Enter into the Attach/Fork overlay
    /// instead of resuming.
    #[test]
    fn a_failed_run_yields_empty_even_when_its_stdout_is_valid_json() {
        assert!(
            agents_from_output(false, ONE_AGENT_JSON).is_empty(),
            "a non-zero exit is no signal: its stdout must never be read, even \
             when it parses cleanly"
        );
        // The same bytes DO parse on a zero exit, so the emptiness above is the
        // status check's doing rather than an unreadable fixture quietly passing.
        assert!(
            !agents_from_output(true, ONE_AGENT_JSON).is_empty(),
            "the fixture must be parseable, else the assertion above is vacuous"
        );
    }

    /// FAIL-SOFT on the success path too: a zero exit whose output the parser
    /// cannot read collapses to empty rather than panicking. The parse itself is
    /// covered above; this pins that the seam delegating to it degrades the same
    /// way.
    #[test]
    fn a_successful_run_with_non_json_stdout_yields_empty() {
        assert!(
            agents_from_output(true, "claude: unexpected error").is_empty(),
            "unparseable stdout on a clean exit is still no signal"
        );
    }

    /// Schema drift (missing optional fields, unknown extra fields, or a
    /// sessionId-less element) never drops the WHOLE parse.
    #[test]
    fn schema_drift_never_fails_the_whole_parse() {
        let raw = r#"[
            {"kind":"background"},
            {"sessionId":"kept","future":"field","kind":42}
        ]"#;
        let reported = parse_agents_json(raw);
        // The sessionId-less element is skipped; the other survives.
        assert_eq!(reported.len(), 1);
        let kept = reported
            .get("kept")
            .expect("record with a sessionId survives");
        // `kind` was a NUMBER (not a string) -> fail-soft to empty, no panic.
        assert_eq!(kept.kind, "");
        assert_eq!(kept.qualifier(), None);
        assert_eq!(kept.name, None);
    }
}
