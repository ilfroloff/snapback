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
//! * **[`reported_agents`] — `--json --all` — the BOARD's signal.** Polled ~1s
//!   off-thread; drives badges, colors, the pulse and the preview banner via
//!   [`classify`]. `--all` is what makes [`AgentActivity::Done`] observable at
//!   all: the bare command reports only currently-active agents, so without it a
//!   session that just wrapped up renders as though claude had never heard of it.
//! * **[`live_agents`] — `--json`, NO `--all` — the HAND-OFF's signal.** A
//!   one-shot probe at hand-off; MEMBERSHIP in what it returns is liveness,
//!   structurally, because the bare command IS claude's active list. It returns
//!   the RECORDS, so the same authoritative read also carries each live agent's
//!   attach job [`id`](ReportedAgent::id).
//!
//! The durable finding behind the split: **`--all`'s `state: "done"` means "the
//! agent reported completion", NOT "claude will permit `-r`".** The two can
//! disagree transiently — an agent can report `done` while claude still holds the
//! session, and a session can go live between two polls — and **claude is the
//! only authority** on its own refusal. So the resume gate probes claude's active
//! list AT HAND-OFF rather than inferring liveness from a polled snapshot that is
//! up to ~1.3s stale. Inferring `state != "done"` ⇒ live agrees in steady state
//! but is a GUESS about claude's gate; membership in the bare list is not.
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
/// Observable ONLY under `--all` (the bare `claude agents --json` reports just
/// the currently-active agents), which is precisely why [`agents_argv`] passes
/// it.
///
/// It means the agent SAID it finished — NOT that claude will permit `-r`. Those
/// two can disagree transiently, and claude is the authority on its own refusal,
/// so this value colors a BADGE and never gates the resume: the gate asks
/// [`live_agents`] instead.
const QUALIFIER_DONE: &str = "done";

/// User-facing copy for [`AgentActivity::NeedsInput`] — the ONE translated
/// bucket. Phrased as what the SESSION wants ("needs input"), so the banner
/// tells the user why it stopped instead of restating the raw token
/// ([`QUALIFIER_BLOCKED`] or [`QUALIFIER_WAITING`]).
const NEEDS_INPUT_COPY: &str = "needs input";

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
    /// field exists on both; but an `id` off the `--all` map is a ~1.3s-stale
    /// snapshot of a job that may have ended, and spawning `claude attach` with it
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
            "background" => "bg",
            "interactive" => "live",
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
/// and the badge color ([`crate::tui::view::badge_color`]) all map from it, so a
/// schema drift is a one-line change in [`classify`] rather than a hunt for raw
/// string matches scattered across the UI.
///
/// Every consumer is a DISPLAY decision, and that is the boundary: no bucket
/// gates the resume. Liveness is not inferred from a qualifier — it is
/// [`live_agents`]' membership answer, straight from claude.
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
    /// Reported completion ([`QUALIFIER_DONE`]); only observable under `--all`.
    /// Badges green and steady — it does NOT decide whether `-r` is permitted.
    Done,
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
#[must_use]
pub fn classify(agent: &ReportedAgent) -> AgentActivity {
    match agent.qualifier() {
        Some(QUALIFIER_BLOCKED | QUALIFIER_WAITING) => AgentActivity::NeedsInput,
        Some(QUALIFIER_IDLE) => AgentActivity::Idle,
        Some(QUALIFIER_WORKING | QUALIFIER_BUSY) => AgentActivity::Working,
        Some(QUALIFIER_DONE) => AgentActivity::Done,
        _ => AgentActivity::Other,
    }
}

/// A one-line, human-readable status for a reported agent: the kind label (`bg`
/// / `live`) plus its qualifier — e.g. `bg needs input`, `live working`.
///
/// Pure, and derived from [`classify`] so the value set is interpreted in
/// exactly one place. Translates ONLY [`AgentActivity::NeedsInput`] — BOTH of
/// its qualifiers ([`QUALIFIER_BLOCKED`] and [`QUALIFIER_WAITING`]) render as
/// [`NEEDS_INPUT_COPY`], since the bucket is the thing being named and two
/// spellings of "it wants you" must not read as two different states. Every
/// other bucket renders the raw qualifier VERBATIM (fail-soft: an unknown value
/// is shown, never hidden or relabeled), and an agent with no qualifier at all
/// renders the kind label alone.
#[must_use]
pub fn friendly_status(agent: &ReportedAgent) -> String {
    let label = agent.kind_label();
    let Some(qualifier) = agent.qualifier() else {
        return label.to_string(); // Nothing to qualify -> the label stands alone.
    };
    let phrase = match classify(agent) {
        AgentActivity::NeedsInput => NEEDS_INPUT_COPY,
        AgentActivity::Idle
        | AgentActivity::Working
        | AgentActivity::Done
        | AgentActivity::Other => qualifier,
    };
    format!("{label} {phrase}")
}

/// Whether a reported agent is actively WORKING (versus waiting or finished),
/// deciding if its list-badge dot pulses.
///
/// Pure, and derived from [`classify`] so "active" can never disagree with the
/// banner about what the qualifier meant. The RESTING buckets are steady:
/// [`AgentActivity::NeedsInput`] (stopped, waiting on the user),
/// [`AgentActivity::Idle`] (up, but not working a turn), and
/// [`AgentActivity::Done`] (finished — nothing is happening to animate).
///
/// [`AgentActivity::Other`] — an unknown or absent qualifier — counts as ACTIVE
/// on purpose: schema drift fails toward showing activity rather than silently
/// hiding a busy session behind a steady dot.
#[must_use]
pub fn is_active(agent: &ReportedAgent) -> bool {
    match classify(agent) {
        AgentActivity::NeedsInput | AgentActivity::Idle | AgentActivity::Done => false,
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
/// [`AGENTS_ALL_FLAG`] is what makes [`AgentActivity::Done`] observable AT ALL —
/// the bare command reports only the currently-active agents, so dropping it
/// silently erases the finished bucket and a session that just wrapped up renders
/// as though claude had never heard of it. It is not a nicety; it is the flag the
/// `done` badge exists on.
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
/// [`crate::watch::EventLoop::spawn_agents_poller`], which polls it ~1s).
#[must_use]
pub fn reported_agents() -> HashMap<String, ReportedAgent> {
    run_agents(&agents_argv())
}

/// Probe claude's ACTIVE agent list (`claude agents --json`, no `--all`) and
/// return the agents it is holding open RIGHT NOW, keyed by full `sessionId`.
///
/// The authoritative answer to BOTH hand-off questions, from ONE read:
///
/// * **"Will `claude -r <id>` refuse?"** — MEMBERSHIP, and the reason that is
///   enough is structural: the bare command IS claude's active list, so there is
///   no bucket to infer from and nothing to be uncertain about.
/// * **"What does `claude attach` take?"** — the matched record's own short
///   agent-view [`id`](ReportedAgent::id).
///
/// It returns the RECORDS rather than bare ids precisely so the second question
/// has an authoritative answer. The parse has the `id` in hand either way;
/// discarding it would force the attach path back onto [`reported_agents`]' map
/// — an authoritative decision made from a ~1.3s-stale snapshot, which is the
/// exact class of bug that moving the liveness gate here fixed. One shell-out,
/// one parse, both answers, no second notion of "which agent is this".
///
/// [`reported_agents`]' `--all` map cannot answer either question: it is up to
/// ~1.3s stale and its `done` qualifier means "the agent reported completion",
/// not "claude will permit `-r`".
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

    /// `done` is a bucket of its own: an agent that REPORTED completion. Observed
    /// only under `--all`, rendered verbatim, and steady — there is no work left
    /// to animate.
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
    }

    /// The BOARD poll's exact invocation, pinned without spawning `claude` (the
    /// suite never does) — the same way `resume`'s hand-off argvs are pinned.
    ///
    /// `--all` is the load-bearing half and the reason this test exists: the bare
    /// command reports only the CURRENTLY-ACTIVE agents, so dropping the flag
    /// makes `AgentActivity::Done` unobservable — no `done` record ever reaches
    /// `classify`, and a just-finished session silently loses its badge and its
    /// banner. Every `done` assertion in this module would still pass, because
    /// they feed `classify` synthetic agents rather than the wire; only this
    /// assertion sees the flag. `--json` is pinned alongside it because the human
    /// table it otherwise prints is not what `parse_agents_json` reads.
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
