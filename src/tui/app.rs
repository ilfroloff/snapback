//! The `App` model.
//!
//! Holds all TUI state: `sessions`, `filtered` indices, `selected` session id,
//! `scroll` offset, `query`, `search_mode` (name | name+content), `scope`
//! (current-folder | project | all) with the project's cached worktree set, and
//! the preview cache. Selection is tracked by stable `session_id` (not list
//! index) so it survives autorefresh.
//!
//! Everything in this module is pure state manipulation with no terminal I/O,
//! so it is unit-testable without a real terminal. The terminal-driving loop
//! lives in [`crate::tui`] (run) and the key/event dispatch in
//! [`crate::tui::update`].

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use ratatui::layout::Rect;
use ratatui::text::{Line, Text};
use time::OffsetDateTime;

use crate::agents::ReportedAgent;
use crate::defined_agents::{self, DefinedAgent};
use crate::{config, delete, hidden};

use crate::search::{SearchIndex, SearchMode};
use crate::store::lineage::{self, LineageKey};
use crate::store::{preview, Session};
// The scope predicate and the worktree resolver MUST canonicalize paths the same
// way or membership compares apples to oranges, so both call the one
// `resolve_dir` that lives beside the worktree set it has to match.
use crate::worktrees::{project_root, project_root_name, resolve_dir, WorktreeSet};

// The transcript's wrap model is the VIEW's (it is a fact about the widget that
// paints the pane, not about this state), so the cache here stores what that module
// measures rather than re-deriving a second answer.
use super::view;

/// Lines the preview scrolls per mouse-wheel notch. Small (the terminal reports
/// discrete notches, not pixel deltas) so a trackpad flick stays controllable.
const PREVIEW_WHEEL_STEP: i32 = 2;

/// Rows the list selection moves per mouse-wheel notch.
const LIST_WHEEL_STEP: isize = 1;

/// How many `AppEvent::Tick`s a transient confirmation/nudge lives on the help line.
///
/// `16 * watch::TICK` (250ms) = 4s — long enough to read a short confirmation,
/// short enough not to squat on the keymap row. `STATUS_DWELL_TICKS` is multiplied
/// by `watch::TICK`, so the two must be tuned together (PATTERNS §8).
pub(crate) const STATUS_DWELL_TICKS: u16 = 16;

/// Minimum columns either pane keeps when the list/preview splitter is
/// dragged, so neither pane can be crushed to zero width or dragged past the
/// other (which would invert the layout).
pub const MIN_PANE_WIDTH: u16 = 15;

/// The list pane's share of the body width before the user has ever dragged
/// the splitter — matches the historical `Constraint::Percentage(48)` split
/// this feature replaces.
const DEFAULT_LIST_PERCENT: u32 = 48;

/// Prefix of the board status shown when persisting the hidden-id set fails.
/// Hidden state is a CONVENIENCE, so a write error degrades to a message rather
/// than aborting the board; the in-memory set stays authoritative for the rest
/// of the session (FAIL-SOFT). Named rather than inlined so the one user-facing
/// wording lives in exactly one place (NO MAGIC VALUES).
const HIDDEN_SAVE_ERROR_PREFIX: &str = "could not save hidden sessions";

/// The HARD-delete confirm's standing prompt: the irreversibility warning plus
/// the BLAST RADIUS, which the writer guard's admission of parked background
/// agents makes load-bearing — the agent survives in Claude Code, and attaching
/// then replying later writes a fresh transcript under the same session.
///
/// Named rather than inlined so [`delete_confirm_message`] composes it instead of
/// re-spelling it, and so the copy under test is the copy that ships.
const DELETE_CONFIRM_PROMPT: &str = "Permanently delete this transcript from disk? This \
     can't be undone. It removes the file only: a background agent keeps running in Claude \
     Code until you stop it there, and attaching then replying later can write a new \
     transcript.";

/// The HARD-delete confirm's message: [`DELETE_CONFIRM_PROMPT`], led by a
/// DISCLOSURE sentence when the lineage button would take members the board is
/// not showing.
///
/// `members` is the size of the set `Delete lineage (N)` acts on; `hidden` is how
/// many of those are in [`App::hidden_ids`]. The disclosure exists because the two
/// can differ: [`App::lineage_member_ids`] sweeps the FULL store, so a
/// soft-hidden member is counted in `(N)` and deleted with the rest (deliberate —
/// hiding is a visibility preference, not a tombstone, so a lineage delete takes
/// the whole lineage). The button can therefore read `Delete lineage (5)` with
/// only three of those rows on screen, and an IRREVERSIBLE action the user cannot
/// predict is not acceptable. This changes what is TOLD, never what is taken.
///
/// It says nothing extra when `hidden` is 0, and nothing when `members <= 1`
/// (there is no lineage button to disclose for).
///
/// **The counts lead the sentence, and the sentence leads the message**, because
/// that is the most clip-RESISTANT place to put them. The message is wrapped by
/// `view::wrap_message` to the modal-width CONSTANT rather than to the clamped
/// area, so on a terminal narrower than the box every message row loses its TAIL;
/// leading with the counts puts them in the first columns of the FIRST row, where
/// they survive far more clipping than a mid-strip button label would — both counts
/// still read whole at 30 columns, under half the box's width.
///
/// A better position, NOT a safe one. A MULTI-DIGIT count can still be cut into a
/// shorter, plausible one: at 23 columns a 12-member/10-hidden lineage reads
/// `12 in this lineage, 1`, precisely the wrong-number risk `view::fit_child_msgs`
/// refuses to take. That needs a terminal about a third of the box's width — where
/// the same counts in the button label would be gone entirely rather than merely
/// wrong — so this is the better end of a trade, not the absence of one.
///
/// The alternative — those counts in the `Delete lineage` LABEL — was measured and
/// rejected on WIDTH, drawn through a `TestBackend`. It grows the button strip from
/// 49 to 59 columns, and `view::render_modal` never wraps that strip (a `Row` modal
/// only centers it), so it TRUNCATES: `Cancel` — the SAFE DEFAULT — would need a
/// 60-column terminal to draw whole, where the plain strip needs 50. Ten columns of
/// legibility off the one button that undoes a mistake.
///
/// **The shipped option is not free either, and its cost is on the HEIGHT axis.**
/// The sentence adds ONE wrapped row (4 → 5 at the 60-column inner width).
/// `view::centered_rect` clamps the box and `render_modal` draws top-down with no
/// vertical scroll, so the button strip now needs a terminal 9 rows tall instead of
/// 8 and the `Esc cancel` footer 11 instead of 10 — a terminal exactly 8 rows tall
/// loses a strip it used to draw. One row is what THIS sentence costs, NOT a floor
/// under any disclosure (a short enough prefix reflows nothing); the first draft
/// cost TWO, which is why the shipped one is terse.
///
/// What a too-short terminal loses is legibility, never the safe default:
/// [`App::open_delete_confirm`] leaves `Cancel` PRESELECTED, and Esc/Enter still
/// cancel with the strip off screen.
///
/// `view::tests` pins exactly this much:
/// `the_disclosing_delete_confirm_costs_one_row_and_keeps_cancel_default` sweeps
/// rendered terminal heights for 4 → 5 wrapped rows and the 9/11 thresholds against
/// their 8/10 non-disclosing baselines;
/// `counts_in_the_lineage_label_would_cost_cancel_ten_columns` the 50-vs-60-column
/// `Cancel` comparison; `the_disclosed_counts_survive_clipping_to_a_third_of_the_box`
/// the 30- and 23-column readings. The box heights those thresholds come from are
/// derivation, NOT measurement — nothing branches on them and no test pins them; the
/// thresholds survive a changed `view::MODAL_ROW_CHROME_ROWS` because the sweep
/// searches rendered TEXT and the clamp makes a taller box clip differently without
/// moving the strip's first visible row.
///
/// Pure so the copy is testable without a store, a modal, or a terminal
/// (PATTERNS §3).
#[must_use]
fn delete_confirm_message(members: usize, hidden: usize) -> String {
    if members <= 1 || hidden == 0 {
        return DELETE_CONFIRM_PROMPT.to_string();
    }
    // Deliberately terse: every word costs wrapped rows, and a wrapped row costs
    // the button strip a whole terminal size (see above). The button beside it
    // already reads `Delete lineage (N)`, so the sentence states the SPLIT rather
    // than re-explaining what the button does.
    format!("{members} in this lineage, {hidden} of them hidden. {DELETE_CONFIRM_PROMPT}")
}

/// Which set of sessions the list shows.
///
/// Three concentric answers to "which sessions are mine right now", declared
/// WIDEST-LAST so the variant order is the cycle order: the exact launch folder
/// ([`Scope::CurrentFolder`], the default), every live worktree of that folder's
/// project ([`Scope::Project`]), then the whole store ([`Scope::All`]). Selected
/// at launch by a CLI flag, and cycled by a keybinding — though only the first
/// two are on that key by default; see [`Scope::toggled`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Only sessions launched from the current working directory (exact
    /// canonical `cwd` match).
    CurrentFolder,
    /// Every session launched from ANY worktree of the current project — the
    /// live ones AND the ones that have since been removed.
    ///
    /// Membership is [`in_scope`]'s two-armed test: the project's worktree roots
    /// as `git worktree list --porcelain` reports them ([`crate::worktrees`]),
    /// which is why it can span folders a pure path heuristic could never
    /// relate, OR a shared repo ROOT, which is why a deleted worktree's sessions
    /// stay in the project instead of dropping to the all scope. The git set is
    /// resolved once at launch and AUTORELOADED on every session reload, so a
    /// worktree created mid-run joins the view without restarting the board.
    ///
    /// FAIL-SOFT: an unresolved set (no `git`, not a repository, a non-zero exit)
    /// is EMPTY, and the root arm carries the scope on its own from there —
    /// narrower than git could make it, never "nothing matches".
    Project,
    /// Every session, grouped by folder.
    ///
    /// Reachable ONLY by launching with `--all`/`-a`
    /// ([`crate::cli::Args::all_scope_enabled`]) — there is no in-board chord
    /// for it, and [`Scope::toggled`] skips it otherwise.
    All,
}

impl Scope {
    /// Advance to the next scope.
    ///
    /// Without `all_enabled` this is a two-state FLIP — current folder <->
    /// project — and with it the historical three-stop cycle, current folder ->
    /// project -> all -> current folder. Either way it WIDENS at every step
    /// before wrapping, so one key walks steadily outward and never means
    /// "narrow" partway through.
    ///
    /// [`Scope::All`] is off the key by default because it is the whole store —
    /// every session of every repo on the machine, the widest and least often
    /// wanted answer — and it used to sit MID-cycle, one stray press away.
    /// `Scope::Project` now spans a project's whole history, deleted worktrees
    /// included (see [`in_scope`]), so the middle stop is what the wide press
    /// was usually reaching for anyway.
    ///
    /// `all_enabled` is a PARAMETER rather than something this reads off the
    /// board, exactly as [`in_scope`] takes its worktree set: the cycle is then
    /// assertable on its own, with no [`App`] to build.
    #[must_use]
    pub fn toggled(self, all_enabled: bool) -> Self {
        match self {
            Scope::CurrentFolder => Scope::Project,
            Scope::Project if all_enabled => Scope::All,
            // The flip's return leg: no `-a`, so there is nothing wider to
            // reach and the key wraps here instead.
            Scope::Project => Scope::CurrentFolder,
            // `All` wraps to the narrowest scope whether or not the flag is on.
            // With it, that closes the three-stop cycle. WITHOUT it the state is
            // only reachable by a bug — but the function is total, and the safe
            // answer to "where next" is OUT of the widest scope: returning
            // `Scope::All` here would strand the user in the one scope they
            // never asked for, with no key that leaves it.
            Scope::All => Scope::CurrentFolder,
        }
    }
}

/// A rendered row in the grouped list: either a group head (shown ONCE per
/// repo->branch group) or a session row addressing `sessions[index]`.
///
/// [`build_rows`] guarantees rows of the same group are contiguous and each
/// group emits exactly one head, so the git-log-style folder head appears once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// A group head for a repo->branch group.
    Group {
        /// The repo grouping label.
        repo: String,
        /// The branch label (`(detached)` when absent).
        branch: String,
    },
    /// A session row addressing [`App::sessions`].
    Session {
        /// Index into [`App::sessions`].
        index: usize,
        /// How many fork-lineage members this row is standing in for, driving
        /// its `(+N)` marker. `0` on a plain row — a lone session, and an
        /// EXPANDED head (whose members are drawn on their own rows and are
        /// therefore not hidden by anything). Only a genuinely folded head
        /// carries a count, so a `(+N)` can never claim a fold that did not
        /// happen.
        hidden: usize,
        /// This row is an expanded lineage member that is NOT its head, so it is
        /// drawn indented beneath the head it belongs to. Never true for a row
        /// with no lineage, which must look exactly as it always has.
        child: bool,
    },
}

/// The layout a [`Modal`] renders in — the ONLY structural fork the generic modal
/// supports, and the thing its key map is derived from: a `Row` binds the
/// horizontal `←`/`→`/`h`/`l` keys (a button strip); a `List` deliberately does
/// NOT (a vertical picker navigated with `↑`/`↓`/`k`/`j`/`Tab` alone). Namespaced
/// under `ModalLayout` so the `Row` variant does not collide with the list-row
/// [`Row`] enum above.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalLayout {
    /// A horizontal strip of buttons (the running-session Attach/Fork/Cancel
    /// choice; a delete confirm). Binds the horizontal keys on top of the shared
    /// vertical ones.
    Row,
    /// A vertical list of rows (the new-session agent picker). Vertical keys only.
    List,
}

/// What confirming a [`ModalChoice`] does — a plain tag the ONE generic confirm
/// handler matches on, so a single handler serves every modal: the running-session
/// overlay (`Attach`/`Fork`/`Cancel`), the new-session picker (`New`), and the
/// hard-delete confirm (`Delete`). Carries no borrowed data so it can ride on a
/// choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModalAction {
    /// Attach to the running session's background agent (`claude attach <job-id>`),
    /// keyed on the modal's target `session_id`. Only a background (`bg`) agent
    /// carries an attachable job id; an interactive (`live`) session has none, so
    /// the choice refuses with a clear hint (Fork instead).
    Attach,
    /// Fork the target session (`claude -r <id> --fork-session`).
    Fork,
    /// Start a brand-new session, optionally bound to the named agent. `None` is
    /// the "default (no agent)" row (a bare `claude`); the agent name rides the
    /// choice so the confirm handler needs no index-to-agent lookup.
    New(Option<String>),
    /// HARD-delete the target session's transcript from disk, keyed on the modal's
    /// `session_id`. The confirm handler runs the pure `delete::can_delete_target`
    /// writer guard first, then the FS removal; the modal only OPENS the prompt, it
    /// never deletes on its own. Default-highlighted on Cancel for safety.
    Delete,
    /// HARD-delete EVERY session in the target's fork lineage — the same grouping
    /// `Ctrl-X x` hides as one unit — carrying the member ids the choice was BUILT
    /// with.
    ///
    /// The ids ride the action rather than being re-derived at confirm time, and
    /// that is a correctness property, not a convenience (the precedent is
    /// [`ModalAction::New`], which carries its agent name for the same reason).
    /// `App::lineage_member_ids` resolves the lineage from the CURRENT SELECTION,
    /// while a `SessionsChanged` reload can clamp that selection to an unrelated
    /// row with the modal still open — so re-deriving on confirm could delete a
    /// lineage the user never saw named. Carrying them also makes the `(N)` in the
    /// button label and the set actually deleted the SAME value by construction.
    ///
    /// The confirm handler guards each member individually and deletes the ones
    /// that pass; one busy fork must not block the rest of the family.
    DeleteLineage(Vec<String>),
    /// Dismiss the modal, returning to the board.
    Cancel,
}

/// One selectable choice in a [`Modal`]: its button/row label, an optional dim
/// description (the picker's per-agent blurb; unused by the `Row` layout), and the
/// [`ModalAction`] the confirm handler runs when it is highlighted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModalChoice {
    /// The user-facing button/row label.
    pub label: String,
    /// An optional dim description trailing the label (List layout only).
    pub description: Option<String>,
    /// What confirming this choice does.
    pub action: ModalAction,
}

/// The open "stop the waiting agent?" confirmation, shown when `Ctrl-R` targets a
/// `needs input` background agent: stopping it to reply in place would abandon a
/// live agent, so the user confirms first (`Enter`) or cancels (`Esc`). A simple
/// yes/no gate (no navigation), so it holds only what a confirmed stop-then-reply
/// needs: the target session and the job id to `claude stop`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingStop {
    /// Stable `session_id` the reply will target once confirmed.
    pub session_id: String,
    /// Short agent-view job id to `claude stop` before the reply.
    pub job_id: String,
}

/// The open "stop this agent?" confirmation, shown when `Ctrl-K` (interrupt) targets
/// a LIVE agent that is not already finished: stopping it abandons live work, so the
/// user confirms first (`Enter`) or cancels (`Esc`). A simple yes/no gate (no
/// navigation). Distinct from [`PendingStop`], which is the reply's stop-THEN-reply
/// pre-step; this one resolves into an actual `claude stop` and nothing more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingInterrupt {
    /// Stable `session_id` the interrupt targets — kept only to label the modal.
    pub session_id: String,
    /// Short agent-view job id to `claude stop` on confirm.
    pub job_id: String,
}

/// A quick-reply send that is IN FLIGHT (dispatched, not yet finished).
///
/// Carries everything the preview needs to render the reply OPTIMISTICALLY while
/// `claude -p -r` runs: the target session, the message that was sent, and the
/// session's turn count AT SEND TIME. The count is the dedup signal — while the
/// reloaded session still reports `baseline_msg_count`, claude has not yet written
/// the user turn, so the preview echoes a synthetic one; the moment the real turn
/// lands (count grows) the echo yields to it, so the swap never doubles the line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sending {
    /// Authoritative `sessionId` the reply targets (re-read from the file at Send
    /// time, so it is what identifies the in-flight row — STABLE-ID STATE).
    pub session_id: String,
    /// The message text that was sent, echoed under a synthetic `▶ you` turn until
    /// claude writes the real one to disk.
    pub message: String,
    /// The target session's [`Session::msg_count`](crate::store::Session::msg_count)
    /// when the send was dispatched. The echo shows only while the reloaded count
    /// still equals this — i.e. nothing new has landed on disk yet.
    pub baseline_msg_count: usize,
}

/// A `claude stop` interrupt that is IN FLIGHT (dispatched, not yet finished).
///
/// Carries the session id the interrupt targeted so a completion event can be
/// attributed to the surface that dispatched it and ignored if the board has
/// moved on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interrupting {
    /// Authoritative `sessionId` the interrupt targeted.
    pub session_id: String,
}

/// The open NEW-SESSION draft — the PANE-level twin of the compose editor.
///
/// Its whole job is to let the VIEW know a background draft is open WITHOUT
/// reading [`super::compose::ComposeState`]: the compose editor answers "what does
/// the keyboard do", this answers "what does the preview pane show". Keeping them
/// apart is the point — while this is set the preview renders a placeholder card
/// instead of the SELECTED session's transcript, so a docked compose box can never
/// sit over an unrelated conversation and read as a reply to it.
///
/// Deliberately holds NOTHING about a session: a brand-new agent has no
/// `sessionId` yet, no row on the board, and no transcript. The agent name is here
/// only because the card names it; once the agent exists, its own `agent-setting`
/// record (already rendered by [`crate::store::preview`]) is what says which agent
/// it was.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NewSessionDraft {
    /// The picked agent, or `None` for the picker's "default (no agent)" row.
    pub agent: Option<String>,
    /// The launch this card is reporting, or `None` while it is still being typed.
    ///
    /// Stamped by [`App::dispatch_draft`] the moment `Enter` hands a
    /// [`crate::send::BgLaunchRequest`] to the driver, and cleared when THAT
    /// launch's one-shot
    /// [`AppEvent::BgLaunchFinished`](crate::watch::AppEvent::BgLaunchFinished)
    /// lands. Exactly the shape of [`Sending`] — set at dispatch, cleared by the
    /// completion event already on the channel — so the card can report the launch
    /// without a tick, thread, or event source of its own.
    ///
    /// It is an ID rather than a flag for the same reason [`Sending`] carries a
    /// `session_id`: the card outlives the editor, so by the time a result arrives
    /// the surface underneath may be something else entirely (a quick reply, a
    /// second draft). [`App::launching_draft`] is the matching identity check, and
    /// without it a completing launch tears down whatever is open — including a
    /// half-typed reply.
    pub launch_id: Option<u64>,
}

impl NewSessionDraft {
    /// Whether this card is reporting a DISPATCHED launch (rather than an editable
    /// draft). The rendering predicate behind the card's in-flight line — one
    /// reading of [`launch_id`](Self::launch_id), so "in flight" and "which launch"
    /// can never disagree.
    #[must_use]
    pub fn is_launching(&self) -> bool {
        self.launch_id.is_some()
    }
}

/// A titled, centered prompt with N labelled choices and a wrapping-cycle
/// highlight — the ONE overlay model behind the running-session choice, the
/// new-session agent picker, and (later) a delete confirm.
///
/// Modeled as explicit state so the whole overlay is a small, unit-testable state
/// machine that owns the keyboard while open. `selected` is a `rem_euclid` index
/// over `choices` (wraps both directions, both layouts). `session_id` is the
/// target a session-addressed action routes to (`Some` for Attach/Fork/delete);
/// the picker leaves it `None` because a new session has no source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Modal {
    /// The bordered box title (rendered padded with a space either side).
    pub title: String,
    /// The prompt line drawn above the choices.
    pub message: String,
    /// The layout — and, with it, the key map — this modal renders in.
    pub layout: ModalLayout,
    /// The selectable choices in display order.
    pub choices: Vec<ModalChoice>,
    /// The highlighted choice, an index into `choices` (wraps via `rem_euclid`).
    pub selected: usize,
    /// The session a session-addressed action (Attach/Fork/delete) targets;
    /// `None` for the picker, which starts a fresh session with no target.
    pub session_id: Option<String>,
}

impl Modal {
    /// The action of the highlighted choice, or `None` if `selected` somehow
    /// points past `choices` — the safe fallback (confirm treats it as a no-op)
    /// rather than a panic. `selected` is kept in range by [`App::cycle_modal`] and
    /// seeded in range by every constructor, so the `None` arm is purely defensive
    /// (the folded-in equivalent of the picker's old `checked_sub` launch-bare
    /// fallback: an out-of-range highlight never launches or panics).
    #[must_use]
    pub fn selected_action(&self) -> Option<&ModalAction> {
        self.choices.get(self.selected).map(|c| &c.action)
    }
}

/// Choose the picker row to pre-highlight when it opens.
///
/// Returns the row matching `last` (offset by 1 for the leading default entry),
/// or `0` (the default "no agent" row) when `last` is `None` or no longer among
/// `agents` (its defining file was removed since the last pick). Pure so the
/// "pick default vs. last" decision is unit-tested; keeps `Ctrl-N` then `Enter`
/// a one-keystroke fast path onto whatever was chosen last.
#[must_use]
pub fn pick_default_index(last: Option<&str>, agents: &[DefinedAgent]) -> usize {
    match last {
        Some(name) => agents
            .iter()
            .position(|a| a.name == name)
            .map_or(0, |i| i + 1),
        None => 0,
    }
}

/// The folder-scoping predicate: is `session` in `scope`?
///
/// [`Scope::All`] always matches. [`Scope::CurrentFolder`] matches only when the
/// session's resolved `cwd` is byte-equal to `launch` — an EXACT canonical match
/// (design decision: precise, a repo's other worktree folders do not appear).
/// [`Scope::Project`] matches on EITHER of two independent answers to "is this
/// folder part of my project", and needs both:
///
/// * MEMBERSHIP in `worktrees`, the launch project's LIVE worktree roots as git
///   reports them. Authoritative, and the only answer that can relate two
///   folders no path rule could — a worktree parked anywhere on the filesystem.
/// * A shared REPO ROOT ([`crate::worktrees::project_root`]). Weaker, but it
///   answers for a worktree that has been REMOVED, which git structurally
///   cannot: `git worktree list` reports what exists NOW, so a deleted
///   worktree's sessions match no live root and were reachable only in the all
///   scope. On this repo's own store that was a third of the project's history.
///
/// Roots are compared, never [`crate::store::group::repo_of`] LABELS: that
/// function spells a plain checkout `<base>` and a worktree `<parent>/<base>`,
/// so a repo and its own worktree carry different labels and a label comparison
/// is silently wrong exactly where it matters most.
///
/// `launch` MUST already be resolved via [`resolve_dir`], and so must every root
/// in `worktrees` (which [`crate::worktrees::resolve`] guarantees) — every side
/// of every comparison is canonicalized by the same function or membership
/// silently misses across symlinks. `session.cwd` is passed RAW to
/// `project_root`, which owns the derive-then-canonicalize order for the deleted
/// case; see its docs.
///
/// The root arm has NO git dependency, so an EMPTY `worktrees` — "could not
/// resolve", never "nothing matches" — still scopes the project to its whole
/// repo root rather than collapsing to the launch folder. That is a change of
/// the old fail-soft posture, and a deliberate one: the folder the user launched
/// in was never the honest answer to "show me this project", only the answer
/// available without git.
///
/// PURE: the worktree set is a parameter rather than something this reaches for,
/// so the predicate never resolves git and stays testable from a seeded set. It
/// does canonicalize, which is why it runs on reload / scope-toggle only and
/// never per keystroke (`App::recompute_scope`).
#[must_use]
pub fn in_scope(scope: Scope, session: &Session, launch: &Path, worktrees: &WorktreeSet) -> bool {
    match scope {
        Scope::All => true,
        Scope::CurrentFolder => resolve_dir(&session.cwd) == launch,
        Scope::Project => {
            worktrees.contains(&resolve_dir(&session.cwd))
                || project_root(&session.cwd) == project_root(launch)
        }
    }
}

/// The three numbers the header's counter is built from — ALL THREE COUNTED IN
/// LINEAGES (conversations), never in session files.
///
/// The unit is the whole point. A fork lineage is ONE unit everywhere on this
/// line: head plus the members it folds away counts once in `visible`, once in
/// `total`, and — when every one of them is hidden — once in `hidden`. The
/// counter used to mix units, taking its numerator from the post-fold display
/// list (already lineages) and its denominator from the population's session
/// FILES, and read `115 / 146` on a board that could never draw more than 115
/// rows.
///
/// Invariants, all pinned by tests:
///
/// - `total + hidden` == the number of lineages in [`App::population`],
///   whatever the scope and whatever is hidden. (The replacement for the old
///   `total + hidden == population.len()`, which counted files.)
/// - With an EMPTY query and show-hidden off, `visible == total` in
///   [`Scope::Project`] and [`Scope::All`] — the board is drawing exactly the
///   population it counts.
/// - [`Scope::CurrentFolder`] is the deliberate exception: `visible` counts the
///   folder's lineages while `total` counts the PROJECT's, and the gap is the
///   feature (see [`App::population`]).
/// - Expanding or collapsing a lineage moves NEITHER number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionCounts {
    /// How many lineages the board is DRAWING — the NUMERATOR of
    /// `N / M sessions`, and the one number a query moves.
    pub visible: usize,
    /// How many lineages the board could show — the DENOMINATOR. See
    /// [`App::population`] for which sessions those are grouped from.
    pub total: usize,
    /// How many lineages of that population the user has SOFT-HIDDEN ENTIRELY,
    /// and which `total` therefore leaves out. A PARTIALLY hidden lineage still
    /// draws a row, so it counts in `total` (and in `visible`), never here.
    /// Always `0` while [`show_hidden`](App::show_hidden) is on, because the
    /// rows are back on the board and are counted INSIDE `total` there —
    /// reporting them again would count visible rows twice.
    pub hidden: usize,
}

/// Count the header's three numbers over ONE grouping rule, so the two sides of
/// `N / M` can never be assembled from different units at a call site. That
/// separation is exactly what produced the `115 / 146` the type's doc describes,
/// which is why the numerator is computed HERE and not read off `filtered.len()`
/// by the renderer.
///
/// `population` is [`App::population`]: the counted set, ALREADY grouped into
/// lineages. `filtered` is the display list — post-fold, so a collapsed lineage
/// is one entry and an expanded one is its head plus its children — which is why
/// the numerator re-groups it instead of taking its length.
///
/// A lineage is HIDDEN only when EVERY member is in `hidden_ids`. Hiding flips a
/// whole family at once in practice (`App::toggle_hidden_selected` ->
/// `lineage_member_ids`), but the strict test is what keeps a partially hidden
/// lineage — one member hidden, another still drawing — counted as the visible
/// row it is. `all` over an empty group would answer `true`; it cannot arise,
/// because [`lineage::group_members`] only ever emits non-empty groups.
///
/// Pure so the arithmetic is assertable without an [`App`], a store or a
/// terminal, and cheap BY CONSTRUCTION: it walks sets someone else already
/// decided and asks `hidden_ids` a hash question per member, never a path one.
/// That is what lets a hide/un-hide or a show-hidden flip re-derive the counter
/// on the spot, with none of the canonicalization [`App::recompute_scope`] pays
/// for the population itself.
#[must_use]
fn count_lineages(
    sessions: &[Session],
    population: &[Vec<usize>],
    filtered: &[usize],
    hidden_ids: &HashSet<String>,
    show_hidden: bool,
) -> SessionCounts {
    let visible = lineage::group_members(sessions, filtered).len();
    if show_hidden {
        return SessionCounts {
            visible,
            total: population.len(),
            hidden: 0,
        };
    }
    let hidden = population
        .iter()
        .filter(|members| {
            members
                .iter()
                .all(|&i| hidden_ids.contains(&sessions[i].session_id))
        })
        .count();
    SessionCounts {
        visible,
        total: population.len() - hidden,
        hidden,
    }
}

/// The session indices in `filtered` that are EXPANDED lineage members standing
/// beneath a visible head — i.e. every member of a lineage that still shows more
/// than one row, except that lineage's own head.
///
/// Derived from what is VISIBLE rather than from `expanded`, and the two agree by
/// construction: [`lineage::fold`] leaves a collapsed lineage exactly one visible
/// member, so a lineage with two or more visible members is an open one. Reading
/// visibility keeps this honest even if the caller hands over a `filtered` built
/// some other way — a row is a child iff its head is really on screen with it.
///
/// The head is [`lineage::head_of`], never "whichever member came first", so the
/// D1 rule lives in exactly one place and this cannot drift from the head
/// [`lineage::fold`] chose. Pure so the child marking is unit-testable on its own.
///
/// Groups through [`lineage::group_members`], the SAME partition
/// [`count_lineages`] counts — so "this row is a child" and "this row is not its
/// own conversation" are one decision, and the header can never disagree with
/// the indent about how many lineages are on screen. Rootless sessions come back
/// as lineages of one there and are dropped by the `len() > 1` test below,
/// exactly as a keyed map that never held them would.
fn child_indices(sessions: &[Session], filtered: &[usize]) -> HashSet<usize> {
    lineage::group_members(sessions, filtered)
        .into_iter()
        .filter(|group| group.len() > 1)
        .flat_map(|group| {
            let head = lineage::head_of(sessions, &group);
            group.into_iter().filter(move |&i| i != head)
        })
        .collect()
}

/// The `(repo, branch)` group a session renders under.
///
/// The branch half is always the session's own. The repo half is normally
/// [`Session::repo`] too — but `project_head`, when `Some`, REPLACES it for
/// every session, which is what makes one project draw as one head.
///
/// It has to, because [`Session::repo`] is not a project identity: it is
/// [`crate::store::group::repo_of`]'s label for the session's own FOLDER,
/// assigned once at parse time, and that function deliberately renders a plain
/// checkout as `<base>` while rendering a worktree as `<parent>/<base>`. The
/// main checkout and its worktrees therefore carry different labels even though
/// they are one project, and heading groups by that field splits the project in
/// two on screen. Only [`Scope::Project`] can fix it: there, and only there, the
/// board KNOWS which project it is showing, because git resolved the membership
/// set — and membership is already restricted to that one project's worktrees,
/// so an override can only ever unify heads that belong together, never merge
/// two projects.
///
/// The row builder and the display ordering share this one function on purpose:
/// [`App::order_filtered`] sorts same-key rows together and
/// [`build_rows`] emits one head per contiguous run of equal keys, so a second
/// notion of "the same group" would let a group's rows scatter and re-emit its
/// head.
#[must_use]
fn group_key(session: &Session, project_head: Option<&str>) -> (String, String) {
    (
        project_head.unwrap_or(&session.repo).to_string(),
        session.branch_display().to_string(),
    )
}

/// Flatten `filtered` (indices into `sessions`, in scope-aware display order)
/// into rows for the list.
///
/// In [`Scope::All`] and [`Scope::Project`] this emits a group head the first
/// time a repo->branch group appears, then that group's session rows. Because
/// `filtered` is kept in display order (group-most-recent-desc, then
/// timestamp-desc within a group) same-group rows are contiguous, so each group
/// yields exactly ONE head. [`Scope::CurrentFolder`] suppresses heads entirely
/// and yields a flat, timestamp-desc list — and it is the ONLY scope that does,
/// because it is the only one that cannot span more than one folder. Where rows
/// come from several folders, the head is what tells them apart.
///
/// `scope` is the scope the user selected, unconditionally. There used to be a
/// `render_scope` translation here, on the premise that an UNRESOLVED project
/// scope matched a single folder and therefore had to draw flat like
/// [`Scope::CurrentFolder`]. That premise is gone: [`in_scope`]'s repo-root arm
/// has no git dependency, so an unresolved project scope still spans every
/// folder of the repo and still needs its heads.
///
/// Folding cannot disturb that one-head-per-group invariant, because a lineage is
/// keyed by `(repo, branch, root)` (D4): every member of one lineage shares the
/// group its head sits in, so hiding members or drawing them back can only ever
/// add or remove rows INSIDE a single group's contiguous run.
///
/// `hidden` is [`lineage::fold`]'s head-index -> hidden-count map; an index absent
/// from it hides nothing and renders as a plain row.
///
/// `project_head` overrides the repo half of every group key — see
/// [`group_key`]. Pass [`App::project_head`]'s answer: it is `Some` for EVERY
/// row in the project scope — resolved worktree set or not, for the same reason
/// the paragraph above gives — and `None` in every other scope. So `Some` and
/// "draws grouped" arrive together, and there is no state in which this emits
/// project heads with no project label to head them by.
#[must_use]
pub fn build_rows(
    sessions: &[Session],
    filtered: &[usize],
    scope: Scope,
    hidden: &HashMap<usize, usize>,
    project_head: Option<&str>,
) -> Vec<Row> {
    let children = child_indices(sessions, filtered);
    // One place builds a session row, so the flat and grouped lists can never
    // disagree about a row's marker or indent.
    let session_row = |i: usize| Row::Session {
        index: i,
        hidden: hidden.get(&i).copied().unwrap_or(0),
        child: children.contains(&i),
    };

    let mut rows = Vec::with_capacity(filtered.len() + 8);
    // Current-folder scope is a flat, head-less list — and it ALONE, which is why
    // this asks for that one scope rather than excluding `All`: a scope that can
    // span folders (`Project`, `All`) must keep its heads.
    if scope == Scope::CurrentFolder {
        rows.extend(filtered.iter().map(|&i| session_row(i)));
        return rows;
    }
    let mut current: Option<(String, String)> = None;
    for &i in filtered {
        let session = &sessions[i];
        let key = group_key(session, project_head);
        if current.as_ref() != Some(&key) {
            rows.push(Row::Group {
                repo: key.0.clone(),
                branch: key.1.clone(),
            });
            current = Some(key);
        }
        rows.push(session_row(i));
    }
    rows
}

/// Clamp a requested list-pane width into `[MIN_PANE_WIDTH, body_width -
/// MIN_PANE_WIDTH]`, so the preview pane always keeps at least
/// `MIN_PANE_WIDTH` columns too. A `body_width` too narrow to fit both
/// minimums degrades to an even half-split rather than inverting the panes or
/// underflowing. Pure so a fresh drag ([`App::drag_split_to`]) and every
/// render's re-clamp ([`resolve_list_width`], called from
/// `tui::view::render_body`) share the exact same rule.
#[must_use]
pub fn clamp_list_width(requested: u16, body_width: u16) -> u16 {
    if body_width < MIN_PANE_WIDTH * 2 {
        return body_width / 2;
    }
    requested.clamp(MIN_PANE_WIDTH, body_width - MIN_PANE_WIDTH)
}

/// Resolve the list pane's width in columns for a body `body_width` columns
/// wide: the persisted drag width when one exists (re-clamped against the
/// CURRENT `body_width`, so a stale width from a wider terminal never
/// survives a resize into a degenerate layout), or the historical 48% default
/// when the user has never dragged the splitter. Called every render
/// (`tui::view::render_body`) rather than trusting a stored value on its own.
#[must_use]
pub fn resolve_list_width(list_width: Option<u16>, body_width: u16) -> u16 {
    let requested =
        list_width.unwrap_or_else(|| (u32::from(body_width) * DEFAULT_LIST_PERCENT / 100) as u16);
    clamp_list_width(requested, body_width)
}

/// How the board asks claude which sessions are LIVE right now.
///
/// Boxed so it is injectable at the one seam that matters — see
/// [`App::live_probe`]. Returns claude's ACTIVE agents keyed by full
/// `sessionId`; an empty map means "none, or we could not ask", which both
/// resolve to the same fail-soft answer: not live, let claude decide.
///
/// It yields the RECORDS, not bare ids, so ONE probe answers both hand-off
/// questions — *is it live* (membership) and *what is its attach job `id`* (the
/// matched record's own). See [`crate::agents::live_agents`].
type LiveProbe = Box<dyn Fn() -> HashMap<String, ReportedAgent>>;

/// The probe a fresh [`App`] starts with: the real shell-out to claude.
#[cfg(not(test))]
fn default_live_probe() -> HashMap<String, ReportedAgent> {
    crate::agents::live_agents()
}

/// The probe a fresh [`App`] starts with UNDER TEST: none. Panics.
///
/// Two things this makes impossible, both of which this seam exists to prevent:
///
/// 1. **A test spawning `claude`.** The suite never spawns it (PATTERNS.md), and
///    the real default would — the gate calls it on every Enter, and the Attach
///    hand-off calls it again.
/// 2. **A test passing for the wrong reason.** Defaulting to an empty map would
///    silently mean "nothing is live", so a test that forgot to seed would
///    plain-resume and look correct while asserting nothing about the gate. This
///    forces every test that reaches a liveness decision to STATE what claude
///    reports, via [`App::set_live_probe`].
#[cfg(test)]
fn default_live_probe() -> HashMap<String, ReportedAgent> {
    panic!(
        "a test reached the liveness gate without seeding the live set — call \
         App::set_live_probe to state what claude reports (never spawn `claude`)"
    )
}

/// How the board asks git which worktrees the launch project has right now.
///
/// Boxed for the same one reason [`LiveProbe`] is — see [`App::worktree_probe`]:
/// the answer comes from a subprocess, and a test must be able to STATE it
/// instead of running one. Takes the launch dir rather than closing over it so
/// the reload path can re-ask with the same probe.
type WorktreeProbe = Box<dyn Fn(&Path) -> WorktreeSet>;

/// The probe a fresh [`App`] starts with: the real shell-out to git.
#[cfg(not(test))]
fn default_worktree_probe(launch_dir: &Path) -> WorktreeSet {
    crate::worktrees::resolve(launch_dir)
}

/// The probe a fresh [`App`] starts with UNDER TEST: none resolved, ever.
///
/// Returns the EMPTY set rather than panicking like [`default_live_probe`],
/// because the two seams guard opposite failures. A liveness answer that
/// defaulted to "nothing is live" would let a test pass for the wrong reason, so
/// that one demands a decision. An empty worktree set demands nothing: it is the
/// documented "could not resolve" answer, under which [`in_scope`] carries
/// [`Scope::Project`] on its git-free repo-root arm — so the ~40 [`App::new`]
/// call sites that say nothing about worktrees still get the answer a user with
/// no `git` on `PATH` would, and NONE of them spawns git.
///
/// A test that means to exercise the cross-worktree scope seeds the set through
/// [`App::set_worktree_probe`].
#[cfg(test)]
fn default_worktree_probe(_launch_dir: &Path) -> WorktreeSet {
    WorktreeSet::empty()
}

/// One session's preview as rendered at [`App::preview_width`] — the styled text
/// and its link regions, plus everything else that is a function of that same
/// (session, width) pair.
///
/// The derived count lives INSIDE the entry rather than in a second map on the side
/// so it cannot outlive what it describes: the entry is the unit that is inserted and
/// dropped, so every existing invalidation (a width change, a reload) already carries
/// the count with it and no future one can remember half of it.
struct CachedPreview {
    /// The styled transcript + clickable link regions from one `preview::render`.
    rendered: preview::RenderedPreview,
    /// Screen rows `rendered.text` occupies once WORD-wrapped at
    /// [`App::preview_width`] — measured by `view::wrapped_text_rows`, i.e. by the
    /// same wrapper that paints the pane, never by a model of our own. Cached
    /// because that wrapper walks the whole transcript and the pane redraws several
    /// times a second.
    wrapped_rows: usize,
}

/// The full TUI state model.
///
/// Selection is stored as a stable `session_id` (never a list index) so it
/// survives an autorefresh reload; `scroll` is likewise preserved across
/// reloads and only clamped to bounds.
pub struct App {
    /// Every loaded session, in the store's stable repo->branch->timestamp
    /// order.
    pub sessions: Vec<Session>,
    /// Indices into [`sessions`](Self::sessions) that pass scope+query, kept in
    /// scope-aware DISPLAY order: flat timestamp-desc for the current-folder
    /// scope; group-most-recent-desc then timestamp-desc for the project and the
    /// all scope. Whether the worktree set resolved does not enter into it —
    /// [`order_filtered`](Self::order_filtered) owns that rule and argues why.
    pub filtered: Vec<usize>,
    /// Stable id of the selected session (survives reload); `None` when the
    /// filtered list is empty.
    pub selected: Option<String>,
    /// First visible list row (scroll offset); preserved across reloads.
    pub scroll: usize,
    /// Live search query text.
    pub query: String,
    /// Name-only vs. name+content search (mirrors the search index mode).
    pub search_mode: SearchMode,
    /// Which sessions the board is showing: current folder, project, or all.
    pub scope: Scope,
    /// Whether [`Scope::All`] is on the `Ctrl-A` cycle at all — the second
    /// meaning of the `--all`/`-a` launch flag
    /// ([`crate::cli::Args::all_scope_enabled`]), which is why it is state
    /// rather than a fact about [`scope`](Self::scope): a board launched
    /// `-a -p` starts in the project scope and can still reach the whole store.
    ///
    /// Read ONLY where the user asks "what does this key do next" —
    /// [`Scope::toggled`] and the empty list's advice — and passed to both as a
    /// parameter, never consulted from inside them. It filters nothing, so
    /// setting it after [`App::new`] (as the launch path does) cannot leave the
    /// list disagreeing with it.
    pub all_scope_enabled: bool,
    /// Canonicalized launch directory for the current-folder predicate.
    pub launch_dir: PathBuf,
    /// Names of DEFINED agents (`~/.claude/agents/*.md` + project overrides),
    /// discovered ONCE at construction. Gates the preview's `agent-name` fallback
    /// so a free-form background-job title never renders as a bogus `@handle`
    /// (see [`store::preview::render`]). A DISPLAY-only allowlist, never a launch
    /// gate — kept distinct from the picker's on-demand [`DefinedAgent`] list.
    ///
    /// [`store::preview::render`]: crate::store::preview::render
    pub agent_names: Vec<String>,
    /// Whether the preview pane is visible.
    pub show_preview: bool,
    /// Vertical scroll offset (in WRAPPED rows) of the preview pane. Requested by
    /// the scroll keys and clamped to content bounds by `view::render_preview`,
    /// which writes the clamped value back (mirroring `scroll`/`ListState`).
    pub preview_scroll: u16,
    /// When set, the preview stays pinned to the BOTTOM (newest turn): the next
    /// render resolves the offset to `max_offset`. Set on any selection change
    /// and when the preview is toggled on; cleared by an explicit up/page/Home
    /// scroll; re-enabled by `End`.
    pub preview_follow_bottom: bool,
    /// Last inner viewport height of the preview (rows), written back by the view
    /// so page / quarter-page scrolling can size a page without knowing the layout.
    pub preview_viewport_h: u16,
    /// Last-rendered list-pane rectangle, written back by the view each frame so
    /// a mouse wheel can be hit-tested to the list without a terminal. Empty
    /// (`Rect::default`) until the first render.
    pub list_rect: Rect,
    /// Last-rendered preview-pane rectangle (mouse hit-testing). Set to an EMPTY
    /// rect when the preview is hidden so it never matches a hit-test.
    pub preview_rect: Rect,
    /// List pane width in columns, set the first time the user drags the
    /// splitter between the list and preview panes. `None` until the first
    /// drag: the view falls back to the historical 48% split
    /// ([`resolve_list_width`]). Re-clamped against the CURRENT body width on
    /// every render, so a stale width from a wider terminal never survives a
    /// resize into a degenerate layout.
    pub list_width: Option<u16>,
    /// Transient board status (e.g. a resume refusal for a deleted worktree).
    /// Rendered on the help line and cleared on the next actionable keypress, OR
    /// when its sibling `status_ttl` counts down to zero.
    ///
    /// `status` and `status_ttl` are written only by `set_status`,
    /// `set_status_transient`, `clear_status`, and `tick_status`, so they cannot
    /// drift apart — the same invariant that keeps `compose`/`draft` paired.
    pub status: Option<String>,
    /// Remaining ticks for a **transient** status, or `None` for a sticky one.
    ///
    /// `None` means "cleared only by the next actionable keypress" (today's
    /// behaviour for refusals and failures). `Some(n)` decrements on every
    /// `AppEvent::Tick` and clears `status` when it reaches zero. Paired with
    /// `status` by the single-writer invariant above.
    pub status_ttl: Option<u16>,
    /// Count of `AppEvent::Tick`s since launch, advanced at the `watch::TICK`
    /// cadence and wrapping rather than overflowing.
    ///
    /// The board's only clock, and it exists for exactly one reason: it phases
    /// the live-badge pulse via [`super::view::blink_visible`]. The pulse is
    /// APP-driven because the terminal-driven alternative does not work — most
    /// modern terminals ignore the ANSI blink attribute, so a `SLOW_BLINK` dot
    /// renders steady. This reuses the redraw cadence that already exists; it
    /// adds no tick, thread, or event source of its own.
    pub tick: u64,
    /// Sessions claude REPORTED as agents, keyed by full `session_id` (joined
    /// from `claude agents --json --all`). Refreshed OFF the UI thread; drives
    /// the badges and the preview status banner. Empty when the signal is
    /// unavailable.
    ///
    /// A DISPLAY signal only, and that is the WHOLE of its authority: `--all`
    /// includes agents that reported completion, so membership here does not mean
    /// the session is running. **Nothing may hand off on it** — not the resume
    /// gate (ask [`is_live_now`](Self::is_live_now)) and not the Attach job `id`
    /// either (ask [`live_agent_now`](Self::live_agent_now)). Both once read this
    /// map; both now re-ask claude at the moment they act, because a hand-off
    /// decided from a ~1.3s-stale snapshot is the bug shape this seam exists to
    /// prevent. It renders badges and the banner, nothing more.
    pub reported_agents: HashMap<String, ReportedAgent>,
    /// How [`live_agent_now`](Self::live_agent_now) and
    /// [`is_live_now`](Self::is_live_now) ask claude which sessions are live.
    /// Defaults to the real [`crate::agents::live_agents`] shell-out.
    ///
    /// A seam, not a strategy: it exists so tests seed the live set directly
    /// rather than spawning `claude` (which the suite never does), in the spirit
    /// of `resume::build_argv`. Production swaps it exactly never.
    live_probe: LiveProbe,
    /// The launch project's live worktree roots — the membership set
    /// [`Scope::Project`] scopes by, CACHED so the scope predicate never runs
    /// git.
    ///
    /// Resolved ONCE in [`App::new`] and refreshed on reload, never on a
    /// keystroke: `toggle_scope` reaches this scope through a key, and a
    /// subprocess on a key press is exactly the blocking work that may not sit on
    /// the UI thread. Empty means "could not resolve" (see [`WorktreeSet`]): the
    /// live-membership arm then contributes nothing and [`Scope::Project`] rests
    /// on [`in_scope`]'s repo-root arm alone, which needs no git and still spans
    /// the project.
    pub worktrees: WorktreeSet,
    /// How [`worktrees`](Self::worktrees) is (re-)resolved. Defaults to the real
    /// [`crate::worktrees::resolve`] shell-out.
    ///
    /// A seam, not a strategy, exactly like [`live_probe`](Self::live_probe):
    /// tests seed a worktree set rather than spawning git (which the suite never
    /// does). Production swaps it exactly never.
    worktree_probe: WorktreeProbe,
    /// The open modal overlay, if any. `Some` while a titled prompt owns the
    /// keyboard — the running-session Attach/Fork/Cancel choice, or the
    /// new-session agent picker (`Ctrl-N` when defined agents exist). The two are
    /// now one `Option<Modal>`, so their mutual exclusion is structural rather
    /// than conventional.
    pub modal: Option<Modal>,
    /// The open compose editor, if any. `Some` while the compose modal owns the
    /// keyboard — for EITHER draft: a quick reply (`Ctrl-R` on an idle session) or
    /// a new background agent (`Ctrl-N`), told apart by its
    /// [`ComposeTarget`](super::compose::ComposeTarget). Its type
    /// ([`super::compose::ComposeState`]) is the ONLY place outside
    /// [`super::compose`] that touches `ratatui_textarea`, the way `search`
    /// confines nucleo.
    pub compose: Option<super::compose::ComposeState>,
    /// The open NEW-SESSION draft, if any — see [`NewSessionDraft`].
    ///
    /// Independent of [`compose`](Self::compose) ON PURPOSE: the view asks THIS
    /// whether the preview pane shows a draft card, and never inspects the compose
    /// target to decide. The two are written only by
    /// [`open_compose`](Self::open_compose) / [`close_compose`](Self::close_compose)
    /// / [`dispatch_draft`](Self::dispatch_draft), so they cannot drift apart.
    pub draft: Option<NewSessionDraft>,
    /// The open "stop the waiting agent?" confirmation, if any. `Some` while that
    /// modal owns the keyboard (`Ctrl-R` on a `needs input` background agent, before
    /// compose opens).
    pub pending_stop: Option<PendingStop>,
    /// The open "stop this agent?" interrupt confirmation, if any. `Some` while that
    /// modal owns the keyboard (`Ctrl-K` on a live, not-yet-finished agent). Distinct
    /// from [`pending_stop`](Self::pending_stop): this resolves into a bare
    /// `claude stop`, not a reply.
    pub pending_interrupt: Option<PendingInterrupt>,
    /// The quick-reply send that is IN FLIGHT (dispatched, not yet finished), or
    /// `None`. Drives the optimistic in-preview echo of the message plus the
    /// animated `cooking…` placeholder so the reply feels instant; set when the
    /// send is handed off and cleared when its `AppEvent::SendFinished` lands.
    /// See [`Sending`].
    pub sending: Option<Sending>,
    /// The `claude stop` interrupt that is IN FLIGHT (dispatched, not yet finished),
    /// or `None`. Set when the stop is handed off and cleared when its
    /// `AppEvent::InterruptFinished` lands for the same session id.
    /// See [`Interrupting`].
    pub interrupting: Option<Interrupting>,
    /// The id the NEXT background launch is stamped with, handed out by
    /// [`dispatch_draft`](Self::dispatch_draft).
    ///
    /// Monotonic and board-local: it exists only so a completion event can be told
    /// apart from a later one, which is what
    /// [`launching_draft`](Self::launching_draft) checks. A brand-new agent has no
    /// `sessionId` to key by (that is the whole reason
    /// [`crate::send::BgLaunchRequest`] is thinner than its send sibling), so the
    /// board mints its own identity for the round trip and nothing outside it ever
    /// sees this number.
    next_bg_launch_id: u64,
    /// The agent chosen for the most recent new session (`None` = default / no
    /// agent). In-memory ONLY — never persisted to disk — so the NEXT `Ctrl-N`
    /// pre-highlights it for a one-keystroke repeat.
    last_new_agent: Option<String>,
    /// Whether the list/preview splitter is currently being dragged (mouse
    /// button down on the seam). Private: only the drag methods below need
    /// it, mirroring `scoped`/`preview_cache`/`index`.
    dragging_split: bool,
    /// Indices (into `sessions`) that pass the scope predicate, cached so a
    /// per-keystroke query re-filter never re-canonicalizes paths.
    scoped: Vec<usize>,
    /// The population the header COUNTS, GROUPED INTO LINEAGES: one entry per
    /// conversation, holding that conversation's indices into `sessions`. Its
    /// LENGTH is the denominator of `N / M sessions`, and its entries are what
    /// the `· N hidden` segment is taken from.
    ///
    /// Grouped, not flat, because the counter's unit is the CONVERSATION: a
    /// folded fork lineage draws one row and must therefore count once on both
    /// sides of the `/`. A flat index set makes `len()` a file count, and that
    /// is the arithmetic that read `115 / 146` on a board of 115 rows.
    ///
    /// Deliberately WIDER than `scoped` in the default scope. It holds the whole
    /// launch PROJECT — [`Scope::Project`] membership — even while the board is
    /// narrowed to one folder, because the old denominator was `sessions.len()`:
    /// the whole store, which in a worktree advertises hundreds of rows the
    /// board will never draw. `5 / 30` instead states how much the `Ctrl-A`
    /// widen would reveal. [`Scope::All`] is the ONE exception: there the board
    /// is not about a project, so the population stays the whole store and that
    /// header reads as it always did.
    ///
    /// Cached for the same reason `scoped` is, and it is the harder constraint:
    /// deciding project membership canonicalizes every `cwd`, so it is computed
    /// ONLY in [`recompute_scope`](Self::recompute_scope) — never per render,
    /// never per keystroke. The GROUPING is cached in the same place for a
    /// weaker but real reason, argued at that call site: it is pure (no path
    /// work, so it is free to live anywhere) but it allocates a three-`String`
    /// key per member over a set that can be the whole store, which is not work
    /// to repeat on every frame for a number that only a reload or a scope
    /// toggle can change. The hidden SPLIT is deliberately NOT cached beside
    /// either: [`session_counts`](Self::session_counts) derives that from
    /// `hidden_ids` on demand, so a hide, an un-hide or a show-hidden flip stays
    /// truthful without re-resolving a single path.
    population: Vec<Vec<usize>>,
    /// The fork lineages the user has EXPANDED. EMPTY IS THE DEFAULT, and it
    /// means every lineage is folded: a background fork shows one row until the
    /// user opens it.
    ///
    /// Keyed by the content-derived [`LineageKey`] and NEVER by a list index —
    /// the same discipline `selected` follows, and for the same reason. This set
    /// outlives `apply_sessions`, which replaces `sessions` wholesale and can
    /// reorder every index in it; an index-keyed set would silently start naming
    /// a DIFFERENT lineage on the next autorefresh. The key is derived from what
    /// is inside the files, so it survives any reload that keeps the lineage.
    expanded: HashSet<LineageKey>,
    /// Head session index -> how many lineage members that head is currently
    /// standing in for, rebuilt from scratch by every
    /// [`recompute_filtered`](Self::recompute_filtered) (see [`lineage::fold`]).
    /// Feeds the head's `(+N)` marker.
    ///
    /// Index-keyed is safe here precisely BECAUSE it is derived rather than
    /// persisted: it is discarded and rebuilt alongside the `filtered` indices it
    /// addresses, so no reload can ever leave it pointing at the wrong session.
    /// Contrast `expanded` above, which must cross reloads and therefore may not
    /// be.
    hidden: HashMap<usize, usize>,
    /// The PERSISTED set of user-hidden session ids — snapback's OWN visibility
    /// preference, loaded once at construction from
    /// [`config::state_dir`] and re-saved
    /// on every hide/un-hide. When [`show_hidden`](Self::show_hidden) is off,
    /// [`recompute_filtered`](Self::recompute_filtered) drops these ids so their
    /// rows leave the board entirely.
    ///
    /// DISTINCT from the fold's [`hidden`](Self::hidden) map directly above, and
    /// named so the two never blur: the fold's is a DERIVED head-index ->
    /// folded-member-COUNT map, rebuilt every recompute to feed the `(+N)` marker;
    /// THIS is a user-chosen, cross-restart set of `session_id`s. Different
    /// vocabulary on purpose — `hidden_ids` / "hide" here vs. the fold's `hidden`
    /// / "fold".
    pub hidden_ids: HashSet<String>,
    /// Whether user-hidden sessions are currently revealed inline (drawn dimmed
    /// with a `[hidden]` marker). `false` by default: hidden rows stay off the
    /// board until the show-hidden toggle brings them back for review or un-hide.
    pub show_hidden: bool,
    /// Whether a `Ctrl-X` leader chord is pending — the moment between the leader
    /// keypress and its follow-up (`x` hide, `d` hard-delete, `h` show-hidden,
    /// anything else cancels). While `true` the view draws the which-key hint and
    /// [`handle_event`](crate::tui::update) routes the NEXT key through the pure
    /// `chord_key` machine BEFORE normal key handling, so a printable follow-up
    /// never leaks into the search query.
    ///
    /// A plain marker rather than a data-carrying enum because there is exactly ONE
    /// leader (`Ctrl-X`) with no per-chord state; folded into
    /// [`overlay_active`](Self::overlay_active) so mouse actions stay gated while
    /// it is pending (PATTERNS §10).
    pub pending_chord: bool,
    /// Readable, markdown-styled transcript preview, keyed by `session_id`.
    ///
    /// The rendered layout (GFM tables shrink-to-fit) depends on the preview
    /// pane's inner width, so the cache is scoped to a single width tracked in
    /// [`preview_width`](Self::preview_width): a width change CLEARS it rather
    /// than keying every entry by `(id, width)`. This keeps the cache to one
    /// entry per session and mirrors the reload-clear in `apply_sessions`;
    /// re-render on resize is cheap because only the selected session is ever
    /// rendered. Each entry carries the styled `Text`, its clickable link
    /// regions (see [`preview::RenderedPreview`]) and the transcript's wrapped
    /// row count (see [`CachedPreview`]); all three are produced from one pass at
    /// a fixed width, so a region's columns and a row count always match the
    /// drawn text.
    preview_cache: HashMap<String, CachedPreview>,
    /// Inner content width the `preview_cache` entries were rendered for. A
    /// change invalidates the whole cache (see `preview_cache`). `None` until
    /// the first preview render.
    preview_width: Option<u16>,
    /// Live substring index over `sessions` (isolated in [`crate::search`],
    /// which also confines every `memchr`/`nucleo` call).
    index: SearchIndex,
}

impl App {
    /// Build an app over `sessions` with the given `scope` and canonicalized
    /// `launch_dir`. Computes the initial filter and selects the first row.
    #[must_use]
    pub fn new(sessions: Vec<Session>, scope: Scope, launch_dir: PathBuf) -> Self {
        let index = SearchIndex::build(&sessions);
        let search_mode = index.mode();
        // Discover DEFINED agents ONCE (a one-shot FS scan, like the picker's) so
        // the preview can validate the `agent-name` fallback without re-reading disk
        // per render.
        let agent_names: Vec<String> = defined_agents::discover_agents(&launch_dir)
            .into_iter()
            .map(|a| a.name)
            .collect();
        let mut app = App {
            sessions,
            filtered: Vec::new(),
            selected: None,
            scroll: 0,
            query: String::new(),
            search_mode,
            scope,
            // OFF unless the launch flag says otherwise (`crate::run` sets it
            // from `cli::Args`), so the widest scope stays off the key for every
            // board that did not ask for it.
            all_scope_enabled: false,
            launch_dir,
            agent_names,
            show_preview: true,
            preview_scroll: 0,
            preview_follow_bottom: true,
            preview_viewport_h: 0,
            list_rect: Rect::default(),
            preview_rect: Rect::default(),
            list_width: None,
            status: None,
            status_ttl: None,
            tick: 0,
            reported_agents: HashMap::new(),
            live_probe: Box::new(default_live_probe),
            // Seeded a few lines down, once the probe it is resolved by is in
            // place.
            worktrees: WorktreeSet::empty(),
            worktree_probe: Box::new(default_worktree_probe),
            modal: None,
            pending_interrupt: None,
            compose: None,
            draft: None,
            pending_stop: None,
            sending: None,
            interrupting: None,
            next_bg_launch_id: 0,
            last_new_agent: None,
            dragging_split: false,
            scoped: Vec::new(),
            population: Vec::new(),
            expanded: HashSet::new(),
            hidden: HashMap::new(),
            // Load the persisted hidden set ONCE at startup. Resolve the dir here
            // (and again at save time) rather than caching a path, so a
            // `SNAPBACK_CONFIG_DIR` override is honored per call and tests can
            // inject an isolated dir. Fail-soft: a missing/unreadable file is an
            // empty set (nothing hidden yet), never a startup error.
            hidden_ids: hidden::load_hidden(&config::state_dir()),
            show_hidden: false,
            pending_chord: false,
            preview_cache: HashMap::new(),
            preview_width: None,
            index,
        };
        // Resolve the launch project's worktree set ONCE, here, BEFORE the first
        // `recompute_scope`: it is launch context in the same sense `launch_dir`
        // is, so a board started with `Scope::Project` is already scoped on its
        // very first frame instead of one reload later. Going through the probe
        // field (rather than calling the default directly) is what keeps this the
        // SAME resolution path the reload takes.
        app.worktrees = (app.worktree_probe)(&app.launch_dir);
        app.recompute_scope();
        app.recompute_filtered();
        app.select_first();
        app
    }

    // --- selection (by stable id) -----------------------------------------

    /// The selected session, if any.
    #[must_use]
    pub fn selected_session(&self) -> Option<&Session> {
        let id = self.selected.as_ref()?;
        self.sessions.iter().find(|s| &s.session_id == id)
    }

    /// Position of the selected session within [`filtered`](Self::filtered).
    #[must_use]
    pub fn selected_pos(&self) -> Option<usize> {
        let id = self.selected.as_ref()?;
        self.filtered
            .iter()
            .position(|&i| &self.sessions[i].session_id == id)
    }

    /// Index into [`sessions`](Self::sessions) of the selected VISIBLE row.
    ///
    /// Distinct from [`selected_session`](Self::selected_session), which finds the
    /// id anywhere in the store: this resolves through `filtered`, so it is the
    /// index `hidden` and [`lineage::fold`] speak in.
    fn selected_index(&self) -> Option<usize> {
        self.filtered.get(self.selected_pos()?).copied()
    }

    /// The fork lineage of the selected row, or `None` when nothing is selected
    /// or the selection has no derivable lineage (FAIL-SOFT: a session with no
    /// root uuid belongs to none and is never folded).
    fn selected_lineage(&self) -> Option<LineageKey> {
        lineage::lineage_key(&self.sessions[self.selected_index()?])
    }

    /// The VISIBLE members of `key`'s lineage, as indices into
    /// [`sessions`](Self::sessions).
    ///
    /// Gathered from `filtered` — the very list [`lineage::fold`] gathers from —
    /// so a head derived from this can never disagree with the head the fold
    /// keeps.
    fn visible_lineage_members(&self, key: &LineageKey) -> Vec<usize> {
        self.filtered
            .iter()
            .copied()
            .filter(|&i| lineage::lineage_key(&self.sessions[i]).as_ref() == Some(key))
            .collect()
    }

    /// Every session id that hides or exposes TOGETHER with the selection: all
    /// members of its fork lineage, so a folded lineage (its `(+N)` nested forks
    /// included) flips as one unit rather than shedding only its head. Gathered
    /// from the FULL store — not the visible `filtered` — so already-folded or
    /// already-hidden forks are swept in too. A selection with no derivable lineage
    /// (a rootless session) is its own singleton.
    fn lineage_member_ids(&self, selected_id: &str) -> Vec<String> {
        match self.selected_lineage() {
            Some(key) => {
                let members: Vec<String> = self
                    .sessions
                    .iter()
                    .filter(|s| lineage::lineage_key(s).as_ref() == Some(&key))
                    .map(|s| s.session_id.clone())
                    .collect();
                if members.is_empty() {
                    vec![selected_id.to_string()]
                } else {
                    members
                }
            }
            None => vec![selected_id.to_string()],
        }
    }

    /// The single selection setter: assign the selected id and, when it actually
    /// CHANGES, re-anchor the preview to the newest turn (follow-bottom) so a new
    /// session always opens at its most-recent message. A no-op reassignment of
    /// the same id (common on a query keystroke that keeps the row) leaves any
    /// manual preview scroll intact.
    fn set_selected(&mut self, next: Option<String>) {
        if self.selected != next {
            self.selected = next;
            self.preview_follow_bottom = true;
            self.preview_scroll = 0;
        }
    }

    /// Select the session at filtered position `pos` (no-op if out of range).
    fn select_pos(&mut self, pos: usize) {
        if let Some(&i) = self.filtered.get(pos) {
            let id = self.sessions[i].session_id.clone();
            self.set_selected(Some(id));
        }
    }

    /// Select the first filtered row, or clear selection when empty.
    fn select_first(&mut self) {
        let next = self
            .filtered
            .first()
            .map(|&i| self.sessions[i].session_id.clone());
        self.set_selected(next);
    }

    /// Move the selection by `delta` rows, clamped to the filtered bounds.
    pub fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            self.set_selected(None);
            return;
        }
        let cur = self.selected_pos().unwrap_or(0) as isize;
        let last = self.filtered.len() as isize - 1;
        let next = (cur + delta).clamp(0, last) as usize;
        self.select_pos(next);
    }

    /// Move the list selection one mouse-wheel notch (`up = true` toward the
    /// top), reusing the clamped [`move_selection`](Self::move_selection).
    pub fn list_wheel(&mut self, up: bool) {
        let step = if up {
            -LIST_WHEEL_STEP
        } else {
            LIST_WHEEL_STEP
        };
        self.move_selection(step);
    }

    // --- query / mode / scope ---------------------------------------------

    /// Append a character to the query and re-filter (type-to-search).
    pub fn push_query_char(&mut self, c: char) {
        self.query.push(c);
        self.index.set_query(&self.query);
        self.reapply_preserving_selection();
    }

    /// Append a whole STRING to the query and re-filter ONCE — the terminal-paste
    /// sibling of [`push_query_char`](Self::push_query_char).
    ///
    /// A paste arrives as one `Event::Paste`, so it re-filters once for the whole
    /// text rather than once per character; `set_query` rebuilds the pattern and the
    /// per-atom finders on each call, so looping `push_query_char` would pay that
    /// rebuild N times for a single user action. An empty string is a no-op (no
    /// pointless re-filter).
    ///
    /// The caller owns the SHAPE of `text` — the query is a single line, so
    /// `update::flatten_for_query` has already turned any newline into a space
    /// before this is reached.
    pub fn push_query_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.query.push_str(text);
        self.index.set_query(&self.query);
        self.reapply_preserving_selection();
    }

    /// Delete the last query character and re-filter.
    pub fn pop_query_char(&mut self) {
        if self.query.pop().is_some() {
            self.index.set_query(&self.query);
            self.reapply_preserving_selection();
        }
    }

    /// Toggle name-only vs. name+content search and re-filter.
    pub fn toggle_search_mode(&mut self) {
        self.search_mode = self.index.toggle_mode();
        self.reapply_preserving_selection();
    }

    /// Advance one step along [`Scope::toggled`] — current folder <-> project,
    /// or the full three-stop cycle when
    /// [`all_scope_enabled`](Self::all_scope_enabled) — and re-filter
    /// (recomputes the scope membership set, which is what canonicalizes paths).
    pub fn toggle_scope(&mut self) {
        self.scope = self.scope.toggled(self.all_scope_enabled);
        self.recompute_scope();
        self.reapply_preserving_selection();
    }

    // --- fork-lineage fold toggle -----------------------------------------

    /// Expand the selected row's fork lineage, bringing back the members its head
    /// stands for.
    ///
    /// A no-op unless the selection is a head that is CURRENTLY hiding something.
    /// `hidden` carries an entry only for such a head (see [`lineage::fold`]), so
    /// asking it settles every other case at once: a rootless row, a lone
    /// session, an already-expanded lineage and an empty list all fall through
    /// without disturbing the list.
    pub fn expand_selected(&mut self) {
        let Some(index) = self.selected_index() else {
            return;
        };
        if !self.hidden.contains_key(&index) {
            return;
        }
        let Some(key) = lineage::lineage_key(&self.sessions[index]) else {
            return;
        };
        self.expanded.insert(key);
        self.reapply_preserving_selection();
    }

    /// Collapse the selected row's fork lineage back to its single head.
    ///
    /// A no-op when the selection has no lineage, or when the lineage has nothing
    /// to hide — a lone session, or one already folded, since either way only one
    /// of its members is on screen to begin with.
    pub fn collapse_selected(&mut self) {
        let Some(key) = self.selected_lineage() else {
            return;
        };
        let members = self.visible_lineage_members(&key);
        if members.len() < 2 {
            return;
        }
        let head = lineage::head_of(&self.sessions, &members);
        // Retarget FIRST — this ordering is the correctness of the whole method.
        // Collapsing drops every non-head member from `filtered`, so a selection
        // still pointing at a CHILD would be a selection on a row that no longer
        // exists, and `restore_selection` would clamp it to whatever unrelated row
        // inherited that position. Moving to the head while the children are still
        // visible means the row we are standing on is one the fold is guaranteed
        // to keep, and the user's place can never be lost as a side effect.
        self.set_selected(Some(self.sessions[head].session_id.clone()));
        self.expanded.remove(&key);
        self.reapply_preserving_selection();
    }

    // --- hide (soft delete) ------------------------------------------------

    /// Toggle the SELECTED session's membership in the persisted hidden set, then
    /// re-filter the board and persist the change.
    ///
    /// Hiding a row while [`show_hidden`](Self::show_hidden) is off drops it from
    /// the visible list; the selection then clamps to the nearest surviving row
    /// (via [`reapply_preserving_selection`](Self::reapply_preserving_selection),
    /// the same clamp a vanished reload id takes). A hidden row is DELIBERATELY
    /// NOT auto-revealed — contrast the fold's
    /// [`reveal_hidden`](Self::reveal_hidden), which re-opens a merely-folded row:
    /// a soft-hidden row is meant to stay off the board. Un-hiding (toggling an
    /// already-hidden id) puts it back on the next re-filter.
    ///
    /// A background-fork LINEAGE hides and exposes as ONE unit: the whole `(+N)`
    /// family flips together (see [`lineage_member_ids`](Self::lineage_member_ids)),
    /// so a folded head can never shed only itself and let the fold re-head to a
    /// surviving fork — the lineage leaves and returns to the board whole.
    ///
    /// A no-op when nothing is selected. Wired to the `Ctrl-X x` soft-hide chord.
    pub fn toggle_hidden_selected(&mut self) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        let members = self.lineage_member_ids(&id);
        delete::toggle_hidden(&mut self.hidden_ids, &members, &id);
        self.persist_hidden();
        self.reapply_preserving_selection();
    }

    /// Toggle whether user-hidden sessions are revealed inline, then re-filter.
    ///
    /// Turning it ON brings the hidden rows back (the view draws them dimmed and
    /// marked); turning it OFF drops them again. The selection is preserved by id
    /// and clamped if it left the newly-recomputed visible set. Pure visibility —
    /// nothing is persisted, since the toggle is a transient view state, not a
    /// stored preference. Wired to the `Ctrl-X h` show-hidden chord.
    pub fn toggle_show_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.reapply_preserving_selection();
    }

    /// Persist the hidden-id set to snapback's own state dir, FAIL-SOFT.
    ///
    /// Resolves the dir at SAVE time (never a cached path) so a `SNAPBACK_CONFIG_DIR`
    /// override is honored per call. A write error is surfaced as a transient board
    /// status but leaves the in-memory set AUTHORITATIVE for the rest of the
    /// session: the hidden state is a convenience, so a failed write must never
    /// abort the hide or the board.
    fn persist_hidden(&mut self) {
        if let Err(err) = hidden::save_hidden(&config::state_dir(), &self.hidden_ids) {
            self.set_status(format!("{HIDDEN_SAVE_ERROR_PREFIX}: {err}"));
        }
    }

    /// Toggle the preview pane visibility. Showing it (re)anchors to the newest
    /// turn so it opens at the bottom, matching a fresh selection.
    pub fn toggle_preview(&mut self) {
        self.show_preview = !self.show_preview;
        if self.show_preview {
            self.preview_follow_bottom = true;
            self.preview_scroll = 0;
        }
    }

    // --- preview scroll ----------------------------------------------------

    /// Move the preview by `rows` (positive = down/toward newer). Any explicit
    /// scroll drops follow-bottom and sets a concrete offset; the view clamps it
    /// to `[0, max_offset]` on the next render. Saturating so it can never
    /// underflow below zero or overflow `u16`.
    fn preview_scroll_by(&mut self, rows: i32) {
        self.preview_follow_bottom = false;
        let next = (i32::from(self.preview_scroll) + rows).clamp(0, i32::from(u16::MAX));
        self.preview_scroll = next as u16;
    }

    /// The page size for page/quarter-page scrolling: the last known viewport
    /// height, at least one row so a page always makes progress.
    fn preview_page(&self) -> u16 {
        self.preview_viewport_h.max(1)
    }

    /// Scroll the preview up one page (`PgUp`).
    pub fn preview_page_up(&mut self) {
        self.preview_scroll_by(-i32::from(self.preview_page()));
    }

    /// Scroll the preview down one page (`PgDn`).
    pub fn preview_page_down(&mut self) {
        self.preview_scroll_by(i32::from(self.preview_page()));
    }

    /// Scroll the preview up a quarter page (`Ctrl-U`).
    pub fn preview_half_up(&mut self) {
        self.preview_scroll_by(-i32::from((self.preview_page() / 4).max(1)));
    }

    /// Scroll the preview down a quarter page (`Ctrl-D`).
    pub fn preview_half_down(&mut self) {
        self.preview_scroll_by(i32::from((self.preview_page() / 4).max(1)));
    }

    /// Jump the preview to the top (`Home`); drops follow-bottom.
    pub fn preview_top(&mut self) {
        self.preview_follow_bottom = false;
        self.preview_scroll = 0;
    }

    /// Jump the preview to the bottom (`End`); re-enables follow-bottom so the
    /// view pins the offset to the newest turn.
    pub fn preview_bottom(&mut self) {
        self.preview_follow_bottom = true;
    }

    /// Scroll the preview one mouse-wheel notch (`up = true` toward older turns).
    /// Reuses the saturating clamp in [`preview_scroll_by`](Self::preview_scroll_by),
    /// so rapid notches neither underflow below zero nor overflow `u16`.
    pub fn preview_wheel(&mut self, up: bool) {
        let step = if up {
            -PREVIEW_WHEEL_STEP
        } else {
            PREVIEW_WHEEL_STEP
        };
        self.preview_scroll_by(step);
    }

    // --- splitter drag -------------------------------------------------------

    /// Begin dragging the list/preview splitter (mouse-down on the seam). A
    /// no-op while ANY modal overlay owns input (the running-session choice or
    /// the new-session agent picker), so a stray click during an overlay can
    /// never start a drag that a later `Drag` event would then apply once the
    /// overlay closes.
    pub fn begin_split_drag(&mut self) {
        if !self.overlay_active() {
            self.dragging_split = true;
        }
    }

    /// Whether the splitter is currently being dragged — gates `Drag` event
    /// routing in `tui::update::handle_mouse`.
    #[must_use]
    pub fn is_dragging_split(&self) -> bool {
        self.dragging_split
    }

    /// Recompute and persist the list pane's width from an absolute mouse
    /// column and the current body width, via [`clamp_list_width`] so neither
    /// pane is crushed below [`MIN_PANE_WIDTH`] or inverted. Safe to call at
    /// any time — even a degenerate `body_width` (e.g. before the first
    /// render, or the preview hidden) yields a well-formed width, which is
    /// re-clamped again on the next render regardless.
    pub fn drag_split_to(&mut self, col: u16, body_width: u16) {
        self.list_width = Some(clamp_list_width(col, body_width));
    }

    /// End a splitter drag (mouse-up). Defensive: clears the flag even if no
    /// [`begin_split_drag`](Self::begin_split_drag) preceded it, so a stray
    /// `Up` can never wedge the drag state.
    pub fn end_split_drag(&mut self) {
        self.dragging_split = false;
    }

    // --- transient status --------------------------------------------------

    /// Set a **sticky** board status (e.g. a resume refusal or a failure).
    /// Shown until the next actionable keypress clears it.
    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status = Some(message.into());
        self.status_ttl = None;
    }

    /// Set a **transient** board status (e.g. a confirmation or a gentle nudge).
    /// Shown for [`STATUS_DWELL_TICKS`] ticks, then auto-cleared by
    /// [`tick_status`]. Failures and refusals stay sticky via [`set_status`].
    pub fn set_status_transient(&mut self, message: impl Into<String>) {
        self.status = Some(message.into());
        self.status_ttl = Some(STATUS_DWELL_TICKS);
    }

    /// Clear any board status and its dwell timer. Called at the start of
    /// handling an actionable keypress so a message survives exactly until the
    /// next input.
    pub fn clear_status(&mut self) {
        self.status = None;
        self.status_ttl = None;
    }

    /// Decrement the status dwell timer and clear a transient status that has
    /// expired. Called from the `AppEvent::Tick` arm; uses `saturating_sub` so
    /// it never interacts with the wrapping `app.tick`.
    pub fn tick_status(&mut self) {
        match self.status_ttl {
            None => {}
            Some(0) => self.clear_status(),
            Some(n) => {
                self.status_ttl = Some(n.saturating_sub(1));
                if self.status_ttl == Some(0) {
                    self.clear_status();
                }
            }
        }
    }

    // --- reported agents + running-session choice overlay -----------------

    /// Replace the reported-agent set (delivered off-thread by the agents
    /// poller). Keyed by full `session_id`; the poller refreshes the whole map
    /// each cycle so stale entries self-heal without an explicit clear.
    pub fn set_reported_agents(&mut self, agents: HashMap<String, ReportedAgent>) {
        self.reported_agents = agents;
    }

    /// The reported-agent record for `session_id`, if claude knows it as an agent
    /// (drives the badge + banner; `None` for a row it never reported).
    ///
    /// Says NOTHING about liveness — an agent that reported completion is
    /// reported too, and both the badge and the banner must still see it in order
    /// to render it green and steady. Use
    /// [`is_live_now`](Self::is_live_now) to gate behavior on "running now", and
    /// [`live_agent_now`](Self::live_agent_now) for a hand-off's job `id`.
    ///
    /// RENDERING ONLY. This is the badge/banner accessor: the view draws from the
    /// polled snapshot precisely BECAUSE a render must never shell out. Nothing
    /// that hands off to `claude` may read it.
    #[must_use]
    pub fn reported_agent(&self, session_id: &str) -> Option<&ReportedAgent> {
        self.reported_agents.get(session_id)
    }

    /// Ask claude, RIGHT NOW, for the ACTIVE agent record it holds for
    /// `session_id` — the one authoritative read every hand-off makes.
    ///
    /// `Some(agent)` means claude is holding the session open AND carries its
    /// attach job `id`; `None` means it is not in claude's active list (finished,
    /// or the probe failed — see below). Both answers a hand-off needs come from
    /// this SINGLE read, which is the point: liveness and the job id can never be
    /// resolved against different snapshots.
    ///
    /// Deliberately NOT a read of [`reported_agents`](Self::reported_agents):
    /// that map is a ~1.3s-stale `--all` snapshot whose `done` qualifier means
    /// "the agent reported completion", not "claude will permit `-r`". The two
    /// can disagree transiently, and claude is the only authority on its own
    /// refusal.
    ///
    /// Routed through [`live_probe`](Self::live_probe) so tests seed the live set
    /// instead of spawning `claude`. Fail-soft: an unavailable signal is an empty
    /// map ⇒ `None` ⇒ the caller degrades toward letting claude decide (see
    /// [`crate::agents::live_agents`]). It SHELLS OUT — call it at a hand-off,
    /// never from a render.
    #[must_use]
    pub fn live_agent_now(&self, session_id: &str) -> Option<ReportedAgent> {
        // The probe hands back an owned, freshly-parsed map, so `remove` lifts the
        // record out without a clone; the map dies at the end of the statement.
        self.live_agents_now().remove(session_id)
    }

    /// Ask claude, RIGHT NOW, for its WHOLE active agent list — the one read a
    /// caller with SEVERAL sessions to judge must use.
    ///
    /// [`live_agent_now`](Self::live_agent_now) is this, narrowed to one id (and
    /// is expressed over it, so the two can never resolve against different
    /// snapshots). Reach for this one when N sessions are decided TOGETHER — the
    /// lineage hard-delete is the case: judging its members through the
    /// single-session accessor would spawn `claude` once PER MEMBER, N blocking
    /// shell-outs deep in the render loop, which is exactly what AGENTS.md's
    /// OFF-UI-THREAD rule forbids. One probe, one map, every member judged against
    /// the same instant — which also makes the verdicts mutually consistent
    /// rather than spread across N moments.
    ///
    /// Same posture as its narrowed sibling: SHELLS OUT, so call it at a hand-off
    /// or a confirm, never from a render; fail-soft to an empty map (see
    /// [`crate::agents::live_agents`]); routed through
    /// [`live_probe`](Self::live_probe) so tests seed it instead of spawning
    /// `claude`.
    #[must_use]
    pub fn live_agents_now(&self) -> HashMap<String, ReportedAgent> {
        (self.live_probe)()
    }

    /// Ask claude, RIGHT NOW, whether it is holding `session_id` open — the
    /// smart-Enter gate's predicate and the race-recovery probe.
    ///
    /// MEMBERSHIP in claude's fresh ACTIVE list, with nothing inferred and no
    /// bucket — expressed over [`live_agent_now`](Self::live_agent_now) so
    /// liveness and the attach job `id` are answered by ONE probe rather than two
    /// notions of "live". `Some` ⇒ live is exactly membership: the bare
    /// `claude agents --json` IS the active list.
    #[must_use]
    pub fn is_live_now(&self, session_id: &str) -> bool {
        self.live_agent_now(session_id).is_some()
    }

    /// Seed the ACTIVE list [`live_agent_now`](Self::live_agent_now) and
    /// [`is_live_now`](Self::is_live_now) read, so a test can state "claude says
    /// these are live" — and what their job ids are — without spawning `claude`.
    ///
    /// `#[cfg(test)]` on purpose: the seam exists ONLY for tests, and gating it
    /// this way is what guarantees the production board can never be handed
    /// anything but the real probe.
    ///
    /// The closure is called ONCE PER PROBE, so a test can hand back a DIFFERENT
    /// answer per call — which is what lets the Attach tests express a session
    /// that was live at the Enter gate and gone by the hand-off.
    #[cfg(test)]
    pub fn set_live_probe(&mut self, probe: impl Fn() -> HashMap<String, ReportedAgent> + 'static) {
        self.live_probe = Box::new(probe);
    }

    /// Seed what git reports as the launch project's worktrees, so a test can
    /// state "these folders are one project" without a repository or a
    /// subprocess.
    ///
    /// `#[cfg(test)]` for the same reason [`set_live_probe`](Self::set_live_probe)
    /// is: the seam exists ONLY for tests, so the production board can never be
    /// handed anything but the real resolver.
    ///
    /// Installing a probe does NOT re-resolve on its own — the cached set is
    /// refreshed at the same two moments production refreshes it (construction
    /// and reload), which is exactly what lets a test prove the scope reads a
    /// CACHE and not git.
    #[cfg(test)]
    pub fn set_worktree_probe(&mut self, probe: impl Fn(&Path) -> WorktreeSet + 'static) {
        self.worktree_probe = Box::new(probe);
    }

    /// Look up a loaded session by its stable id.
    #[must_use]
    pub fn session_by_id(&self, session_id: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.session_id == session_id)
    }

    /// Open the running-session choice overlay (Attach/Fork/Cancel) for a live
    /// session (Enter on a running row, or the race-recovery path), defaulting the
    /// highlight to the first choice. A `Row`-layout [`Modal`] targeting
    /// `session_id`.
    ///
    /// The Attach choice reconnects to the running session's background agent
    /// (`claude attach <job-id>`), from which it can be watched, continued, or
    /// stopped through claude's OWN agents — never a snapback-owned "completed"
    /// flag. Only background (`bg`) agents carry a job id, so on an interactive
    /// (`live`) session the choice refuses with a clear hint (Fork instead).
    pub fn open_live_choice(&mut self, session_id: String) {
        self.open_modal(Modal {
            title: "session is running".to_string(),
            message: "This session is running — it can't be plain-resumed.".to_string(),
            layout: ModalLayout::Row,
            choices: vec![
                ModalChoice {
                    label: "Attach".to_string(),
                    description: None,
                    action: ModalAction::Attach,
                },
                ModalChoice {
                    label: "Fork".to_string(),
                    description: None,
                    action: ModalAction::Fork,
                },
                ModalChoice {
                    label: "Cancel".to_string(),
                    description: None,
                    action: ModalAction::Cancel,
                },
            ],
            selected: 0,
            session_id: Some(session_id),
        });
    }

    /// Open the HARD-delete confirmation overlay for the selected session — a
    /// `Row`-layout [`Modal`] reading `[Delete this] [Delete lineage (N)] [Cancel]`,
    /// DEFAULT-HIGHLIGHTED ON CANCEL so a stray Enter can never trigger an
    /// irreversible delete. This method only OPENS the prompt — it never deletes.
    /// A no-op when nothing is selected.
    ///
    /// The LINEAGE choice appears only when the selection's fork lineage actually
    /// has more than one member, and its `(N)` is that real count. It exists
    /// because hide is already a GROUP operation
    /// ([`toggle_hidden_selected`](Self::toggle_hidden_selected)) while delete was
    /// single-id, and the asymmetry showed: hard-deleting a folded HEAD left its
    /// members behind and the fold simply re-headed to a surviving fork, so the
    /// row never left the board and the delete read as broken. The member ids are
    /// resolved HERE, from [`lineage_member_ids`](Self::lineage_member_ids) — the
    /// same grouping the hide uses, never a second rule — and ride the
    /// [`ModalAction::DeleteLineage`] choice, so the count shown and the set
    /// deleted cannot disagree even if a reload moves the selection while the
    /// modal is open.
    ///
    /// That grouping sweeps the FULL store, so `(N)` can exceed what is on
    /// screen: a soft-HIDDEN member counts and is deleted. Deliberate — it is
    /// hide's own rule reused rather than a second one, and hiding a copy is a
    /// visibility preference, not a claim the copy is gone. `(N)` therefore
    /// states the real size of the family the button takes, which is exactly the
    /// number that must not surprise anyone afterwards — and when some of that
    /// family is off screen, [`delete_confirm_message`] SAYS SO, so the number is
    /// predictable before the confirm rather than only explicable after it.
    ///
    /// The message states the BLAST RADIUS honestly, because the writer guard now
    /// admits parked background agents: what goes is the transcript on disk, the
    /// agent itself keeps existing in Claude Code until stopped there, and
    /// attaching + replying later can write a fresh transcript under that session.
    pub fn open_delete_confirm(&mut self) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        let members = self.lineage_member_ids(&id);
        // Counted from the SAME member list the button carries, so the disclosure
        // and the set deleted can never describe different families.
        let hidden = members
            .iter()
            .filter(|id| self.hidden_ids.contains(*id))
            .count();
        let message = delete_confirm_message(members.len(), hidden);
        let mut choices = vec![ModalChoice {
            label: "Delete this".to_string(),
            description: None,
            action: ModalAction::Delete,
        }];
        // Only offer the lineage when there IS one: a lone session would otherwise
        // get a second button that does exactly what the first does.
        if members.len() > 1 {
            choices.push(ModalChoice {
                label: format!("Delete lineage ({})", members.len()),
                description: None,
                action: ModalAction::DeleteLineage(members),
            });
        }
        choices.push(ModalChoice {
            label: "Cancel".to_string(),
            description: None,
            action: ModalAction::Cancel,
        });
        // Derive the default highlight from the Cancel choice's position rather
        // than a bare index, so the safe default survives a choice reorder — and,
        // now, the optional middle button that shifts Cancel's index.
        let selected = choices
            .iter()
            .position(|c| c.action == ModalAction::Cancel)
            .unwrap_or(0);
        self.open_modal(Modal {
            title: "delete session".to_string(),
            message,
            layout: ModalLayout::Row,
            choices,
            selected,
            session_id: Some(id),
        });
    }

    /// Open `modal` as the active overlay, taking the keyboard until it closes.
    pub fn open_modal(&mut self, modal: Modal) {
        self.modal = Some(modal);
    }

    /// Move the highlighted choice forward (wraps). No-op if no modal is open.
    pub fn modal_next(&mut self) {
        self.cycle_modal(1);
    }

    /// Move the highlighted choice backward (wraps). No-op if no modal is open.
    pub fn modal_prev(&mut self) {
        self.cycle_modal(-1);
    }

    /// Shift the highlighted choice by `delta`, wrapping across `choices` via
    /// `rem_euclid` (serves both layouts, both directions). A choiceless modal is
    /// left untouched so `rem_euclid(0)` can never panic.
    fn cycle_modal(&mut self, delta: isize) {
        if let Some(modal) = self.modal.as_mut() {
            let n = modal.choices.len() as isize;
            if n == 0 {
                return;
            }
            modal.selected = (modal.selected as isize + delta).rem_euclid(n) as usize;
        }
    }

    /// Dismiss the open modal, returning to the board. No-op if none is open.
    pub fn close_modal(&mut self) {
        self.modal = None;
    }

    // --- new-session agent picker -----------------------------------------

    /// Whether an overlay currently owns the board. The SINGLE gate predicate
    /// callers use (never `self.modal.is_some()` inline) to keep mouse actions
    /// (splitter drag / link open) from firing while an overlay is up — so a later
    /// gate extension lives in exactly one place.
    ///
    /// True while a [`Modal`] is open, the quick-reply compose zone, the
    /// stop-then-reply confirmation, or the interrupt confirmation owns the
    /// keyboard, OR a `Ctrl-X` leader chord is [pending](Self::pending_chord): each
    /// takes the keyboard, so each must equally gate the mouse (a stray click
    /// mid-chord must not start a drag or open a link), per PATTERNS §10.
    ///
    /// A [`draft`](Self::draft) counts for a related reason: it owns the PANE
    /// rather than the keyboard. While its card is drawn the transcript is not, so
    /// the cached link regions describe text that is no longer on screen — a click
    /// resolved against them would open a link from a session the user cannot see.
    /// It outlives the editor by AT MOST one in-flight launch (whichever comes
    /// first: that launch's own result, or the end of the board session), which is
    /// the window this arm covers on its own.
    #[must_use]
    pub fn overlay_active(&self) -> bool {
        self.modal.is_some()
            || self.compose.is_some()
            || self.draft.is_some()
            || self.pending_stop.is_some()
            || self.pending_interrupt.is_some()
            || self.pending_chord
    }

    /// Open the "stop the waiting agent?" confirmation for a `needs input` agent.
    pub fn open_stop_confirm(&mut self, session_id: String, job_id: String) {
        self.pending_stop = Some(PendingStop { session_id, job_id });
    }

    /// Dismiss the stop confirmation, returning to the board.
    pub fn stop_confirm_cancel(&mut self) {
        self.pending_stop = None;
    }

    /// Open the "stop this agent?" interrupt confirmation for a live, not-yet-finished
    /// agent (`Ctrl-K`).
    pub fn open_interrupt_confirm(&mut self, session_id: String, job_id: String) {
        self.pending_interrupt = Some(PendingInterrupt { session_id, job_id });
    }

    /// Dismiss the interrupt confirmation, returning to the board.
    pub fn interrupt_confirm_cancel(&mut self) {
        self.pending_interrupt = None;
    }

    /// Whether the compose editor owns the keyboard — EITHER draft, since which one
    /// is open is a `ComposeTarget` rather than a second piece of state. Gates key
    /// routing in [`super::update::handle_event`] (all keys go to the compose
    /// handler, bypassing `key_to_action`) and drives the compose-zone layout in
    /// [`super::view`].
    ///
    /// NOT the predicate for "the preview pane is a draft card" — that is
    /// [`draft`](Self::draft), which outlives this by one in-flight launch.
    #[must_use]
    pub fn is_composing(&self) -> bool {
        self.compose.is_some()
    }

    // --- the compose surface: editor + draft card, opened and closed as one ---

    /// Open the compose surface: the editor `state`, plus `draft` when this is a
    /// NEW-SESSION draft (`None` for a quick reply, which previews a real session).
    ///
    /// The ONE writer that installs either, so "a background draft always has a
    /// card and a reply never does" is structural rather than a convention each
    /// call site has to remember. Composing FORCE-SHOWS the preview, since both the
    /// card and the docked editor live in that pane (the renderer falls back to a
    /// full-width bottom bar when the pane is too short).
    pub fn open_compose(
        &mut self,
        state: super::compose::ComposeState,
        draft: Option<NewSessionDraft>,
    ) {
        self.show_preview = true;
        self.compose = Some(state);
        self.draft = draft;
    }

    /// Tear the compose surface DOWN — editor and draft card together.
    ///
    /// THE single teardown, so the two can never desync: `Esc`, every refusal, a
    /// vanished session, the interactive hand-off, and the finished background
    /// launch all end here rather than clearing one field and forgetting the other.
    pub fn close_compose(&mut self) {
        self.compose = None;
        self.draft = None;
    }

    /// Hand the drafted launch over to the driver: close the EDITOR but keep the
    /// card, stamped with the id of the launch it now reports (which is returned,
    /// so the request carries the same id back on completion).
    ///
    /// The one point where the two fields deliberately part, and only for as long
    /// as the child runs: there is nothing left to type, but the pane still has no
    /// session to show, so the card stays and reports the launch it just
    /// dispatched. The one-shot `AppEvent::BgLaunchFinished` — which
    /// `send::spawn_bg_launch` emits exactly once, spawn failures included — closes
    /// it, but ONLY through [`launching_draft`](Self::launching_draft): by the time
    /// it lands the surface may be a quick reply or a second draft, and neither is
    /// this launch's to close.
    ///
    /// `#[must_use]` for the same reason as its sibling checks, and more sharply:
    /// the returned id is the WHOLE of that identity guard. A caller that drops it
    /// still stamps the card, but dispatches a launch whose completion carries an id
    /// nothing can match — so `launching_draft` never fires and the card is stranded
    /// for the rest of the board session.
    #[must_use = "the launch id must ride out on the request, or the card is unmatchable"]
    pub fn dispatch_draft(&mut self) -> u64 {
        // `wrapping_add` for the same reason `tick` uses it: a board left running
        // forever must roll over rather than overflow-panic in debug. Two launches
        // 2^64 apart cannot be in flight together, so a wrapped id cannot collide.
        let launch_id = self.next_bg_launch_id;
        self.next_bg_launch_id = self.next_bg_launch_id.wrapping_add(1);
        self.compose = None;
        if let Some(draft) = self.draft.as_mut() {
            draft.launch_id = Some(launch_id);
        }
        launch_id
    }

    /// The in-flight draft card reporting `launch_id`, or `None` when the pane has
    /// moved on to something else.
    ///
    /// The launch twin of [`sending_to`](Self::sending_to), and load-bearing for
    /// the same reason: a completion event says only that ONE dispatch finished,
    /// never that whatever is on screen now belongs to it. The card outlives its
    /// editor, so the surface underneath when the result lands may be a quick reply
    /// (`Ctrl-R`) or a second draft — closing either would discard a typed buffer
    /// the user never abandoned.
    #[must_use]
    pub fn launching_draft(&self, launch_id: u64) -> Option<&NewSessionDraft> {
        self.draft
            .as_ref()
            .filter(|d| d.launch_id == Some(launch_id))
    }

    /// The in-flight quick-reply send targeting `session_id`, or `None` when no
    /// send is in flight for that session. The preview's optimistic echo and its
    /// banner-suppression both key off this so render and the click hit-test agree.
    #[must_use]
    pub fn sending_to(&self, session_id: &str) -> Option<&Sending> {
        self.sending.as_ref().filter(|s| s.session_id == session_id)
    }

    /// The in-flight `claude stop` interrupt targeting `session_id`, or `None`
    /// when no interrupt is in flight for that session. The interrupt's twin of
    /// [`sending_to`](Self::sending_to): a completion event must only clear the state
    /// it belongs to, so a stale result cannot land on a surface that has moved on.
    #[must_use]
    pub fn interrupting_on(&self, session_id: &str) -> Option<&Interrupting> {
        self.interrupting
            .as_ref()
            .filter(|i| i.session_id == session_id)
    }

    /// Open the new-session agent picker over `agents` as a `List`-layout
    /// [`Modal`], pre-highlighting the last-picked agent (or the default entry when
    /// none was picked or it is gone) via [`pick_default_index`]. Choice 0 is the
    /// synthetic "default (no agent)" entry ([`ModalAction::New`]`(None)`); each
    /// discovered agent follows as `New(Some(name))`, carrying its own name so
    /// confirm needs no index-to-agent lookup.
    ///
    /// The caller only opens this when discovery found at least one agent — an
    /// empty launch dir skips straight to a bare `claude`, so the common no-agent
    /// case keeps its zero-extra-keystroke path.
    pub fn open_agent_picker(&mut self, agents: Vec<DefinedAgent>) {
        let selected = pick_default_index(self.last_new_agent.as_deref(), &agents);
        let mut choices = vec![ModalChoice {
            label: "default (no agent)".to_string(),
            description: None,
            action: ModalAction::New(None),
        }];
        for agent in agents {
            choices.push(ModalChoice {
                label: agent.name.clone(),
                description: agent.description,
                action: ModalAction::New(Some(agent.name)),
            });
        }
        self.open_modal(Modal {
            title: "new session".to_string(),
            message: "Start a new session — pick an agent:".to_string(),
            layout: ModalLayout::List,
            choices,
            selected,
            session_id: None,
        });
    }

    /// Remember the agent chosen for the last new session (`None` = default / no
    /// agent), so the next `Ctrl-N` pre-highlights it. In-memory only.
    pub fn set_last_new_agent(&mut self, agent: Option<String>) {
        self.last_new_agent = agent;
    }

    // --- autorefresh reload -----------------------------------------------

    /// Replace the session list (a `SessionsChanged` reload), re-apply the
    /// active query+scope, and PRESERVE the selection by stable id and the
    /// scroll offset. If the selected id vanished, the selection clamps to the
    /// nearest surviving row.
    ///
    /// This is also where the launch project's worktree set is RE-RESOLVED, and
    /// it needs no wiring at any caller: every reload path already funnels
    /// through here — the `SessionsChanged` watcher event and the post-resume
    /// reload in `lib::run` (the two the autoreload exists for), plus the
    /// post-delete reload — so all of them pick a worktree created mid-run up by
    /// construction, and a future reload path gets the same behavior for free.
    /// Off-UI-thread: a reload is a bounded one-shot, unlike `recompute_scope` /
    /// `toggle_scope`, which run on a keystroke and must never ask git.
    pub fn apply_sessions(&mut self, sessions: Vec<Session>) {
        let prev_id = self.selected.clone();
        let prev_pos = self.selected_pos();

        self.sessions = sessions;
        self.index.refresh(&self.sessions);
        // The transcript on disk may have changed; drop stale preview text.
        self.preview_cache.clear();
        // Re-resolve BEFORE `recompute_scope` so this reload is already scoped by
        // the CURRENT worktree set rather than one reload behind it. The same
        // expression `App::new` seeds with, on purpose: launch and reload must not
        // be able to disagree about what a project is.
        self.worktrees = (self.worktree_probe)(&self.launch_dir);
        self.recompute_scope();
        self.recompute_filtered();

        self.restore_selection(prev_id, prev_pos);
        self.clamp_scroll();
    }

    // --- internals --------------------------------------------------------

    /// Recompute the scope membership set (`scoped`) and the header's counted
    /// [`population`](Self::population). This is the only path that
    /// canonicalizes `cwd`s, so it runs on reload / scope-toggle, never on a
    /// per-keystroke query change.
    fn recompute_scope(&mut self) {
        let scope = self.scope;
        let launch = self.launch_dir.as_path();
        // The CACHED worktree set, never a fresh one: this runs on a keystroke
        // (`toggle_scope`), and resolving git here would put a subprocess on the UI
        // thread. The set is refreshed at launch and on reload instead.
        let worktrees = &self.worktrees;
        self.scoped = (0..self.sessions.len())
            .filter(|&i| in_scope(scope, &self.sessions[i], launch, worktrees))
            .collect();
        let counted: Vec<usize> = match scope {
            // The whole store — the counter's original denominator, kept because
            // a board showing every repo on the machine is not about one project.
            Scope::All => (0..self.sessions.len()).collect(),
            // Already exactly this membership: reuse the pass above rather than
            // canonicalizing every `cwd` a second time on one `Ctrl-A`.
            Scope::Project => self.scoped.clone(),
            // The one scope whose counted population is WIDER than its rows, and
            // the only arm that costs a second pass. It is the default scope, so
            // this is the pass that matters — and it is affordable exactly
            // because it lands here and not on a keystroke or a frame.
            Scope::CurrentFolder => (0..self.sessions.len())
                .filter(|&i| in_scope(Scope::Project, &self.sessions[i], launch, worktrees))
                .collect(),
        };
        // Group the counted set into conversations HERE rather than per call.
        // Unlike the membership above, this is PURE — it reads no path and could
        // legally sit in `session_counts` — but it allocates a `(repo, branch,
        // root)` key per member, and under `All` that member set is the entire
        // store. The result changes only when the population does, i.e. on
        // exactly the two events that reach this function, so paying for it on a
        // frame or a keystroke would buy nothing. What must stay per-call is the
        // hidden split, and that one does (`count_lineages`).
        self.population = lineage::group_members(&self.sessions, &counted);
    }

    /// Recompute `filtered` from the cached scope set and the active query, sort
    /// it into scope-aware display order via
    /// [`order_filtered`](Self::order_filtered), then FOLD each collapsed fork
    /// lineage down to its head.
    ///
    /// An empty query takes the whole scope set; a non-empty query pushes the
    /// scope membership set INTO the search pass
    /// ([`results_within`](crate::search::SearchIndex::results_within)), so only
    /// in-scope sessions are ever scanned — an out-of-scope match is never
    /// searched just to be discarded. Both paths funnel through
    /// [`order_filtered`](Self::order_filtered), so display ordering is applied
    /// uniformly and the result is never left in raw store order.
    ///
    /// [`order_filtered`](Self::order_filtered) is the SOLE source of display
    /// order — the search pass deliberately returns candidates in the order given
    /// and ranks nothing, because this key is a tie-free total order that would
    /// discard any rank anyway.
    ///
    /// The fold is deliberately LAST, AFTER the ordering: it leaves `filtered`
    /// holding only the indices the user can actually SEE. That single choice is
    /// what lets `selected_pos`, `move_selection`, `clamp_scroll` and the mouse
    /// wheel keep working untouched and know nothing about lineages — every one
    /// of them would otherwise have to skip hidden rows by hand. Folding earlier
    /// would also hand `order_filtered` a list it no longer decides the shape of.
    fn recompute_filtered(&mut self) {
        if self.query.is_empty() {
            self.filtered = self.scoped.clone();
        } else {
            self.filtered = self.index.results_within(&self.scoped);
        }
        // Drop user-hidden sessions unless the show-hidden toggle is on. Slotted
        // AFTER the query pass and BEFORE ordering + folding, so hidden rows never
        // reach `order_filtered` or `lineage::fold`: to the rest of the pipeline a
        // hidden row simply is not in the visible set. When `show_hidden` is on the
        // rows stay and the view marks them (see `render_list`). This is a
        // PRESENTATION filter — the sessions are still loaded, parsed and indexed;
        // only their visibility changes.
        if !self.show_hidden {
            self.filtered
                .retain(|&i| !self.hidden_ids.contains(&self.sessions[i].session_id));
        }
        self.order_filtered();
        let folded = lineage::fold(&self.sessions, &self.filtered, &self.expanded);
        self.filtered = folded.visible;
        self.hidden = folded.hidden;
    }

    /// Sort [`filtered`](Self::filtered) into scope-aware DISPLAY order.
    ///
    /// [`Scope::CurrentFolder`] is a flat list ordered by timestamp DESC (with
    /// `None` last), tie-broken by `session_id` ascending for determinism.
    /// [`Scope::All`] AND [`Scope::Project`] order groups by each group's
    /// most-recent (max) timestamp DESC (a group whose sessions are all `None`
    /// sorts last), then by group key ascending so same-group rows stay
    /// contiguous, then by session timestamp DESC (`None` last), then
    /// `session_id` ascending. The per-group max is precomputed once so the sort
    /// stays O(n log n).
    ///
    /// The flat arm is [`Scope::CurrentFolder`]'s ALONE, because it is the only
    /// scope that cannot span more than one folder: a project's worktrees are
    /// several folders on several branches, so [`Scope::Project`] needs the same
    /// repo->branch grouping [`Scope::All`] does to stay readable. That holds
    /// whether or not git resolved — [`in_scope`]'s repo-root arm spans the
    /// repo either way — which is why the arm is chosen from `self.scope`
    /// directly, with no fallback translation in between.
    ///
    /// The grouped arm keys on [`group_key`], the SAME function
    /// [`build_rows`] heads on, so the project scope's unified head cannot sort
    /// rows apart that it then draws under one head — which would re-emit that
    /// head once per run.
    fn order_filtered(&mut self) {
        // Taken before the sort, which borrows `self.filtered` mutably; the head
        // is owned already, so nothing here keeps a borrow of `self` alive.
        let project_head = self.project_head();
        match self.scope {
            Scope::CurrentFolder => {
                let sessions = &self.sessions;
                self.filtered.sort_by_cached_key(|&i| {
                    let s = &sessions[i];
                    (Reverse(s.timestamp), s.session_id.clone())
                });
            }
            Scope::All | Scope::Project => {
                let head = project_head.as_deref();
                // Precompute each group's most-recent timestamp once. Option's
                // Ord gives `Some > None` and later-time-greater, so `max`
                // yields the group's newest session (or `None` if all are).
                let mut group_max: HashMap<(String, String), Option<OffsetDateTime>> =
                    HashMap::new();
                for &i in &self.filtered {
                    let s = &self.sessions[i];
                    let entry = group_max.entry(group_key(s, head)).or_default();
                    *entry = (*entry).max(s.timestamp);
                }
                let sessions = &self.sessions;
                self.filtered.sort_by_cached_key(|&i| {
                    let s = &sessions[i];
                    let key = group_key(s, head);
                    let gmax = group_max.get(&key).copied().flatten();
                    (
                        Reverse(gmax),
                        key,
                        Reverse(s.timestamp),
                        s.session_id.clone(),
                    )
                });
            }
        }
    }

    /// Re-filter after a query/mode/scope change while preserving the selection
    /// by id (clamping to nearest if it left the filtered set).
    fn reapply_preserving_selection(&mut self) {
        let prev_id = self.selected.clone();
        let prev_pos = self.selected_pos();
        self.recompute_filtered();
        self.restore_selection(prev_id, prev_pos);
        self.clamp_scroll();
    }

    /// Restore the selection after `filtered` was recomputed: keep the same id if
    /// it survived; auto-expand its lineage if it was merely FOLDED away; else
    /// clamp the previous position into the new list.
    fn restore_selection(&mut self, prev_id: Option<String>, prev_pos: Option<usize>) {
        let survived = prev_id.as_ref().and_then(|id| {
            self.filtered
                .iter()
                .position(|&i| &self.sessions[i].session_id == id)
        });
        if let Some(pos) = survived {
            self.select_pos(pos);
            return;
        }
        // Absent from `filtered` is not the same as GONE. An autorefresh that
        // introduces a newer fork of the selected session folds the user's row
        // under a head that did not exist a moment ago — the session is still
        // right there on disk. Clamping to a neighbour would move the user's place
        // because a BACKGROUND job wrote a file, which is precisely the surprise
        // the fold is supposed to prevent. Open the lineage instead.
        if let Some(pos) = prev_id.as_deref().and_then(|id| self.reveal_hidden(id)) {
            self.select_pos(pos);
            return;
        }
        if self.filtered.is_empty() {
            self.set_selected(None);
            return;
        }
        let pos = prev_pos.unwrap_or(0).min(self.filtered.len() - 1);
        self.select_pos(pos);
    }

    /// Reveal `id` when it is FOLDED away rather than filtered out, by expanding
    /// its lineage; reports its position in the recomputed `filtered`.
    ///
    /// Returns `None` — leaving `expanded` EXACTLY as it found it — when the id is
    /// missing for any other reason: deleted on disk, out of scope, or not a query
    /// match. That rollback is what stops a vanishing row from quietly unfolding a
    /// lineage the user never opened (a scope toggle would otherwise leave one
    /// open behind their back); only an expansion that genuinely PUT THE ROW BACK
    /// is allowed to stick.
    fn reveal_hidden(&mut self, id: &str) -> Option<usize> {
        let key = lineage::lineage_key(self.sessions.iter().find(|s| s.session_id == id)?)?;
        if !self.expanded.insert(key.clone()) {
            // Already open, so nothing was folding this row away: it is gone.
            return None;
        }
        self.recompute_filtered();
        let pos = self
            .filtered
            .iter()
            .position(|&i| self.sessions[i].session_id == id);
        if pos.is_none() {
            self.expanded.remove(&key);
            self.recompute_filtered();
        }
        pos
    }

    /// Clamp the scroll offset into the current row bounds.
    fn clamp_scroll(&mut self) {
        let max = self.filtered.len().saturating_sub(1);
        self.scroll = self.scroll.min(max);
    }

    // --- render helpers (consumed by tui::view) ---------------------------

    /// The header counter's three numbers — ALL of them, in LINEAGES — over the
    /// current scope's counted [`population`](Self::population) and the board's
    /// current display list.
    ///
    /// The numerator comes from here too, and that is the fix: `tui::view` used
    /// to pair `filtered.len()` with this call's `total`, which put a post-fold
    /// row count over a session-FILE count. One accessor, one grouping rule, one
    /// unit — see [`SessionCounts`].
    ///
    /// An ACCESSOR rather than a public field because the population is a cache
    /// with an invariant — it is only ever written by
    /// [`recompute_scope`](Self::recompute_scope) — and `tui::view` is a SIBLING
    /// module that would otherwise be able to hand out a stale one.
    ///
    /// Derived per call, not cached: the split depends on
    /// [`hidden_ids`](Self::hidden_ids) and [`show_hidden`](Self::show_hidden),
    /// which change on keypresses that must never re-canonicalize a path, and
    /// [`count_lineages`] costs a hash lookup per counted session plus one
    /// grouping pass over the (already narrow) display list. The expensive half
    /// is the population and its grouping, and those ARE cached.
    #[must_use]
    pub fn session_counts(&self) -> SessionCounts {
        count_lineages(
            &self.sessions,
            &self.population,
            &self.filtered,
            &self.hidden_ids,
            self.show_hidden,
        )
    }

    /// The ONE repo head every row groups and renders under in the project
    /// scope, or `None` when the rows keep their own [`Session::repo`] labels.
    ///
    /// UNCONDITIONAL in [`Scope::Project`], and that is the whole invariant: a
    /// project-scoped list is grouped (see [`build_rows`]), and a grouped list
    /// with no head override falls back to [`Session::repo`] — which spells a
    /// checkout and its own worktree differently, splitting one project into a
    /// `snapback` head and an `ilfroloff/snapback` head. That split is what the
    /// override exists to remove, so there is no state in which the scope draws
    /// grouped and this may answer `None`.
    ///
    /// It used to require a RESOLVED worktree set as well, matched by a
    /// `render_scope` that drew an unresolved project flat. Both halves of that
    /// went together, and both are gone: [`in_scope`]'s repo-root arm needs no
    /// git, so an unresolved project scope still spans the repo and still draws
    /// grouped. Naming it is [`Self::project_label`]'s job, and it is TOTAL
    /// precisely so this can be unconditional.
    ///
    /// Owned (`Option<String>`), because the name it carries is not always a
    /// substring of anything `&self` holds — see [`Self::project_label`].
    #[must_use]
    pub fn project_head(&self) -> Option<String> {
        match self.scope {
            Scope::Project => Some(self.project_label()),
            _ => None,
        }
    }

    /// What to CALL this project: the label git resolved for the whole worktree
    /// set, else the name of the REPO ROOT the launch dir sits in.
    ///
    /// That is the same preference — and the same fallback function,
    /// [`project_root_name`] — that `tui::view`'s `project_name` applies to the
    /// HEADER, so the one group head and the header name the project identically
    /// for EVERY launch dir, degenerate ones included. Two compositions of one
    /// naming rule are two chances to drift, so the agreement is asserted, not
    /// assumed: three `tui::view` tests compare the two answers directly —
    /// `head_and_header_name_a_non_utf8_launch_dir_the_same_way`,
    /// `project_scope_header_names_the_launch_dir_when_a_resolved_set_has_no_label`
    /// and `project_scope_header_names_the_repo_root_not_the_worktree_launched_from`.
    ///
    /// The resolved label wins because the scope spans several folders; naming
    /// the one worktree that happened to launch snapback would misdescribe a list
    /// drawn from all of them. The fallback names the repo ROOT for exactly that
    /// reason — a worktree dir is named after its BRANCH — and for a plain
    /// checkout the two are the same directory.
    ///
    /// Total (a `String`, never `None`) — see [`Self::project_head`] for why an
    /// absent head is not an option here: it is not "one unnamed head", it is the
    /// per-folder heads coming back.
    ///
    /// ALLOCATES, and that is what buys the agreement: a lossy repair is a new
    /// `String` that cannot be borrowed back out of `&self`. The cost is ONE
    /// allocation per [`App::rows`] / [`App::order_filtered`] call, not one per
    /// row — [`group_key`] already builds two `String`s for every row it keys
    /// (from this very label), and `order_filtered` already had to own the label
    /// to sort under it.
    fn project_label(&self) -> String {
        self.worktrees
            .label()
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| project_root_name(&self.launch_dir))
    }

    /// The rows for the current filtered list: grouped under repo->branch heads
    /// in the all and project scopes, flat and head-less in the current-folder
    /// scope alone. In the project scope every head is the one project label
    /// ([`project_head`](Self::project_head)).
    #[must_use]
    pub fn rows(&self) -> Vec<Row> {
        build_rows(
            &self.sessions,
            &self.filtered,
            self.scope,
            &self.hidden,
            self.project_head().as_deref(),
        )
    }

    /// The row index (into [`rows`](Self::rows)) of the selected session, for
    /// driving the `ListState` highlight.
    #[must_use]
    pub fn selected_row(&self, rows: &[Row]) -> Option<usize> {
        let sel = self.selected_pos()?;
        let target = *self.filtered.get(sel)?;
        rows.iter()
            .position(|r| matches!(r, Row::Session { index, .. } if *index == target))
    }

    /// The CHAR indices within `display` (a row's visible label) that the
    /// active query matches, for search-match highlighting in the list.
    ///
    /// Delegates to the nucleo seam in [`crate::search`], which scores against
    /// the display string itself (decoupled from the filtering haystack) and
    /// returns sorted, deduplicated char positions. Empty when the query is
    /// empty or does not appear in the visible label (e.g. a content-only hit).
    #[must_use]
    pub fn match_indices(&mut self, display: &str) -> Vec<u32> {
        self.index.match_indices(display)
    }

    /// The width-scoped preview cache entry for the current selection, rendering
    /// and inserting it on a miss.
    ///
    /// A change in `inner_width` CLEARS the whole cache first: GFM tables
    /// shrink-to-fit, so the layout, the link-region columns AND the wrapped row
    /// count all depend on the width (see `preview_cache`). `None` when nothing is
    /// selected or the selected id is no longer among the loaded sessions. This is
    /// the single source [`preview_text`](Self::preview_text),
    /// [`preview_hit_context`](Self::preview_hit_context) and
    /// [`preview_wrapped_rows`](Self::preview_wrapped_rows) read, so the text drawn,
    /// the regions hit-tested and the height scrolled against can never come from
    /// different renders.
    fn ensure_preview(&mut self, inner_width: u16) -> Option<&CachedPreview> {
        if self.preview_width != Some(inner_width) {
            self.preview_cache.clear();
            self.preview_width = Some(inner_width);
        }
        let id = self.selected.clone()?;
        if !self.preview_cache.contains_key(&id) {
            let session = self.sessions.iter().find(|s| s.session_id == id)?;
            // Defined-agent names gate the preview's `agent-name` fallback so a
            // free-form background-job title never renders as a bogus handle.
            let known_agents: HashSet<&str> = self.agent_names.iter().map(String::as_str).collect();
            let rendered = preview::render(session, usize::from(inner_width), &known_agents);
            // Measure ONCE, here, where the text and the width it was rendered for
            // are both in hand — the wrapper walks every line, so a per-frame
            // measurement would put a whole-transcript pass on the draw path.
            let wrapped_rows = view::wrapped_text_rows(&rendered.text.lines, inner_width);
            self.preview_cache.insert(
                id.clone(),
                CachedPreview {
                    rendered,
                    wrapped_rows,
                },
            );
        }
        self.preview_cache.get(&id)
    }

    /// Readable, markdown-styled transcript for the selected session, fit to
    /// `inner_width` (the preview pane's inner content width, in columns) so GFM
    /// tables shrink-to-fit and never wrap. Lazily rendered and cached by id;
    /// a change in `inner_width` clears the cache first (see `preview_cache`).
    /// Empty `Text` when nothing is selected.
    pub fn preview_text(&mut self, inner_width: u16) -> Text<'static> {
        self.ensure_preview(inner_width)
            .map(|p| p.rendered.text.clone())
            .unwrap_or_default()
    }

    /// Screen rows the selected session's transcript occupies once WORD-wrapped at
    /// `inner_width` — the transcript's true content height, which is what the
    /// bottom anchor, the scroll clamp and the scrollbar are all derived from.
    ///
    /// Read off the SAME width-scoped cache the text is drawn from
    /// ([`ensure_preview`](Self::ensure_preview)), measured there by
    /// `view::wrapped_text_rows`, so the height can never describe a different render
    /// than the one on screen. `0` when nothing is selected.
    ///
    /// It counts the TRANSCRIPT and nothing else. A pane showing something else —
    /// the new-session draft card — or showing more than that — the optimistic tail
    /// of an in-flight reply — measures the difference itself at the draw site, since
    /// neither was ever in this cache.
    pub fn preview_wrapped_rows(&mut self, inner_width: u16) -> usize {
        self.ensure_preview(inner_width)
            .map_or(0, |p| p.wrapped_rows)
    }

    /// The wrapped-layout context needed to hit-test a mouse click into a preview
    /// link: each content line's DISPLAY width (feeding the APPROXIMATE
    /// character-packing model in `view::wrapped_line_height`, the hit-test's alone —
    /// the transcript's height comes from
    /// [`preview_wrapped_rows`](Self::preview_wrapped_rows)) and the clickable
    /// [`LinkRegion`](preview::LinkRegion)s — both pulled from the SAME width-scoped
    /// cache the view drew from, so a hit-test can never disagree with what is on
    /// screen. Empty when nothing is selected.
    pub fn preview_hit_context(
        &mut self,
        inner_width: u16,
    ) -> (Vec<usize>, Vec<preview::LinkRegion>) {
        match self.ensure_preview(inner_width) {
            Some(p) => (
                p.rendered.text.lines.iter().map(Line::width).collect(),
                p.rendered.links.clone(),
            ),
            None => (Vec::new(), Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Synthetic session with an explicit id, repo, branch, and cwd.
    fn session(id: &str, repo: &str, branch: Option<&str>, cwd: &str) -> Session {
        Session {
            file: PathBuf::from(format!("/tmp/{id}.jsonl")),
            session_id: id.to_string(),
            cwd: PathBuf::from(cwd),
            git_branch: branch.map(str::to_string),
            timestamp: None,
            repo: repo.to_string(),
            label: format!("label for {id}"),
            root_uuid: None,
            msg_count: 0,
            content_index: String::new(),
        }
    }

    /// Like [`session`] but with a concrete timestamp (unix seconds), for
    /// exercising the scope-aware display ordering.
    fn session_ts(
        id: &str,
        repo: &str,
        branch: Option<&str>,
        cwd: &str,
        unix_secs: i64,
    ) -> Session {
        let mut s = session(id, repo, branch, cwd);
        s.timestamp = Some(OffsetDateTime::from_unix_timestamp(unix_secs).unwrap());
        s
    }

    /// An app over `sessions` in [`Scope::All`] (scope does not interfere) with
    /// a throwaway launch dir.
    fn app_all(sessions: Vec<Session>) -> App {
        App::new(sessions, Scope::All, PathBuf::from("/tmp/launch"))
    }

    /// A member of a fork lineage: like [`session_ts`] but carrying the `root`
    /// uuid every member of a lineage copies verbatim.
    ///
    /// Every lineage session is pinned to ONE repo+branch, because the lineage
    /// key is scoped to them (D4) — a fixture that let the branch vary would be
    /// testing the scoping rather than the fold.
    fn session_fork(id: &str, cwd: &str, root: &str, unix_secs: i64) -> Session {
        let mut s = session_ts(id, "repo", Some("main"), cwd, unix_secs);
        s.root_uuid = Some(root.to_string());
        s
    }

    /// The session ids the board would DRAW, in display order — i.e. `filtered`
    /// after the fold, which is the whole point of folding there (D6).
    fn visible_ids(app: &App) -> Vec<&str> {
        app.filtered
            .iter()
            .map(|&i| app.sessions[i].session_id.as_str())
            .collect()
    }

    /// How many lineage members the row for `id` hides — `None` when it hides
    /// nothing and would therefore render no `(+N)` marker.
    fn hidden_for(app: &App, id: &str) -> Option<usize> {
        let index = app.sessions.iter().position(|s| s.session_id == id)?;
        app.hidden.get(&index).copied()
    }

    // --- fork-lineage folding ---------------------------------------------

    #[test]
    fn folding_hides_members_and_keeps_the_head() {
        // The real background-fork shape: ONE conversation in two files sharing a
        // root uuid under one repo+branch. The bg copy kept growing after the
        // fork, so it is the newer member and therefore the head (D1).
        let mut app = app_all(vec![
            session_fork("ancestor", "/tmp/w", "fork-root", 100),
            session_fork("bg", "/tmp/w", "fork-root", 200),
            session_fork("lone", "/tmp/w", "other-root", 50),
        ]);

        // FOLDED IS THE DEFAULT: `expanded` starts empty, so the duplicate row
        // the user reported is gone without them doing anything.
        assert_eq!(
            visible_ids(&app),
            vec!["bg", "lone"],
            "the stalled ancestor folds under its newest fork by default"
        );
        assert_eq!(
            hidden_for(&app, "bg"),
            Some(1),
            "the head must report the one member it stands for, so `(+N)` can \
             say the row is not the whole story"
        );
        assert_eq!(
            hidden_for(&app, "lone"),
            None,
            "a session with a lineage of one hides nothing and must never claim \
             a (+N)"
        );

        // Expanding brings the ancestor back — nothing was dropped, only hidden.
        app.expand_selected();
        assert_eq!(visible_ids(&app), vec!["bg", "ancestor", "lone"]);
        assert_eq!(
            hidden_for(&app, "bg"),
            None,
            "an open head is standing in for nobody"
        );
    }

    #[test]
    fn j_k_walk_an_expanded_head_into_its_own_children() {
        // D6: `filtered`'s order IS the navigation order — `selected_pos`,
        // `move_selection`, `clamp_scroll` and the wheel all read positions in it.
        // So gathering has to leave `j` walking head -> its children -> next head.
        // The interloper is what gives this teeth: it is NEWER than the ancestor,
        // so an ungathered list would read bg -> interloper -> ancestor and `j`
        // would step off the lineage and back onto it.
        let mut app = app_all(vec![
            session_fork("ancestor", "/tmp/w", "fork-root", 100),
            session_fork("bg", "/tmp/w", "fork-root", 300),
            session_fork("interloper", "/tmp/w", "other-root", 200),
        ]);
        app.select_first();
        app.expand_selected();
        assert_eq!(
            visible_ids(&app),
            vec!["bg", "ancestor", "interloper"],
            "the ancestor gathers under its head, ahead of the newer interloper"
        );

        // Walk down with `j`, then back up with `k`.
        let mut walk = vec![app.selected.clone().unwrap()];
        for _ in 0..2 {
            app.move_selection(1);
            walk.push(app.selected.clone().unwrap());
        }
        assert_eq!(
            walk,
            vec!["bg", "ancestor", "interloper"],
            "`j` must walk the head into its OWN child before the next head"
        );
        app.move_selection(-1);
        assert_eq!(
            app.selected.as_deref(),
            Some("ancestor"),
            "`k` retraces the same order"
        );
    }

    #[test]
    fn collapsing_with_a_child_selected_retargets_to_the_head() {
        let mut app = app_all(vec![
            session_fork("ancestor", "/tmp/w", "fork-root", 100),
            session_fork("bg", "/tmp/w", "fork-root", 200),
            session_fork("neighbour", "/tmp/w", "other-root", 50),
        ]);
        app.expand_selected();
        // Stand on the CHILD — precisely the row the collapse is about to hide.
        app.move_selection(1);
        assert_eq!(app.selected.as_deref(), Some("ancestor"));

        app.collapse_selected();

        assert_eq!(
            app.selected.as_deref(),
            Some("bg"),
            "collapsing must retarget the selection to the lineage head; a \
             selection left on a row the fold removes cannot survive it"
        );
        assert_eq!(
            app.selected_pos(),
            Some(0),
            "the head is a row that is actually on screen"
        );
        assert_eq!(visible_ids(&app), vec!["bg", "neighbour"]);
    }

    #[test]
    fn expand_state_survives_apply_sessions_reorder() {
        // STABLE-ID STATE, applied to the fold. Two lineages of two; the user
        // opens only the FIRST.
        let a_head = session_fork("a-head", "/tmp/w", "root-a", 400);
        let a_child = session_fork("a-child", "/tmp/w", "root-a", 300);
        let b_head = session_fork("b-head", "/tmp/w", "root-b", 200);
        let b_child = session_fork("b-child", "/tmp/w", "root-b", 100);

        let mut app = app_all(vec![
            a_head.clone(),
            a_child.clone(),
            b_head.clone(),
            b_child.clone(),
        ]);
        app.expand_selected();
        assert_eq!(visible_ids(&app), vec!["a-head", "a-child", "b-head"]);

        // Reload the SAME four sessions with lineage B first. Every index moves,
        // and A's head index (0) now addresses B's head — so an `expanded` keyed
        // by index would open the WRONG lineage and re-fold the user's.
        app.apply_sessions(vec![b_head, b_child, a_head, a_child]);

        assert_eq!(
            visible_ids(&app),
            vec!["a-head", "a-child", "b-head"],
            "the fold state names a lineage by its CONTENT, so reordering the \
             sessions vec must not move it onto a different lineage"
        );
        assert_eq!(
            hidden_for(&app, "b-head"),
            Some(1),
            "the lineage the user never opened is still folded"
        );
        assert_eq!(hidden_for(&app, "a-head"), None, "and theirs is still open");
    }

    #[test]
    fn a_reload_that_hides_the_selection_auto_expands_its_lineage() {
        // Before the hand-off there is no lineage at all: one file, one row.
        let mut app = app_all(vec![
            session_fork("mine", "/tmp/w", "fork-root", 100),
            session_fork("neighbour", "/tmp/w", "other-root", 50),
        ]);
        assert_eq!(app.selected.as_deref(), Some("mine"));
        assert_eq!(visible_ids(&app), vec!["mine", "neighbour"]);

        // Now a background hand-off forks the transcript. The autorefresh brings
        // in a NEWER copy sharing the root, which takes over as head and would
        // fold the user's own row away underneath their cursor.
        app.apply_sessions(vec![
            session_fork("mine", "/tmp/w", "fork-root", 100),
            session_fork("neighbour", "/tmp/w", "other-root", 50),
            session_fork("forked-off", "/tmp/w", "fork-root", 200),
        ]);

        assert_eq!(
            app.selected.as_deref(),
            Some("mine"),
            "the selected session still exists, so a fork appearing in the \
             background must never move the user's place"
        );
        assert_eq!(
            visible_ids(&app),
            vec!["forked-off", "mine", "neighbour"],
            "its lineage auto-expands, rather than the selection clamping to a \
             neighbour"
        );
    }

    #[test]
    fn scope_toggle_preserves_expand_state() {
        let here = unique_temp_dir("fold-scope");
        let launch = resolve_dir(&here);
        let cwd = here.to_str().unwrap();
        let mut app = App::new(
            vec![
                session_fork("ancestor", cwd, "fork-root", 100),
                session_fork("bg", cwd, "fork-root", 200),
                session_fork("out-head", "/tmp/somewhere-else", "out-root", 60),
                session_fork("out-child", "/tmp/somewhere-else", "out-root", 50),
            ],
            Scope::All,
            launch,
        );
        // The board starts in the all scope, so it is a `-a` board: the flag
        // that put it there is also what keeps that scope on the key.
        app.all_scope_enabled = true;
        // Open the in-folder lineage; leave the out-of-folder one alone.
        app.expand_selected();
        assert_eq!(visible_ids(&app), vec!["bg", "ancestor", "out-head"]);

        // Stand on a row the scope is about to drop, so the restore path runs
        // against a selection that is GONE rather than merely folded.
        app.move_selection(2);
        assert_eq!(app.selected.as_deref(), Some("out-head"));

        // Walk the WHOLE three-scope cycle back to where it started, so every
        // step of the key is covered rather than just the first two.
        app.toggle_scope();
        assert_eq!(app.scope, Scope::CurrentFolder);
        assert_eq!(
            visible_ids(&app),
            vec!["bg", "ancestor"],
            "scoping to the folder drops the outside rows, but says nothing \
             about lineages and must not re-fold the one the user opened"
        );

        app.toggle_scope();
        assert_eq!(app.scope, Scope::Project);
        assert_eq!(
            visible_ids(&app),
            vec!["bg", "ancestor"],
            "the project scope spans the launch dir's repo ROOT, and the \
             outside rows sit in an unrelated root, so neither arm admits them \
             — and it likewise leaves the fold alone"
        );

        app.toggle_scope();
        assert_eq!(app.scope, Scope::All);
        assert_eq!(
            visible_ids(&app),
            vec!["bg", "ancestor", "out-head"],
            "back in the all scope the open lineage is still open AND the \
             untouched one is still folded: a row leaving the scope must never \
             unfold its own lineage behind the user's back"
        );

        let _ = std::fs::remove_dir_all(&here);
    }

    fn unique_temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!("snapback-app-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    // --- selection preservation by id across a reload ---------------------

    #[test]
    fn reload_preserves_selection_by_id_not_index() {
        // Distinct single-session groups; All-scope display order is driven by
        // each group's timestamp, most-recent first -> [a, b, c, d].
        let v1 = vec![
            session_ts("a", "r1", Some("main"), "/tmp/a", 400),
            session_ts("b", "r2", Some("main"), "/tmp/b", 300),
            session_ts("c", "r3", Some("main"), "/tmp/c", 200),
            session_ts("d", "r4", Some("main"), "/tmp/d", 100),
        ];
        let mut app = app_all(v1);
        // Select "c" at display position 2.
        app.move_selection(2);
        assert_eq!(app.selected.as_deref(), Some("c"));
        assert_eq!(app.selected_pos(), Some(2));

        // Reload with the SAME ids but new timestamps that float "c" to the top
        // (display order becomes [c, d, b, a]); the raw array index of "c" (2)
        // no longer matches its display position.
        let v2 = vec![
            session_ts("a", "r1", Some("main"), "/tmp/a", 100),
            session_ts("b", "r2", Some("main"), "/tmp/b", 200),
            session_ts("c", "r3", Some("main"), "/tmp/c", 400),
            session_ts("d", "r4", Some("main"), "/tmp/d", 300),
        ];
        app.apply_sessions(v2);

        // The stable id survived; its position tracked the new display order.
        assert_eq!(
            app.selected.as_deref(),
            Some("c"),
            "selection must follow the stable id across a reorder"
        );
        assert_eq!(app.selected_pos(), Some(0));
    }

    #[test]
    fn reload_clamps_to_nearest_when_selected_id_vanishes() {
        // Display order [a, b, c, d] by timestamp desc.
        let v1 = vec![
            session_ts("a", "r1", Some("main"), "/tmp/a", 400),
            session_ts("b", "r2", Some("main"), "/tmp/b", 300),
            session_ts("c", "r3", Some("main"), "/tmp/c", 200),
            session_ts("d", "r4", Some("main"), "/tmp/d", 100),
        ];
        let mut app = app_all(v1);
        // Select "c" at display position 2.
        app.move_selection(2);
        assert_eq!(app.selected.as_deref(), Some("c"));
        assert_eq!(app.selected_pos(), Some(2));

        // Reload with "c" removed; display order is [a, b, d]. Previous position
        // 2 clamps to the last row, which is "d" — the nearest surviving row.
        let v3 = vec![
            session_ts("a", "r1", Some("main"), "/tmp/a", 400),
            session_ts("b", "r2", Some("main"), "/tmp/b", 300),
            session_ts("d", "r4", Some("main"), "/tmp/d", 100),
        ];
        app.apply_sessions(v3);
        assert_eq!(
            app.selected.as_deref(),
            Some("d"),
            "a vanished id must clamp to the nearest surviving row by position"
        );
        assert_eq!(app.selected_pos(), Some(2));
    }

    // --- soft-hide (persisted visibility) ---------------------------------

    /// Task 3.2: the persisted hidden set is a PRESENTATION filter. A hidden id
    /// is dropped from the visible list by default and kept (for the view to
    /// mark) when the show-hidden toggle is on.
    #[test]
    fn a_hidden_id_is_excluded_by_default_and_present_when_show_hidden() {
        let _guard = crate::config::env_lock();
        let dir = unique_temp_dir("hidden-filter");
        std::env::set_var("SNAPBACK_CONFIG_DIR", &dir);

        // Session ids are prefixed so a leaked `SNAPBACK_CONFIG_DIR` (this test
        // sets it process-wide) can never collide with another concurrent test's
        // session ids, since `App::new` loads the hidden set from that env.
        let mut app = app_all(vec![
            session("sbhide-a", "r1", Some("main"), "/tmp/a"),
            session("sbhide-b", "r2", Some("main"), "/tmp/b"),
            session("sbhide-c", "r3", Some("main"), "/tmp/c"),
        ]);
        assert_eq!(
            visible_ids(&app),
            vec!["sbhide-a", "sbhide-b", "sbhide-c"],
            "nothing is hidden yet, so every session is visible"
        );

        // Hide the middle session in the persisted set and re-filter.
        app.hidden_ids.insert("sbhide-b".to_string());
        app.recompute_filtered();
        assert_eq!(
            visible_ids(&app),
            vec!["sbhide-a", "sbhide-c"],
            "a hidden id is dropped from the board by default"
        );

        // Reveal hidden rows: the hidden id returns, still in display order, so
        // the view can render it dimmed and marked.
        app.show_hidden = true;
        app.recompute_filtered();
        assert_eq!(
            visible_ids(&app),
            vec!["sbhide-a", "sbhide-b", "sbhide-c"],
            "show_hidden keeps hidden rows in the visible set"
        );

        std::env::remove_var("SNAPBACK_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Task 3.3: hiding the SELECTED row shrinks the visible list and clamps the
    /// selection to the nearest surviving row (never auto-revealing the hidden
    /// row — the contrast with the fold's `reveal_hidden`); toggling show-hidden
    /// brings the row back without un-hiding it.
    #[test]
    fn hiding_the_selected_row_clamps_selection_and_show_hidden_restores_it() {
        let _guard = crate::config::env_lock();
        let dir = unique_temp_dir("hide-selected");
        std::env::set_var("SNAPBACK_CONFIG_DIR", &dir);

        // Prefixed ids: see the note in the filter test above (leaked env dir).
        let mut app = app_all(vec![
            session("sbhide-a", "r1", Some("main"), "/tmp/a"),
            session("sbhide-b", "r2", Some("main"), "/tmp/b"),
            session("sbhide-c", "r3", Some("main"), "/tmp/c"),
        ]);
        // Stand on the LAST row so hiding it must clamp, not stay put.
        app.move_selection(2);
        assert_eq!(app.selected.as_deref(), Some("sbhide-c"));

        app.toggle_hidden_selected();
        assert_eq!(
            visible_ids(&app),
            vec!["sbhide-a", "sbhide-b"],
            "hiding the selected row shrinks the visible list"
        );
        assert_eq!(
            app.selected.as_deref(),
            Some("sbhide-b"),
            "the selection clamps to the nearest surviving row, and the hidden \
             row is NOT auto-revealed to keep the cursor on it"
        );

        // Reveal hidden rows: the hidden row is back, but it stays HIDDEN in the
        // persisted set (show-hidden reveals, it does not un-hide).
        app.toggle_show_hidden();
        assert_eq!(
            visible_ids(&app),
            vec!["sbhide-a", "sbhide-b", "sbhide-c"],
            "toggling show-hidden brings the hidden row back onto the board"
        );
        assert!(
            app.hidden_ids.contains("sbhide-c"),
            "revealing a hidden row must not remove it from the persisted set"
        );

        std::env::remove_var("SNAPBACK_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Hiding a folded fork lineage hides EVERY nested member, and exposing brings
    /// the whole lineage back — the family flips as a unit and never sheds only its
    /// head (which would let the fold re-head to a surviving fork).
    #[test]
    fn ctrl_x_x_hides_and_exposes_a_whole_fork_lineage_together() {
        let _guard = crate::config::env_lock();
        let dir = unique_temp_dir("hide-lineage");
        std::env::set_var("SNAPBACK_CONFIG_DIR", &dir);

        // A 3-fork lineage (shared root) folds to one visible head; an unrelated
        // singleton must NOT be swept in.
        let mut app = app_all(vec![
            session_fork("sbfork-new", "/tmp/p", "root-1", 300), // newest → head
            session_fork("sbfork-mid", "/tmp/p", "root-1", 200),
            session_fork("sbfork-old", "/tmp/p", "root-1", 100),
            session_ts("sbsolo", "repo", Some("main"), "/tmp/p", 50),
        ]);
        assert!(
            visible_ids(&app).contains(&"sbfork-new"),
            "the folded lineage head is on the board before hiding"
        );

        // Hide the folded lineage head.
        app.set_selected(Some("sbfork-new".to_string()));
        app.toggle_hidden_selected();

        for member in ["sbfork-new", "sbfork-mid", "sbfork-old"] {
            assert!(
                app.hidden_ids.contains(member),
                "{member} must be hidden with the rest of its lineage"
            );
        }
        let visible = visible_ids(&app);
        assert!(
            !["sbfork-new", "sbfork-mid", "sbfork-old"]
                .iter()
                .any(|m| visible.contains(m)),
            "no lineage member survives on the board: {visible:?}"
        );
        assert!(
            visible.contains(&"sbsolo"),
            "the unrelated singleton is untouched"
        );

        // Reveal, then expose the lineage from its head: every member clears.
        app.toggle_show_hidden();
        app.set_selected(Some("sbfork-new".to_string()));
        app.toggle_hidden_selected();
        assert!(
            ["sbfork-new", "sbfork-mid", "sbfork-old"]
                .iter()
                .all(|m| !app.hidden_ids.contains(*m)),
            "exposing the lineage clears every member from the hidden set"
        );

        std::env::remove_var("SNAPBACK_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Task 3.4: a hide is PERSISTED. After hiding through the public toggle, a
    /// fresh `App` built over the same `SNAPBACK_CONFIG_DIR` loads the hide at
    /// construction, so the session stays hidden across a restart.
    #[test]
    fn a_hidden_session_survives_a_rebuild_from_the_same_state_dir() {
        let _guard = crate::config::env_lock();
        let dir = unique_temp_dir("hide-persist");
        std::env::set_var("SNAPBACK_CONFIG_DIR", &dir);

        // Prefixed ids: see the note in the filter test above (leaked env dir).
        let sessions = vec![
            session("sbhide-a", "r1", Some("main"), "/tmp/a"),
            session("sbhide-b", "r2", Some("main"), "/tmp/b"),
        ];
        // Hide the second session through the public toggle, which writes to the
        // injected dir.
        let mut app = app_all(sessions.clone());
        app.move_selection(1);
        assert_eq!(app.selected.as_deref(), Some("sbhide-b"));
        app.toggle_hidden_selected();
        assert!(app.hidden_ids.contains("sbhide-b"));

        // A fresh App over the SAME sessions and the SAME state dir must read the
        // hide back from disk at construction — before any interaction.
        let rebuilt = app_all(sessions);
        assert!(
            rebuilt.hidden_ids.contains("sbhide-b"),
            "the persisted hide is loaded by App::new from the state dir"
        );
        assert_eq!(
            visible_ids(&rebuilt),
            vec!["sbhide-a"],
            "a session hidden in a previous run stays hidden across a restart"
        );

        std::env::remove_var("SNAPBACK_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- preview scroll anchoring + bounds --------------------------------

    #[test]
    fn a_new_app_anchors_the_preview_to_the_bottom() {
        let app = app_all(vec![session("a", "r1", Some("main"), "/tmp/a")]);
        assert!(
            app.preview_follow_bottom,
            "a fresh app must open the preview at the newest turn"
        );
        assert_eq!(app.preview_scroll, 0);
    }

    #[test]
    fn changing_selection_reanchors_the_preview_to_the_bottom() {
        let mut app = app_all(vec![
            session("a", "r1", Some("main"), "/tmp/a"),
            session("b", "r2", Some("main"), "/tmp/b"),
        ]);
        // Simulate the user having scrolled up in the current preview.
        app.preview_follow_bottom = false;
        app.preview_scroll = 7;

        // Selecting a DIFFERENT session re-anchors to the bottom.
        app.move_selection(1);
        assert_ne!(app.selected.as_deref(), Some("a"));
        assert!(
            app.preview_follow_bottom,
            "a selection change must re-anchor the preview to the bottom"
        );
        assert_eq!(app.preview_scroll, 0);
    }

    #[test]
    fn reselecting_the_same_session_keeps_manual_scroll() {
        let mut app = app_all(vec![session("a", "r1", Some("main"), "/tmp/a")]);
        app.preview_follow_bottom = false;
        app.preview_scroll = 4;
        // A no-op move (already at the only row) must not disturb the scroll.
        app.move_selection(0);
        assert!(!app.preview_follow_bottom, "same id must not re-anchor");
        assert_eq!(app.preview_scroll, 4);
    }

    #[test]
    fn toggling_the_preview_on_reanchors_to_the_bottom() {
        let mut app = app_all(vec![session("a", "r1", Some("main"), "/tmp/a")]);
        app.preview_follow_bottom = false;
        app.preview_scroll = 9;
        app.toggle_preview(); // hide
        app.toggle_preview(); // show again
        assert!(app.show_preview);
        assert!(
            app.preview_follow_bottom,
            "re-showing the preview re-anchors it to the newest turn"
        );
        assert_eq!(app.preview_scroll, 0);
    }

    #[test]
    fn preview_scroll_keys_clear_follow_and_saturate_at_bounds() {
        let mut app = app_all(vec![session("a", "r1", Some("main"), "/tmp/a")]);
        app.preview_viewport_h = 10; // set by the view; sizes a page

        app.preview_page_down();
        assert!(
            !app.preview_follow_bottom,
            "an explicit scroll drops follow"
        );
        assert_eq!(app.preview_scroll, 10, "a page is one viewport height");

        app.preview_half_up();
        assert_eq!(
            app.preview_scroll, 8,
            "Ctrl-U scrolls a quarter page (viewport/4)"
        );
        app.preview_half_down();
        assert_eq!(
            app.preview_scroll, 10,
            "Ctrl-D scrolls a quarter page back down (viewport/4)"
        );

        app.preview_page_up();
        assert_eq!(app.preview_scroll, 0, "cannot scroll above the top");
        app.preview_page_up();
        assert_eq!(app.preview_scroll, 0, "top saturates (no underflow)");

        app.preview_top();
        assert!(!app.preview_follow_bottom);
        assert_eq!(app.preview_scroll, 0);

        app.preview_bottom();
        assert!(
            app.preview_follow_bottom,
            "End re-enables follow-bottom so the view pins to the newest turn"
        );
    }

    // --- preview cache: the wrapped height rides with the text it describes --

    /// A session whose transcript is a checked-in fixture, so the cache has real
    /// rendered turns to measure rather than an empty `Text` (a synthetic
    /// [`session`] points at a `/tmp` path that does not exist, which renders to
    /// nothing — every height below would be 0 and every assertion vacuous).
    fn fixture_session(id: &str, folder: &str, file: &str) -> Session {
        let mut s = session(id, "project", Some("main"), "/tmp/project");
        s.file = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("store")
            .join(folder)
            .join(file);
        s
    }

    /// The longer of the two fixture transcripts (a full four-turn session).
    const LONG_FIXTURE: (&str, &str) = ("-Users-me-project-alpha", "sess-normal-1.jsonl");
    /// The shorter one, so a reload can make a session visibly GROW.
    const SHORT_FIXTURE: (&str, &str) = ("-Users-me-project-beta", "sess-nosummary-1.jsonl");

    /// The cached row count always describes the cached text: same session, same
    /// width, measured the same way the pane measures what it draws.
    ///
    /// This is the whole reason the count lives inside the cache entry instead of a
    /// map beside it — the two cannot be invalidated apart.
    #[test]
    fn the_cached_preview_height_describes_the_cached_preview_text() {
        let (folder, file) = LONG_FIXTURE;
        let mut app = app_all(vec![fixture_session("s1", folder, file)]);
        for width in [80u16, 40, 12] {
            let text = app.preview_text(width);
            assert_eq!(
                app.preview_wrapped_rows(width),
                view::wrapped_text_rows(&text.lines, width),
                "the cached height must be the height of the cached text at {width}"
            );
        }
    }

    /// A width change RE-MEASURES the height rather than serving the one cached for
    /// the old width — the same invalidation the text itself gets, since a narrower
    /// pane both re-lays the text out and re-wraps it.
    #[test]
    fn a_width_change_remeasures_the_cached_preview_height() {
        let (folder, file) = LONG_FIXTURE;
        let mut app = app_all(vec![fixture_session("s1", folder, file)]);

        let wide = app.preview_wrapped_rows(60);
        let narrow = app.preview_wrapped_rows(12);
        assert!(
            narrow > wide,
            "a narrower pane must wrap the same transcript into more rows \
             (wide={wide}, narrow={narrow})"
        );
        assert_eq!(
            app.preview_wrapped_rows(60),
            wide,
            "and widening back must return the wide answer, not a sticky one"
        );
    }

    /// A reload DROPS the cached height with the text it was measured from.
    ///
    /// The transcript on disk grows while the board is open (that is what a live
    /// agent does), so a height that outlived its reload would keep the pane's bottom
    /// anchored to a session that is no longer there.
    #[test]
    fn a_reload_drops_the_cached_preview_height() {
        let (short_folder, short_file) = SHORT_FIXTURE;
        let (long_folder, long_file) = LONG_FIXTURE;
        let mut app = app_all(vec![fixture_session("s1", short_folder, short_file)]);

        let before = app.preview_wrapped_rows(40);
        // The SAME session id, now backed by a longer transcript: the shape of a
        // session that gained turns between reloads.
        app.apply_sessions(vec![fixture_session("s1", long_folder, long_file)]);
        let after = app.preview_wrapped_rows(40);
        assert!(
            after > before,
            "a reloaded transcript must be re-measured, not answered from the \
             pre-reload cache (before={before}, after={after})"
        );
    }

    // --- splitter drag: clamp math + state machine -------------------------

    #[test]
    fn clamp_list_width_clamps_to_the_minimum_on_both_sides() {
        // Dragging far left clamps to MIN_PANE_WIDTH.
        assert_eq!(clamp_list_width(0, 100), MIN_PANE_WIDTH);
        // Dragging far right leaves the preview MIN_PANE_WIDTH columns.
        assert_eq!(clamp_list_width(1000, 100), 100 - MIN_PANE_WIDTH);
        // A mid-range request passes through untouched.
        assert_eq!(clamp_list_width(40, 100), 40);
    }

    #[test]
    fn clamp_list_width_degrades_to_a_half_split_when_too_narrow_for_both_minimums() {
        // A body narrower than 2*MIN_PANE_WIDTH cannot fit both minimums;
        // falling back to an even half-split never inverts or underflows.
        let body = MIN_PANE_WIDTH; // less than MIN_PANE_WIDTH * 2
        let width = clamp_list_width(0, body);
        assert_eq!(width, body / 2);
        assert!(width <= body, "must never exceed the body width");
    }

    #[test]
    fn resolve_list_width_defaults_to_48_percent_until_a_drag_sets_one() {
        assert_eq!(resolve_list_width(None, 100), 48);
        // A stale drag width from a WIDER terminal re-clamps to the current,
        // narrower body rather than overflowing it.
        assert_eq!(resolve_list_width(Some(90), 40), 40 - MIN_PANE_WIDTH);
        // A drag width that already fits passes through untouched.
        assert_eq!(resolve_list_width(Some(30), 100), 30);
    }

    #[test]
    fn begin_split_drag_is_a_no_op_while_the_live_choice_overlay_is_open() {
        let mut app = app_all(vec![session("a", "r1", Some("main"), "/tmp/a")]);
        app.open_live_choice("a".to_string());
        app.begin_split_drag();
        assert!(
            !app.is_dragging_split(),
            "a stray click during the overlay must not start a drag"
        );
    }

    #[test]
    fn begin_split_drag_starts_dragging_when_no_overlay_is_open() {
        let mut app = app_all(vec![session("a", "r1", Some("main"), "/tmp/a")]);
        assert!(!app.is_dragging_split());
        app.begin_split_drag();
        assert!(app.is_dragging_split());
    }

    #[test]
    fn drag_split_to_never_inverts_or_panics_on_a_narrow_body() {
        let mut app = app_all(vec![session("a", "r1", Some("main"), "/tmp/a")]);
        // Dragging far left/right on a normal body clamps to the minimums.
        app.drag_split_to(0, 100);
        assert_eq!(app.list_width, Some(MIN_PANE_WIDTH));
        app.drag_split_to(u16::MAX, 100);
        assert_eq!(app.list_width, Some(100 - MIN_PANE_WIDTH));
        // A degenerate (very narrow) body must never panic or invert.
        app.drag_split_to(5, 3);
        assert_eq!(app.list_width, Some(1));
        // A zero-width body (e.g. before the first render) is also safe.
        app.drag_split_to(5, 0);
        assert_eq!(app.list_width, Some(0));
    }

    #[test]
    fn end_split_drag_clears_the_flag_even_without_a_preceding_begin() {
        let mut app = app_all(vec![session("a", "r1", Some("main"), "/tmp/a")]);
        assert!(!app.is_dragging_split());
        app.end_split_drag(); // defensive: no panic, stays cleared
        assert!(!app.is_dragging_split());

        app.begin_split_drag();
        assert!(app.is_dragging_split());
        app.end_split_drag();
        assert!(!app.is_dragging_split());
    }

    #[test]
    fn reload_to_empty_clears_selection() {
        let mut app = app_all(vec![session("a", "r1", Some("main"), "/tmp/a")]);
        assert_eq!(app.selected.as_deref(), Some("a"));
        app.apply_sessions(vec![]);
        assert_eq!(app.selected, None, "an empty reload clears the selection");
        assert!(app.filtered.is_empty());
    }

    // --- scope predicate (exact canonical cwd match) ----------------------

    /// The worktree set a scope test uses when it is not about worktrees: the
    /// UNRESOLVED one, which is also what every `App::new` starts with under
    /// test.
    fn no_worktrees() -> WorktreeSet {
        WorktreeSet::empty()
    }

    #[test]
    fn scope_predicate_matches_exact_canonical_cwd() {
        let here = unique_temp_dir("scope-here");
        let other = unique_temp_dir("scope-other");
        let launch = resolve_dir(&here);

        let inside = session("in", "r", Some("main"), here.to_str().unwrap());
        let outside = session("out", "r", Some("main"), other.to_str().unwrap());

        // Current-folder: only the exact-cwd session is in scope.
        assert!(in_scope(
            Scope::CurrentFolder,
            &inside,
            &launch,
            &no_worktrees()
        ));
        assert!(!in_scope(
            Scope::CurrentFolder,
            &outside,
            &launch,
            &no_worktrees()
        ));

        // All: everything is in scope.
        assert!(in_scope(Scope::All, &inside, &launch, &no_worktrees()));
        assert!(in_scope(Scope::All, &outside, &launch, &no_worktrees()));

        let _ = std::fs::remove_dir_all(&here);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn scope_predicate_falls_back_to_raw_path_for_missing_cwd() {
        // A deleted worktree: the cwd no longer resolves, so resolve_dir falls
        // back to the raw path and an exact raw match still scopes it in.
        let launch = PathBuf::from("/nonexistent/snapback-test/gone");
        let gone = session("gone", "r", Some("main"), "/nonexistent/snapback-test/gone");
        let elsewhere = session(
            "else",
            "r",
            Some("main"),
            "/nonexistent/snapback-test/other",
        );
        assert!(in_scope(
            Scope::CurrentFolder,
            &gone,
            &launch,
            &no_worktrees()
        ));
        assert!(!in_scope(
            Scope::CurrentFolder,
            &elsewhere,
            &launch,
            &no_worktrees()
        ));
    }

    // --- project scope (membership in the launch project's worktrees) -----

    /// The reason the cross-worktree scope exists: a session started in a
    /// SIBLING worktree of the same project is in scope even though its cwd is
    /// nowhere near the launch dir — the exact-match scope can never say yes to
    /// it, and no path heuristic could relate the two folders either.
    #[test]
    fn project_scope_matches_any_worktree_of_the_project() {
        let main = unique_temp_dir("project-main");
        let sibling = unique_temp_dir("project-sibling");
        let stranger = unique_temp_dir("project-stranger");
        let launch = resolve_dir(&main);
        // Seeded ALREADY CANONICALIZED, exactly as `worktrees::resolve` hands
        // them over — raw temp paths would not compare equal on a platform whose
        // temp dir is a symlink (macOS `/tmp` -> `/private/tmp`).
        let worktrees = WorktreeSet::from_resolved(
            [resolve_dir(&main), resolve_dir(&sibling)],
            Some("acme/web".to_string()),
        );

        let here = session("here", "r", Some("main"), main.to_str().unwrap());
        let next_door = session("next", "r", Some("feature"), sibling.to_str().unwrap());
        let outsider = session("out", "r", Some("main"), stranger.to_str().unwrap());

        assert!(in_scope(Scope::Project, &here, &launch, &worktrees));
        assert!(
            in_scope(Scope::Project, &next_door, &launch, &worktrees),
            "a sibling worktree of the same project is the whole case this \
             scope exists for"
        );
        assert!(
            !in_scope(Scope::Project, &outsider, &launch, &worktrees),
            "a folder outside the set stays out: `Project` is not `All`"
        );
        assert!(
            !in_scope(Scope::CurrentFolder, &next_door, &launch, &worktrees),
            "premise: the exact-cwd scope still refuses the sibling, so the \
             assertion above is about `Project` and not about the fixture"
        );

        let _ = std::fs::remove_dir_all(&main);
        let _ = std::fs::remove_dir_all(&sibling);
        let _ = std::fs::remove_dir_all(&stranger);
    }

    /// FAIL-SOFT: no git, not a repository, or a non-zero exit all arrive here as
    /// an EMPTY set, and an empty set means "could not resolve" — never "nothing
    /// matches". The repo-root arm needs no git, so the scope still spans the
    /// whole repo; it just cannot see worktrees parked outside it.
    ///
    /// This REPLACES an assertion that an unresolved set made `Project` behave
    /// byte-for-byte like `CurrentFolder`. That contract is deliberately gone —
    /// see [`in_scope`] — and it is the launch dir's SIBLING WORKTREE, not the
    /// unrelated repo, that tells the two readings apart.
    #[test]
    fn project_scope_without_a_resolved_set_still_spans_the_repo_root() {
        let repo = unique_temp_dir("project-unresolved-repo");
        let other = unique_temp_dir("project-unresolved-other");
        let launch = resolve_dir(&repo);
        let unresolved = WorktreeSet::empty();
        let sibling = repo.join(".wtp/worktrees/feature/sibling");

        let inside = session("in", "r", Some("main"), repo.to_str().unwrap());
        let next_door = session("next", "r", Some("feature"), sibling.to_str().unwrap());
        let outside = session("out", "r", Some("main"), other.to_str().unwrap());

        assert!(
            in_scope(Scope::Project, &inside, &launch, &unresolved),
            "the launch dir itself is never lost, whatever git says"
        );
        assert!(
            in_scope(Scope::Project, &next_door, &launch, &unresolved),
            "and a sibling worktree of the same repo is in WITHOUT git — this is \
             the arm that carries the scope when nothing resolved"
        );
        assert!(
            !in_scope(Scope::CurrentFolder, &next_door, &launch, &unresolved),
            "premise: the exact-cwd scope refuses that sibling, so the assertion \
             above is a real difference and not a property of the fixture"
        );
        assert!(
            !in_scope(Scope::Project, &outside, &launch, &unresolved),
            "an unrelated repo stays out: `Project` is still not `All`"
        );

        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&other);
    }

    /// THE CASE THIS WIDENING EXISTS FOR: a worktree that has been REMOVED. Git
    /// reports what exists now, so those sessions match no live root — on this
    /// repo's own store that was a third of the project's history, reachable only
    /// under `All`. The repo-root arm keeps them in the project.
    ///
    /// The fixture also pins the LABEL TRAP inside the same premise: the repo and
    /// its worktree carry DIFFERENT `repo_of` labels, so a scope built on label
    /// equality would answer "different project" for two folders of one project.
    /// Roots are what get compared.
    #[test]
    fn project_scope_keeps_a_deleted_worktrees_sessions_in_the_project() {
        use crate::store::group;

        let repo = unique_temp_dir("deleted-wt-repo");
        let stranger = unique_temp_dir("deleted-wt-stranger");
        let launch = resolve_dir(&repo);
        // Never created: this is a worktree the user has since removed.
        let deleted = repo.join(".wtp/worktrees/feature/gone");
        // Git only knows the main checkout now.
        let live = WorktreeSet::from_resolved([launch.clone()], Some("acme/web".to_string()));

        let gone = session("gone", "r", Some("feature"), deleted.to_str().unwrap());
        let outsider = session("out", "r", Some("main"), stranger.to_str().unwrap());

        assert!(!deleted.exists(), "premise: the worktree is really gone");
        assert!(
            !live.contains(&resolve_dir(&deleted)),
            "premise: git cannot report a worktree that no longer exists, so the \
             live-set arm says NO and the assertion below is about the other arm"
        );
        assert_ne!(
            group::repo_of(&repo),
            group::repo_of(&deleted),
            "premise: the two folders' LABELS disagree ({} vs {}), so comparing \
             labels here would be silently wrong",
            group::repo_of(&repo),
            group::repo_of(&deleted)
        );

        assert!(
            in_scope(Scope::Project, &gone, &launch, &live),
            "a removed worktree's sessions stay in their project"
        );
        assert!(
            !in_scope(Scope::CurrentFolder, &gone, &launch, &live),
            "premise: the exact-cwd scope still refuses it"
        );
        assert!(
            !in_scope(Scope::Project, &outsider, &launch, &live),
            "and widening to the repo root admits the repo, not the filesystem"
        );

        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&stranger);
    }

    /// The two arms are INDEPENDENT, and this is the half the root rule cannot
    /// do: `git worktree add` accepts any path, so a live worktree may sit
    /// nowhere near the repo. Only git can relate it — which is why the live-set
    /// arm stays even though the root arm now carries the fail-soft case.
    #[test]
    fn project_scope_still_takes_a_live_worktree_parked_outside_the_repo_root() {
        let repo = unique_temp_dir("outside-root-repo");
        let elsewhere = unique_temp_dir("outside-root-elsewhere");
        let launch = resolve_dir(&repo);
        let live = WorktreeSet::from_resolved(
            [launch.clone(), resolve_dir(&elsewhere)],
            Some("acme/web".to_string()),
        );

        let far = session("far", "r", Some("feature"), elsewhere.to_str().unwrap());

        assert_ne!(
            project_root(&elsewhere),
            project_root(&launch),
            "premise: no path rule relates these two folders"
        );
        assert!(
            in_scope(Scope::Project, &far, &launch, &live),
            "git said it is a worktree of this project, so it is in"
        );

        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    /// One key, three scopes, WIDENING at every step and then wrapping — so the
    /// user walks outward from the exact folder to the whole store and back
    /// without the key ever meaning "narrow" partway through.
    ///
    /// This is the `--all`/`-a` shape. The default shape is the flip pinned by
    /// [`the_all_scope_is_off_the_key_without_its_launch_flag`].
    #[test]
    fn scope_cycles_current_folder_then_project_then_all() {
        assert_eq!(Scope::CurrentFolder.toggled(true), Scope::Project);
        assert_eq!(Scope::Project.toggled(true), Scope::All);
        assert_eq!(Scope::All.toggled(true), Scope::CurrentFolder);
        assert_eq!(
            Scope::CurrentFolder
                .toggled(true)
                .toggled(true)
                .toggled(true),
            Scope::CurrentFolder,
            "the cycle is CLOSED: three presses land where they started, so no \
             scope can be entered and not left"
        );
    }

    /// The DEFAULT shape of the key: a two-state flip, with the whole store
    /// unreachable from inside the board.
    ///
    /// `Scope::All` is every session of every repo on the machine — the widest,
    /// least often wanted answer — and it used to sit MID-cycle, one stray press
    /// away. It is now the launch flag's alone.
    ///
    /// The `All` arm is asserted too, even though the flip cannot produce that
    /// state: a total function must answer for it, and the answer must be the
    /// one that LEAVES the widest scope. Returning `All` there would strand a
    /// board that reached it by any other route — a `-a` flag read from stale
    /// state, a future caller, a bug — in a scope with no key out.
    #[test]
    fn the_all_scope_is_off_the_key_without_its_launch_flag() {
        assert_eq!(Scope::CurrentFolder.toggled(false), Scope::Project);
        assert_eq!(
            Scope::Project.toggled(false),
            Scope::CurrentFolder,
            "the flip wraps HERE without `-a`, instead of reaching the store"
        );
        assert_eq!(
            Scope::CurrentFolder.toggled(false).toggled(false),
            Scope::CurrentFolder,
            "two presses close it, so the flip is a cycle of exactly two"
        );
        assert_eq!(
            Scope::All.toggled(false),
            Scope::CurrentFolder,
            "and an `All` reached some other way still has a way OUT"
        );
    }

    /// The WIRING half of the two tests above, and it is not redundant with
    /// them: the pure cycle can be exactly right while `toggle_scope` hands it a
    /// constant `true`, which would leave the whole store on the key for every
    /// launch — the precise bug this change exists to remove. So the key is
    /// pressed here, against a board, in both flag states.
    #[test]
    fn the_board_key_reaches_the_all_scope_only_when_the_launch_flag_set_it() {
        let mut app = App::new(Vec::new(), Scope::CurrentFolder, PathBuf::from("/tmp"));
        assert!(!app.all_scope_enabled, "premise: this board saw no `-a`");

        app.toggle_scope();
        assert_eq!(app.scope, Scope::Project);
        app.toggle_scope();
        assert_eq!(
            app.scope,
            Scope::CurrentFolder,
            "two presses come back around, never reaching the store"
        );

        app.all_scope_enabled = true;
        app.toggle_scope();
        assert_eq!(app.scope, Scope::Project);
        app.toggle_scope();
        assert_eq!(
            app.scope,
            Scope::All,
            "and only the launch flag puts the store back on the key"
        );
    }

    /// `recompute_scope` runs on a KEYSTROKE, so it may not resolve git — the
    /// off-UI-thread rule. It reads the set cached at launch instead, which is
    /// what this pins: a probe installed afterwards is neither called nor
    /// believed until a refresh path asks it.
    #[test]
    fn toggling_into_the_project_scope_never_re_resolves_the_worktree_set() {
        let here = unique_temp_dir("project-cached-here");
        let sibling = unique_temp_dir("project-cached-sibling");
        let launch = resolve_dir(&here);
        let sessions = vec![
            session("here", "r", Some("main"), here.to_str().unwrap()),
            session("sib", "r", Some("feature"), sibling.to_str().unwrap()),
        ];
        let mut app = App::new(sessions, Scope::CurrentFolder, launch);

        // A probe that WOULD widen the scope, counting how often it is asked.
        let calls = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let asked = std::rc::Rc::clone(&calls);
        let wide = WorktreeSet::from_resolved([resolve_dir(&here), resolve_dir(&sibling)], None);
        app.set_worktree_probe(move |_| {
            asked.set(asked.get() + 1);
            wide.clone()
        });

        app.toggle_scope();

        assert_eq!(app.scope, Scope::Project);
        assert_eq!(
            calls.get(),
            0,
            "a scope keystroke must never spawn git: the set is resolved at \
             launch and on reload, never here"
        );
        assert_eq!(
            visible_ids(&app),
            vec!["here"],
            "and the scope therefore answers from the CACHED set — the sibling \
             the new probe would admit is still out"
        );

        let _ = std::fs::remove_dir_all(&here);
        let _ = std::fs::remove_dir_all(&sibling);
    }

    /// The other half of that rule, and the reason the set is re-resolved in
    /// `apply_sessions` at all: a worktree created AFTER launch has to join the
    /// project scope on the next reload, WITHOUT a restart. A set captured only
    /// in `App::new` would go stale the moment the user ran `git worktree add`,
    /// and the reload is the earliest moment a new worktree can matter — its
    /// first session is exactly what triggers one.
    ///
    /// The zero-probe-calls-on-keystroke half is pinned by
    /// [`toggling_into_the_project_scope_never_re_resolves_the_worktree_set`];
    /// this test asserts the POSITIVE side, and asserts the pre-reload state too
    /// so it cannot pass without the reload having changed anything.
    #[test]
    fn reloading_the_sessions_re_resolves_the_worktree_set() {
        let here = unique_temp_dir("project-reload-here");
        let sibling = unique_temp_dir("project-reload-sibling");
        let launch = resolve_dir(&here);
        let sessions = vec![
            session("here", "r", Some("main"), here.to_str().unwrap()),
            session("sib", "r", Some("feature"), sibling.to_str().unwrap()),
        ];
        // Launch while git still knows nothing of the sibling: the two temp dirs
        // are unrelated repo ROOTS, so only git could ever relate them, and the
        // test-default probe resolves an EMPTY set. `Project` admits `here` alone.
        let mut app = App::new(sessions.clone(), Scope::Project, launch);
        assert_eq!(
            visible_ids(&app),
            vec!["here"],
            "premise: the sibling is OUT before the reload, so the assertion \
             below cannot pass vacuously"
        );

        // `git worktree add` happened: git now reports the sibling too.
        let calls = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let asked = std::rc::Rc::clone(&calls);
        let wide = WorktreeSet::from_resolved([resolve_dir(&here), resolve_dir(&sibling)], None);
        app.set_worktree_probe(move |_| {
            asked.set(asked.get() + 1);
            wide.clone()
        });

        app.apply_sessions(sessions);

        assert_eq!(
            calls.get(),
            1,
            "a reload asks git ONCE — enough to pick the new worktree up, and \
             not once per session"
        );
        // Sorted: which worktree's group sorts first is display ordering, not
        // scope membership, and this test is about membership.
        let mut shown = visible_ids(&app);
        shown.sort_unstable();
        assert_eq!(
            shown,
            vec!["here", "sib"],
            "the worktree added after launch is in scope now, with no restart"
        );

        let _ = std::fs::remove_dir_all(&here);
        let _ = std::fs::remove_dir_all(&sibling);
    }

    #[test]
    fn current_folder_scope_filters_the_list() {
        let here = unique_temp_dir("filter-here");
        let launch = resolve_dir(&here);
        let sessions = vec![
            session("in", "r1", Some("main"), here.to_str().unwrap()),
            session("out", "r2", Some("main"), "/tmp/somewhere-else"),
        ];
        let app = App::new(sessions, Scope::CurrentFolder, launch);
        assert_eq!(
            app.filtered.len(),
            1,
            "only the current-folder session shows"
        );
        assert_eq!(app.selected.as_deref(), Some("in"));
        let _ = std::fs::remove_dir_all(&here);
    }

    #[test]
    fn current_folder_scope_excludes_out_of_scope_query_match() {
        let here = unique_temp_dir("scope-query-here");
        let launch = resolve_dir(&here);
        let inside = here.to_str().unwrap();

        // Two in-scope sessions and one out-of-scope, ALL matching the query
        // ("label" is in every default label, "label for <id>"). In-scope
        // timestamps: in-new (200) newer than in-old (100), so the current-folder
        // display order is [in-new, in-old]; the out-of-scope "out" is dropped.
        let sessions = vec![
            session_ts("in-old", "r1", Some("main"), inside, 100),
            session_ts("out", "r2", Some("main"), "/tmp/somewhere-else", 300),
            session_ts("in-new", "r1", Some("main"), inside, 200),
        ];
        let mut app = App::new(sessions, Scope::CurrentFolder, launch);
        for c in "label".chars() {
            app.push_query_char(c);
        }

        let shown: Vec<&str> = app
            .filtered
            .iter()
            .map(|&i| app.sessions[i].session_id.as_str())
            .collect();
        assert_eq!(
            shown,
            vec!["in-new", "in-old"],
            "out-of-scope match is excluded; in-scope matches keep timestamp-desc order"
        );

        let _ = std::fs::remove_dir_all(&here);
    }

    // --- header counter population (the denominator and its hidden split) --

    /// The three sessions the population cases are decided on: one in the launch
    /// folder, one in a worktree of the same project, one in another project
    /// entirely. `/tmp/sbpop-proj` exists nowhere, so `resolve_dir` hands every
    /// path back raw on BOTH sides of the comparison and the case needs no
    /// fixture on disk.
    fn population_store() -> Vec<Session> {
        vec![
            session("sbpop-here", "sbpop-proj", Some("main"), "/tmp/sbpop-proj"),
            session(
                "sbpop-wt",
                "sbpop-proj",
                Some("feat"),
                "/tmp/sbpop-proj/.agents/worktrees/feat",
            ),
            session("sbpop-away", "other", Some("main"), "/tmp/sbpop-other"),
        ]
    }

    /// A store shaped like the board that exposed the unit bug: a two-member fork
    /// lineage and a lone session, ALL in the launch folder — three FILES, two
    /// CONVERSATIONS. Any counter that reads files says three somewhere.
    fn lineage_population_store() -> Vec<Session> {
        let forked = |id: &str| {
            let mut s = session(id, "sbpop-proj", Some("main"), "/tmp/sbpop-proj");
            s.root_uuid = Some("sbpop-root".to_string());
            s
        };
        vec![
            forked("sbpop-fork-a"),
            forked("sbpop-fork-b"),
            session("sbpop-lone", "sbpop-proj", Some("main"), "/tmp/sbpop-proj"),
        ]
    }

    /// The counted population is the launch PROJECT while the board is narrowed
    /// to one folder — deliberately wider than the rows, so the header can say
    /// what a `Ctrl-A` would reveal — and the whole store ONLY under `All`,
    /// where the board is not about a project at all.
    #[test]
    fn the_counted_population_is_the_project_in_every_scope_but_all() {
        let launch = PathBuf::from("/tmp/sbpop-proj");

        let folder = App::new(population_store(), Scope::CurrentFolder, launch.clone());
        assert_eq!(
            folder.filtered.len(),
            1,
            "premise: the folder scope draws its one exact-cwd match"
        );
        assert_eq!(
            folder.session_counts().total,
            2,
            "but it COUNTS the project: the folder's session plus the worktree's"
        );

        let project = App::new(population_store(), Scope::Project, launch.clone());
        assert_eq!(
            project.session_counts().total,
            folder.session_counts().total,
            "widening to the project must not move the denominator"
        );
        assert_eq!(
            project.session_counts().visible,
            2,
            "only the NUMERATOR catches up with the population it was counting"
        );

        let all = App::new(population_store(), Scope::All, launch);
        assert_eq!(
            all.session_counts().total,
            3,
            "the all scope counts the store, the other project's session included"
        );
    }

    /// The user-visible fix. With an empty query and nothing hidden, a project-
    /// scoped board draws exactly the population it counts, so the two sides of
    /// the `/` MATCH — even though a folded fork lineage means three files sit
    /// behind those two rows. `115 / 146` was this assertion failing.
    #[test]
    fn the_project_scope_counter_reconciles_to_the_rows_on_screen() {
        for scope in [Scope::Project, Scope::All] {
            let app = App::new(
                lineage_population_store(),
                scope,
                PathBuf::from("/tmp/sbpop-proj"),
            );
            let counts = app.session_counts();

            assert_eq!(
                app.filtered.len(),
                2,
                "premise ({scope:?}): the fork pair folds to one head beside the lone row"
            );
            assert_eq!(
                counts.visible, counts.total,
                "an unqueried, unhidden board counts what it draws ({scope:?}): {counts:?}"
            );
            assert_eq!(
                counts.total, 2,
                "and both numbers are CONVERSATIONS, not the three files ({scope:?})"
            );
        }
    }

    /// Load-bearing: neither number may move when a lineage opens or closes. It
    /// is what stops the counter printing `112 / 111`, and it is not only a user
    /// action — `restore_selection` -> `reveal_hidden` auto-expands on
    /// autorefresh, so a fold-sensitive number would drift on its own the moment
    /// a background job wrote a file.
    #[test]
    fn expanding_and_collapsing_a_lineage_moves_neither_number() {
        let mut app = App::new(
            lineage_population_store(),
            Scope::Project,
            PathBuf::from("/tmp/sbpop-proj"),
        );
        let folded = app.session_counts();
        assert_eq!(folded.visible, 2, "premise: the lineage starts folded");

        app.set_selected(Some("sbpop-fork-a".to_string()));
        app.expand_selected();
        assert_eq!(
            app.filtered.len(),
            3,
            "premise: expanding really did put the third row on the board"
        );
        assert_eq!(
            app.session_counts(),
            folded,
            "an expanded lineage is still ONE conversation on both sides"
        );

        app.collapse_selected();
        assert_eq!(app.filtered.len(), 2, "premise: it folded back");
        assert_eq!(app.session_counts(), folded, "and the counter never moved");
    }

    /// A query narrows the BOARD, not the project: only the numerator moves.
    #[test]
    fn a_query_shrinks_only_the_numerator() {
        let mut app = App::new(
            lineage_population_store(),
            Scope::Project,
            PathBuf::from("/tmp/sbpop-proj"),
        );
        let before = app.session_counts();

        app.push_query_str("sbpop-lone");

        let after = app.session_counts();
        assert_eq!(after.visible, 1, "one conversation matches: {after:?}");
        assert_eq!(
            after.total, before.total,
            "the population is unchanged by a search"
        );
        assert_eq!(after.hidden, before.hidden);
    }

    /// A PARTIALLY hidden lineage still draws a row, so it stays counted on both
    /// sides and discloses nothing. Hiding normally flips a whole family at once
    /// (`toggle_hidden_selected` -> `lineage_member_ids`), but the counter must
    /// not assume it.
    #[test]
    fn a_partially_hidden_lineage_still_counts_as_visible() {
        let mut app = App::new(
            lineage_population_store(),
            Scope::Project,
            PathBuf::from("/tmp/sbpop-proj"),
        );
        app.hidden_ids.insert("sbpop-fork-b".to_string());
        app.apply_sessions(lineage_population_store());

        let counts = app.session_counts();
        assert_eq!(
            counts,
            SessionCounts {
                visible: 2,
                total: 2,
                hidden: 0
            },
            "one member hidden leaves the conversation on the board: {counts:?}"
        );

        app.hidden_ids.insert("sbpop-fork-a".to_string());
        app.apply_sessions(lineage_population_store());

        let counts = app.session_counts();
        assert_eq!(
            counts,
            SessionCounts {
                visible: 1,
                total: 1,
                hidden: 1
            },
            "hiding the LAST member is what moves it into the segment: {counts:?}"
        );
    }

    /// A scope toggle rebuilds the population, and it is the ONLY keystroke
    /// allowed to: it is the one that already canonicalizes.
    #[test]
    fn toggling_the_scope_rebuilds_the_counted_population() {
        let mut app = App::new(
            population_store(),
            Scope::Project,
            PathBuf::from("/tmp/sbpop-proj"),
        );
        app.all_scope_enabled = true;
        assert_eq!(app.session_counts().total, 2, "premise: the project's two");

        app.toggle_scope();
        assert_eq!(app.scope, Scope::All);
        assert_eq!(
            app.session_counts().total,
            3,
            "the all scope widens the denominator to the whole store"
        );

        app.toggle_scope();
        assert_eq!(app.scope, Scope::CurrentFolder);
        assert_eq!(
            app.session_counts().total,
            2,
            "and wrapping back to the folder narrows it to the project again"
        );
    }

    /// The counted population of [`lineage_population_store`], as
    /// `recompute_scope` groups it: the fork pair is ONE entry, the lone session
    /// another. Written out literally so the pure cases below state the shape
    /// they depend on instead of borrowing it from a grouping pass.
    fn population_lineages() -> Vec<Vec<usize>> {
        vec![vec![0, 1], vec![2]]
    }

    /// The pure split, in the counter's unit: a lineage whose members are ALL
    /// hidden leaves `total` for `hidden`, and the two always add back up to the
    /// number of lineages in the population — the invariant that lets
    /// `N / M sessions · K hidden` reconcile.
    #[test]
    fn count_lineages_moves_a_fully_hidden_lineage_out_of_the_total() {
        let sessions = lineage_population_store();
        let population = population_lineages();
        let hidden_ids: HashSet<String> = ["sbpop-fork-a", "sbpop-fork-b"]
            .into_iter()
            .map(ToString::to_string)
            .collect();

        let counts = count_lineages(&sessions, &population, &[2], &hidden_ids, false);
        assert_eq!(counts.visible, 1, "one conversation is left drawing");
        assert_eq!(counts.total, 1, "the hidden one is not on the board");
        assert_eq!(counts.hidden, 1, "and is disclosed instead of vanishing");
        assert_eq!(
            counts.total + counts.hidden,
            population.len(),
            "the two numbers always reconcile to the population's LINEAGES — two \
             here, not the three files behind them"
        );
    }

    /// The strict test, stated on its own: one hidden member out of two leaves a
    /// row on the board, so the lineage is still counted and nothing is
    /// disclosed. A `any()` test here would drop a drawn row from `total`.
    #[test]
    fn count_lineages_keeps_a_partially_hidden_lineage_in_the_total() {
        let sessions = lineage_population_store();
        let population = population_lineages();
        let hidden_ids: HashSet<String> = ["sbpop-fork-b".to_string()].into_iter().collect();

        let counts = count_lineages(&sessions, &population, &[0, 2], &hidden_ids, false);
        assert_eq!(counts.total, 2, "the half-hidden conversation still draws");
        assert_eq!(counts.hidden, 0, "so there is nothing to disclose");
        assert_eq!(counts.visible, 2);
    }

    /// The numerator groups the DISPLAY list, so an expanded lineage's children
    /// do not each count as a conversation. `filtered.len()` — what the renderer
    /// used to print — says three here.
    #[test]
    fn count_lineages_counts_an_expanded_lineage_once() {
        let sessions = lineage_population_store();
        let population = population_lineages();
        let hidden_ids = HashSet::new();

        let folded = count_lineages(&sessions, &population, &[0, 2], &hidden_ids, false);
        let open = count_lineages(&sessions, &population, &[0, 1, 2], &hidden_ids, false);

        assert_eq!(folded.visible, 2);
        assert_eq!(
            open, folded,
            "re-emitting a lineage's members must not move the counter"
        );
    }

    /// With show-hidden on the rows are back on the board, so they are counted
    /// INSIDE `total` and reported as zero hidden — the view draws no segment for
    /// a zero, which is exactly what stops it counting visible rows twice.
    #[test]
    fn count_lineages_folds_revealed_lineages_back_into_the_total() {
        let sessions = lineage_population_store();
        let population = population_lineages();
        let hidden_ids: HashSet<String> = ["sbpop-fork-a", "sbpop-fork-b"]
            .into_iter()
            .map(ToString::to_string)
            .collect();

        let counts = count_lineages(&sessions, &population, &[0, 2], &hidden_ids, true);
        assert_eq!(counts.total, 2, "a revealed row is counted like any other");
        assert_eq!(counts.hidden, 0, "so there is nothing left to disclose");
    }

    /// The split is an INTERSECTION with the counted population, never a
    /// property of `hidden_ids` at large: a session hidden in ANOTHER project
    /// must not be subtracted from this project's count. `hidden_ids.len()`
    /// would say one here.
    #[test]
    fn count_lineages_ignores_a_hidden_id_outside_the_population() {
        let sessions = lineage_population_store();
        let population = population_lineages();
        let hidden_ids: HashSet<String> = ["sbpop-away".to_string()].into_iter().collect();

        let counts = count_lineages(&sessions, &population, &[0, 2], &hidden_ids, false);
        assert_eq!(counts.total, 2, "nothing in this population is hidden");
        assert_eq!(counts.hidden, 0, "and nothing about it is disclosed");
    }

    // --- pasted query text (the whole-string sibling of typing) ------------

    /// A PASTED query re-filters the board, exactly as typing the same text
    /// would.
    ///
    /// [`App::push_query_str`] exists to pay `set_query`'s pattern rebuild ONCE
    /// for a whole paste instead of once per char, so what it owes is not "the
    /// query string grew" but "the LIST narrowed to what the query matches" —
    /// the same debt `push_query_char` settles above. Both calls below are
    /// asserted, because appending re-filters too: a paste onto a non-empty
    /// query must narrow again, not leave the previous result standing.
    ///
    /// The spaces are what a multi-line paste ARRIVES as here:
    /// `update::flatten_for_query` has already turned `label\nalpha` into
    /// `label alpha`, which `search::gate_atoms` splits into substring atoms
    /// that must ALL match.
    #[test]
    fn pasting_query_text_refilters_the_board() {
        let mut app = app_all(vec![
            session_ts("alpha-one", "repo", Some("main"), "/tmp/a", 300),
            session_ts("alpha-two", "repo", Some("main"), "/tmp/b", 200),
            session_ts("bravo", "repo", Some("main"), "/tmp/c", 100),
        ]);
        assert_eq!(
            visible_ids(&app),
            vec!["alpha-one", "alpha-two", "bravo"],
            "premise: an empty query shows every row, so the paste has something to cut"
        );

        // The flattened form of a pasted `label\nalpha`: every label carries
        // `label`, only the two `alpha` rows carry both atoms.
        app.push_query_str("label alpha");
        assert_eq!(app.query, "label alpha");
        assert_eq!(
            visible_ids(&app),
            vec!["alpha-one", "alpha-two"],
            "both atoms gate the list: `bravo` matches `label` alone"
        );

        // A second paste APPENDS to the query — and re-filters the narrowed list
        // again rather than freezing it.
        app.push_query_str(" two");
        assert_eq!(app.query, "label alpha two");
        assert_eq!(
            visible_ids(&app),
            vec!["alpha-two"],
            "the appended atom cuts `alpha-one`, which matches only the first two"
        );
    }

    // --- grouping for render ----------------------------------------------

    /// A plain session row: no lineage, so nothing hidden and no indent. Rows
    /// like this must stay exactly what they have always been.
    fn plain_row(index: usize) -> Row {
        Row::Session {
            index,
            hidden: 0,
            child: false,
        }
    }

    #[test]
    fn grouping_emits_one_head_per_repo_branch_group() {
        let sessions = vec![
            session("s0", "repo-a", Some("main"), "/tmp/s0"),
            session("s1", "repo-a", Some("main"), "/tmp/s1"),
            session("s2", "repo-a", Some("dev"), "/tmp/s2"),
            session("s3", "repo-b", Some("main"), "/tmp/s3"),
        ];
        let filtered = vec![0usize, 1, 2, 3];
        let rows = build_rows(&sessions, &filtered, Scope::All, &HashMap::new(), None);

        // Distinct groups: (repo-a,main), (repo-a,dev), (repo-b,main) -> 3 heads.
        let heads: Vec<&Row> = rows
            .iter()
            .filter(|r| matches!(r, Row::Group { .. }))
            .collect();
        assert_eq!(heads.len(), 3, "one head per repo->branch group: {rows:?}");

        // Every session row is preceded (somewhere above) by its own head, and a
        // head never repeats for the same group.
        assert_eq!(
            rows,
            vec![
                Row::Group {
                    repo: "repo-a".into(),
                    branch: "main".into()
                },
                plain_row(0),
                plain_row(1),
                Row::Group {
                    repo: "repo-a".into(),
                    branch: "dev".into()
                },
                plain_row(2),
                Row::Group {
                    repo: "repo-b".into(),
                    branch: "main".into()
                },
                plain_row(3),
            ]
        );
    }

    #[test]
    fn detached_branch_groups_under_detached_head() {
        let sessions = vec![session("s0", "repo-a", None, "/tmp/s0")];
        let rows = build_rows(&sessions, &[0], Scope::All, &HashMap::new(), None);
        assert_eq!(
            rows[0],
            Row::Group {
                repo: "repo-a".into(),
                branch: "(detached)".into()
            },
            "a session with no branch groups under the (detached) head"
        );
    }

    #[test]
    fn selected_row_points_at_the_session_row_not_a_head() {
        let sessions = vec![
            session("s0", "repo-a", Some("main"), "/tmp/s0"),
            session("s1", "repo-b", Some("main"), "/tmp/s1"),
        ];
        let mut app = app_all(sessions);
        app.move_selection(1); // select s1
        let rows = app.rows();
        let row = app.selected_row(&rows).expect("selected row");
        assert!(
            matches!(rows[row], Row::Session { index: 1, .. }),
            "selected_row must land on the session row, not a group head: {rows:?}"
        );
    }

    // --- scope-aware display ordering -------------------------------------

    #[test]
    fn all_scope_orders_groups_by_most_recent_then_ts_desc() {
        // group-a: max 300 (members 300, 100); group-b: max 400 (members 400,
        // 200); group-c: max 50. Most-recent-desc across groups: b, a, c.
        let sessions = vec![
            session_ts("a-old", "repo-a", Some("main"), "/tmp/a1", 100),
            session_ts("a-new", "repo-a", Some("main"), "/tmp/a2", 300),
            session_ts("b-old", "repo-b", Some("main"), "/tmp/b1", 200),
            session_ts("b-new", "repo-b", Some("main"), "/tmp/b2", 400),
            session_ts("c-only", "repo-c", Some("main"), "/tmp/c1", 50),
        ];
        let app = app_all(sessions);
        let rows = app.rows();

        // Groups ordered by most-recent desc (b, a, c), one head each, same-group
        // rows contiguous and timestamp-desc within the group.
        assert_eq!(
            rows,
            vec![
                Row::Group {
                    repo: "repo-b".into(),
                    branch: "main".into()
                },
                plain_row(3), // b-new (400)
                plain_row(2), // b-old (200)
                Row::Group {
                    repo: "repo-a".into(),
                    branch: "main".into()
                },
                plain_row(1), // a-new (300)
                plain_row(0), // a-old (100)
                Row::Group {
                    repo: "repo-c".into(),
                    branch: "main".into()
                },
                plain_row(4), // c-only (50)
            ],
            "groups most-recent-desc, rows ts-desc, one head per group: {rows:?}"
        );
    }

    #[test]
    fn current_folder_scope_is_flat_timestamp_desc() {
        let here = unique_temp_dir("flat-order");
        let launch = resolve_dir(&here);
        let cwd = here.to_str().unwrap();
        // Same cwd, DIFFERENT branches, different timestamps: order must be pure
        // timestamp-desc regardless of branch, with no group heads.
        let sessions = vec![
            session_ts("s-mid", "repo-a", Some("main"), cwd, 200),
            session_ts("s-new", "repo-a", Some("dev"), cwd, 300),
            session_ts("s-old", "repo-a", Some("feature"), cwd, 100),
        ];
        let app = App::new(sessions, Scope::CurrentFolder, launch);
        let rows = app.rows();

        assert!(
            rows.iter().all(|r| matches!(r, Row::Session { .. })),
            "current-folder scope must be a flat, head-less list: {rows:?}"
        );
        assert_eq!(
            rows,
            vec![plain_row(1), plain_row(0), plain_row(2)],
            "flat list ordered purely by timestamp desc: {rows:?}"
        );
        let _ = std::fs::remove_dir_all(&here);
    }

    /// A RESOLVED project spans several worktrees on several branches, so the
    /// project scope renders GROUPED (heads, groups newest-first) exactly as the
    /// all scope does — the flat, head-less list belongs to the one scope that
    /// cannot span more than one folder.
    ///
    /// The fixture is deliberately one where the two orders DISAGREE: by bare
    /// timestamp the rows would read m-new, d-mid, m-old, so `main`'s older
    /// session following its newer one is only explicable by grouping.
    #[test]
    fn project_scope_groups_like_the_all_scope() {
        let here = unique_temp_dir("project-rows");
        let launch = resolve_dir(&here);
        let cwd = here.to_str().unwrap();
        let sessions = vec![
            session_ts("m-new", WORKTREE_LABEL, Some("main"), cwd, 300),
            session_ts("d-mid", WORKTREE_LABEL, Some("dev"), cwd, 200),
            session_ts("m-old", WORKTREE_LABEL, Some("main"), cwd, 100),
        ];
        let app = app_project(sessions, launch, vec![resolve_dir(&here)]);

        assert_eq!(
            visible_ids(&app),
            vec!["m-new", "m-old", "d-mid"],
            "groups ordered by their most-recent session, rows ts-desc INSIDE a \
             group — not one flat timestamp-desc run"
        );
        assert_eq!(
            app.rows(),
            vec![
                Row::Group {
                    repo: PROJECT_LABEL.into(),
                    branch: "main".into()
                },
                plain_row(0), // m-new (300)
                plain_row(2), // m-old (100)
                Row::Group {
                    repo: PROJECT_LABEL.into(),
                    branch: "dev".into()
                },
                plain_row(1), // d-mid (200)
            ],
            "the project scope emits one group head per branch: {:?}",
            app.rows()
        );

        let _ = std::fs::remove_dir_all(&here);
    }

    /// The head-less path is `CurrentFolder`'s ALONE. Asserted directly on
    /// [`build_rows`] so it holds for the row builder itself, not merely for the
    /// one scope an `App` happened to be in.
    #[test]
    fn build_rows_emits_group_heads_in_project_scope() {
        let sessions = vec![
            session("s0", "repo-a", Some("main"), "/tmp/s0"),
            session("s1", "repo-b", Some("dev"), "/tmp/s1"),
        ];
        let rows = build_rows(&sessions, &[0, 1], Scope::Project, &HashMap::new(), None);

        assert_eq!(
            rows,
            vec![
                Row::Group {
                    repo: "repo-a".into(),
                    branch: "main".into()
                },
                plain_row(0),
                Row::Group {
                    repo: "repo-b".into(),
                    branch: "dev".into()
                },
                plain_row(1),
            ],
            "a scope that can span folders must keep its heads: {rows:?}"
        );
    }

    #[test]
    fn build_rows_suppresses_heads_in_current_folder_scope() {
        let sessions = vec![
            session("s0", "repo-a", Some("main"), "/tmp/s0"),
            session("s1", "repo-b", Some("dev"), "/tmp/s1"),
        ];
        let rows = build_rows(
            &sessions,
            &[0, 1],
            Scope::CurrentFolder,
            &HashMap::new(),
            None,
        );
        assert_eq!(
            rows,
            vec![plain_row(0), plain_row(1)],
            "the folder scope emits only session rows, no heads: {rows:?}"
        );
    }

    // --- one project, one head (project-scope rendering) -------------------

    /// The label `repo_of` gives the MAIN checkout of this repo, and the DIFFERENT
    /// one it gives that repo's worktrees: a plain checkout renders as `<base>`,
    /// a worktree as `<parent>/<base>`. Both are correct labels for the folder
    /// they describe, and both belong to ONE project — which is exactly why the
    /// project scope cannot head its groups by `session.repo`.
    const MAIN_LABEL: &str = "snapback";
    const WORKTREE_LABEL: &str = "ilfroloff/snapback";

    /// The label git resolves for the whole worktree set, i.e. the text the ONE
    /// project head must be drawn with.
    ///
    /// Deliberately matches NEITHER [`MAIN_LABEL`] nor [`WORKTREE_LABEL`], so a
    /// head bearing this text can only have come from the RESOLVED label. Reusing
    /// a session's own `repo` here — as these fixtures once did — would let a
    /// `project_head` that simply echoed some `session.repo` pass, which is
    /// precisely the reading these tests exist to rule out. The header test in
    /// `tui::view` pins its project name the same way, with the same string.
    const PROJECT_LABEL: &str = "acme/web";

    /// An app in [`Scope::Project`] whose worktree set RESOLVED to `roots`
    /// (already canonicalized), carrying `label`.
    ///
    /// Seeded through the probe plus one reload rather than by poking the field,
    /// so the fixture arrives by the very path a real launch/reload takes.
    ///
    /// `label` is a parameter because `None` is a REACHABLE state of a resolved
    /// set — `WorktreeSet::from_resolved` is public and takes it — not merely a
    /// hypothetical one.
    fn app_project_labeled(
        sessions: Vec<Session>,
        launch: PathBuf,
        roots: Vec<PathBuf>,
        label: Option<String>,
    ) -> App {
        let mut app = App::new(sessions.clone(), Scope::Project, launch);
        let set = WorktreeSet::from_resolved(roots, label);
        app.set_worktree_probe(move |_| set.clone());
        app.apply_sessions(sessions);
        app
    }

    /// The ordinary case: a set that resolved a project label too.
    fn app_project(sessions: Vec<Session>, launch: PathBuf, roots: Vec<PathBuf>) -> App {
        app_project_labeled(sessions, launch, roots, Some(PROJECT_LABEL.to_string()))
    }

    /// The group heads a board is currently drawing, in order.
    fn heads(app: &App) -> Vec<(String, String)> {
        app.rows()
            .into_iter()
            .filter_map(|r| match r {
                Row::Group { repo, branch } => Some((repo, branch)),
                Row::Session { .. } => None,
            })
            .collect()
    }

    /// The whole point of the scope: "aggregate sessions by the root project".
    /// Membership already unifies the main checkout with its worktrees, but they
    /// carry DIFFERENT `session.repo` labels, so heading groups by that field
    /// splits one project across two heads. Under a RESOLVED set the head comes
    /// from the project label instead, and one project draws as one head.
    #[test]
    fn project_scope_renders_every_worktree_under_one_project_head() {
        let main = unique_temp_dir("one-head-main");
        let worktree = unique_temp_dir("one-head-wt");
        let launch = resolve_dir(&main);
        let sessions = vec![
            session_ts(
                "s-main",
                MAIN_LABEL,
                Some("main"),
                main.to_str().unwrap(),
                300,
            ),
            session_ts(
                "s-wt",
                WORKTREE_LABEL,
                Some("main"),
                worktree.to_str().unwrap(),
                200,
            ),
        ];
        let app = app_project(
            sessions,
            launch,
            vec![resolve_dir(&main), resolve_dir(&worktree)],
        );

        assert_eq!(
            visible_ids(&app),
            vec!["s-main", "s-wt"],
            "premise: both worktrees' sessions are in scope, so the head count \
             below is about rendering and not about membership"
        );
        assert_eq!(
            heads(&app),
            vec![(PROJECT_LABEL.to_string(), "main".to_string())],
            "one project, ONE head, named from the RESOLVED project label — a \
             label no session in this fixture carries, so echoing any \
             `session.repo` cannot produce it: {:?}",
            app.rows()
        );

        let _ = std::fs::remove_dir_all(&main);
        let _ = std::fs::remove_dir_all(&worktree);
    }

    /// Unifying the repo head must not flatten the level BELOW it: a project
    /// spans branches, and telling them apart is what the grouped list is for.
    ///
    /// The fixture also pins the ONE thing that can go wrong once heads stop
    /// tracking `session.repo` — the timestamps INTERLEAVE the two `main`
    /// sessions with the `feature` one under the old per-folder key (300, 250,
    /// 200 reads main, feature, main), so if the display ordering and the row
    /// builder disagreed about what a group is, `main` would be sorted apart and
    /// draw its head TWICE. Sharing [`group_key`] is what keeps it one run.
    #[test]
    fn project_scope_keeps_the_branch_groups_under_the_one_head() {
        let main = unique_temp_dir("branch-groups-main");
        let worktree = unique_temp_dir("branch-groups-wt");
        let launch = resolve_dir(&main);
        let sessions = vec![
            session_ts(
                "s-main",
                MAIN_LABEL,
                Some("main"),
                main.to_str().unwrap(),
                300,
            ),
            session_ts(
                "s-wt-feature",
                WORKTREE_LABEL,
                Some("feature"),
                worktree.to_str().unwrap(),
                250,
            ),
            session_ts(
                "s-wt-main",
                WORKTREE_LABEL,
                Some("main"),
                worktree.to_str().unwrap(),
                200,
            ),
        ];
        let app = app_project(
            sessions,
            launch,
            vec![resolve_dir(&main), resolve_dir(&worktree)],
        );

        assert_eq!(
            heads(&app),
            vec![
                (PROJECT_LABEL.to_string(), "main".to_string()),
                (PROJECT_LABEL.to_string(), "feature".to_string()),
            ],
            "two branches -> two branch groups, each head drawn ONCE and both \
             under the one RESOLVED project label, which no session here \
             carries: {:?}",
            app.rows()
        );
        assert_eq!(
            visible_ids(&app),
            vec!["s-main", "s-wt-main", "s-wt-feature"],
            "and the main-checkout session sits INSIDE the branch group it \
             shares with a worktree session, contiguous with it rather than \
             sorted away from it"
        );

        let _ = std::fs::remove_dir_all(&main);
        let _ = std::fs::remove_dir_all(&worktree);
    }

    /// The blast radius, pinned: the head override belongs to the project scope
    /// ALONE. In the all scope the same fixture still heads each folder by its
    /// own `repo_of` label, because there "one project" is not a question the
    /// board can answer — it is showing every project at once.
    ///
    /// The app is walked from a RESOLVED project scope into the all scope rather
    /// than started in it, so the worktree set (and its label) is sitting in the
    /// cache the whole time. Starting fresh in the all scope would leave the set
    /// empty, and the guard would then hold for the wrong reason — there would be
    /// no label available to leak in the first place.
    #[test]
    fn all_scope_still_heads_each_worktree_by_its_own_repo_label() {
        let main = unique_temp_dir("all-heads-main");
        let worktree = unique_temp_dir("all-heads-wt");
        let sessions = vec![
            session_ts(
                "s-main",
                MAIN_LABEL,
                Some("main"),
                main.to_str().unwrap(),
                300,
            ),
            session_ts(
                "s-wt",
                WORKTREE_LABEL,
                Some("main"),
                worktree.to_str().unwrap(),
                200,
            ),
        ];
        let mut app = app_project(
            sessions,
            resolve_dir(&main),
            vec![resolve_dir(&main), resolve_dir(&worktree)],
        );
        // A `-a` board: the all scope is on the key only where the launch flag
        // put it, and this test's whole subject is that scope.
        app.all_scope_enabled = true;
        app.toggle_scope();
        assert_eq!(app.scope, Scope::All, "premise: `Project` -> `All`");
        assert!(
            !app.worktrees.is_empty(),
            "premise: the resolved set is still cached, so the label the all \
             scope must NOT use is genuinely available"
        );

        assert_eq!(
            heads(&app),
            vec![
                (MAIN_LABEL.to_string(), "main".to_string()),
                (WORKTREE_LABEL.to_string(), "main".to_string()),
            ],
            "the all scope is UNCHANGED — two labels, two heads: {:?}",
            app.rows()
        );

        let _ = std::fs::remove_dir_all(&main);
        let _ = std::fs::remove_dir_all(&worktree);
    }

    /// The RE-DERIVED fail-soft rendering contract, and it is the opposite of
    /// what it used to be.
    ///
    /// It used to be "an unresolved project scope renders exactly like the
    /// current-folder scope", which followed from an unresolved scope matching
    /// exactly one folder. [`in_scope`]'s repo-root arm needs no git, so an
    /// unresolved project scope still spans every folder of the repo — and a
    /// scope that spans folders MUST keep its heads, or rows from different
    /// worktrees sit in one undifferentiated list.
    ///
    /// One head is the other half. A grouped list with no head override falls
    /// back to `session.repo`, which spells a checkout and its worktree
    /// differently, so the project would split in two exactly where nothing
    /// resolved it. The fixture gives its two sessions the two REAL labels of one
    /// project, so that split is what a `None` head would produce here.
    ///
    /// The fixture also keeps the flat and grouped ORDERS apart, so the id
    /// assertion cannot pass by coincidence: by bare timestamp the rows read a
    /// (300), b (250), c (100), while grouped the `main` group leads — its
    /// newest, 300, beats `dev`'s 250 — and takes BOTH its sessions first,
    /// giving a, c, b. Only the grouped arm can produce the order asserted here.
    #[test]
    fn an_unresolved_project_scope_still_draws_one_head_over_the_whole_repo() {
        let repo = unique_temp_dir("unresolved-render");
        let launch = resolve_dir(&repo);
        let repo_cwd = repo.to_str().unwrap();
        let worktree = repo.join(".wtp/worktrees/dev");
        let wt_cwd = worktree.to_str().unwrap();
        let sessions = vec![
            session_ts("a", MAIN_LABEL, Some("main"), repo_cwd, 300),
            session_ts("b", WORKTREE_LABEL, Some("dev"), wt_cwd, 250),
            session_ts("c", WORKTREE_LABEL, Some("main"), wt_cwd, 100),
        ];
        // The test-default probe resolves an EMPTY set, which is what "no git,
        // not a repo, non-zero exit" all arrive here as.
        let project = App::new(sessions.clone(), Scope::Project, launch.clone());
        let folder = App::new(sessions, Scope::CurrentFolder, launch.clone());

        assert!(project.worktrees.is_empty(), "premise: nothing resolved");
        assert_eq!(
            visible_ids(&folder),
            vec!["a"],
            "premise: the current-folder scope sees the launch dir alone, so the \
             widening below is a real difference"
        );
        assert_eq!(
            visible_ids(&project),
            vec!["a", "c", "b"],
            "the whole repo is in scope, in GROUPED order (a, c, b) — the flat \
             arm would have given a, b, c"
        );

        let repo_name = launch
            .file_name()
            .and_then(|n| n.to_str())
            .expect("the temp repo dir has a UTF-8 final component")
            .to_string();
        assert_eq!(
            heads(&project),
            vec![
                (repo_name.clone(), "main".to_string()),
                (repo_name, "dev".to_string()),
            ],
            "ONE project head over both branches — never the two per-folder \
             labels the sessions carry: {:?}",
            project.rows()
        );

        let _ = std::fs::remove_dir_all(&repo);
    }

    /// A set with ROOTS but no LABEL still draws exactly one head.
    ///
    /// This used to be the case that could split the project in two: the render
    /// shape keyed on `roots.is_empty()` while [`App::project_head`] keyed on
    /// `label()`, so a set answering YES to one and NO to the other drew GROUPED
    /// with no project label to head by — one head PER FOLDER, the very split the
    /// override removes. [`App::project_head`] is now unconditional in the
    /// project scope, which closes it structurally; this pins the state anyway,
    /// because it is the one that used to be able to reintroduce the split.
    ///
    /// `parse_porcelain` cannot build that shape — it labels from the first root
    /// it inserts, so roots and label arrive together — but
    /// [`WorktreeSet::from_resolved`] takes the two independently and both it and
    /// the `worktrees` field are public, which is all it takes to reach.
    ///
    /// The one head then still needs a NAME, and it is the project ROOT's — the
    /// same [`project_root_name`] fallback the header uses (`tui::view`'s
    /// `project_name`), so head and header cannot disagree about what to call the
    /// project. This launch dir is a plain temp dir, so it IS its own root and
    /// the two spellings coincide; the branch-named case is
    /// [`the_one_head_is_named_after_the_repo_root_not_the_branch_launched_from`].
    #[test]
    fn a_resolved_set_with_no_label_still_draws_one_head_named_from_the_repo_root() {
        let main = unique_temp_dir("no-label-main");
        let worktree = unique_temp_dir("no-label-wt");
        let launch = resolve_dir(&main);
        let launch_name = launch
            .file_name()
            .and_then(|n| n.to_str())
            .expect("the temp launch dir has a UTF-8 final component")
            .to_string();
        let sessions = vec![
            session_ts(
                "s-main",
                MAIN_LABEL,
                Some("main"),
                main.to_str().unwrap(),
                300,
            ),
            session_ts(
                "s-wt",
                WORKTREE_LABEL,
                Some("main"),
                worktree.to_str().unwrap(),
                200,
            ),
        ];
        // Roots but NO label: membership resolved, the project simply unnamed.
        let app = app_project_labeled(
            sessions,
            launch,
            vec![resolve_dir(&main), resolve_dir(&worktree)],
            None,
        );

        assert!(
            !app.worktrees.is_empty(),
            "premise: membership DID resolve, so this renders grouped"
        );
        assert_eq!(
            app.worktrees.label(),
            None,
            "premise: and it resolved without a label"
        );
        assert_eq!(
            heads(&app),
            vec![(launch_name, "main".to_string())],
            "ONE head still — named from the repo ROOT, which this plain temp \
             dir IS, never split back into the per-folder labels: {:?}",
            app.rows()
        );

        let _ = std::fs::remove_dir_all(&main);
        let _ = std::fs::remove_dir_all(&worktree);
    }

    /// The rest of [`App::project_label`]'s totality, which the fixture above
    /// cannot reach: the two launch dirs that have no ordinary NAME — one with no
    /// final component, one that is not UTF-8 at all. A resolved set must still
    /// produce a head for both, because `None` here does not mean "an unnamed
    /// head", it means the per-folder heads coming back.
    ///
    /// Both arms take the fallback the HEADER takes, down to the `to_string_lossy`
    /// repair; `tui::view`'s `head_and_header_name_a_non_utf8_launch_dir_the_same_way`
    /// pins the two answers against each other, while this one pins the head's
    /// exact text.
    ///
    /// The set is poked in directly (as the header's own tests do): this pins
    /// [`App::project_head`] alone, so a rendering fixture would only add noise
    /// between the launch dir and the answer.
    #[test]
    fn a_resolved_set_still_names_a_head_when_the_launch_dir_has_no_usable_name() {
        let head_for = |launch: PathBuf| {
            let mut app = App::new(Vec::new(), Scope::Project, launch);
            app.worktrees = WorktreeSet::from_resolved([PathBuf::from("/any/root")], None);
            app.project_head()
        };

        assert_eq!(
            head_for(PathBuf::from("/")),
            Some("/".to_string()),
            "no final component -> named by the whole path, the same fallback \
             the header takes"
        );

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            assert_eq!(
                head_for(PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/\xff"))),
                Some("\u{FFFD}".to_string()),
                "unspellable in UTF-8 -> the same lossy repair the header makes, \
                 still ONE head"
            );
        }
    }

    /// A worktree DIRECTORY is named after its BRANCH, so naming the one head
    /// after the launch dir would head a list drawn from the whole project with
    /// a branch name — the very misdescription that makes the resolved git label
    /// beat the fallbacks in the first place. The fallback names the repo ROOT
    /// instead, which is also what git would have said.
    ///
    /// Reachable whenever git resolved nothing, which is now a state that still
    /// renders a head: launching `-p` from a worktree with no `git` on `PATH` is
    /// exactly it.
    #[test]
    fn the_one_head_is_named_after_the_repo_root_not_the_branch_launched_from() {
        let launch = PathBuf::from(
            "/Volumes/Development/ilfroloff/snapback/.agents/worktrees/feature/quick-send",
        );
        let app = App::new(Vec::new(), Scope::Project, launch.clone());

        assert!(
            app.worktrees.is_empty(),
            "premise: nothing resolved, so the head takes the FALLBACK name"
        );
        assert_eq!(
            app.project_head(),
            Some("snapback".to_string()),
            "the project is `snapback`; `quick-send` is one of its branches"
        );
        assert_ne!(
            app.project_head().as_deref(),
            launch.file_name().and_then(|n| n.to_str()),
            "and it is emphatically not the launch dir's own name"
        );
    }

    // --- live-agent join + overlay state ----------------------------------

    fn reported_agent(kind: &str) -> ReportedAgent {
        ReportedAgent {
            kind: kind.to_string(),
            id: None,
            state: None,
            status: None,
            name: None,
        }
    }

    /// The reported-agent join is a STRICT full-`session_id` match: a matching id
    /// resolves its record (kind and all), a non-matching one resolves nothing,
    /// and a PREFIX must never match.
    ///
    /// Still load-bearing after the resume gate stopped reading this map: it is
    /// the join the badge, the banner, and — most sharply — the Attach job `id`
    /// lookup all ride on, and a prefix match there would attach to the wrong
    /// agent.
    #[test]
    fn the_reported_agent_join_is_strictly_by_full_session_id() {
        let mut app = app_all(vec![
            session("live-id", "r1", Some("main"), "/tmp/a"),
            session("dead-id", "r2", Some("main"), "/tmp/b"),
        ]);
        let mut reported = HashMap::new();
        reported.insert("live-id".to_string(), reported_agent("background"));
        app.set_reported_agents(reported);

        assert_eq!(
            app.reported_agent("live-id").map(ReportedAgent::kind_label),
            Some("bg"),
            "kind carries through the join"
        );
        assert!(
            app.reported_agent("dead-id").is_none(),
            "a non-matching id resolves no record"
        );
        assert!(
            app.reported_agent("live").is_none(),
            "a PREFIX must not match: the join is the full session id or nothing"
        );
    }

    #[test]
    fn opening_and_cycling_the_choice_overlay_is_a_wrapping_state_machine() {
        let mut app = app_all(vec![session("s", "r", Some("main"), "/tmp/s")]);
        assert!(app.modal.is_none());

        app.open_live_choice("s".to_string());
        let modal = app.modal.clone().expect("overlay open");
        assert_eq!(modal.session_id.as_deref(), Some("s"));
        assert_eq!(modal.layout, ModalLayout::Row, "a button strip");
        assert_eq!(
            modal.selected_action(),
            Some(&ModalAction::Attach),
            "defaults to the Attach choice"
        );

        app.modal_next();
        assert_eq!(
            app.modal.as_ref().unwrap().selected_action(),
            Some(&ModalAction::Fork)
        );
        app.modal_next();
        assert_eq!(
            app.modal.as_ref().unwrap().selected_action(),
            Some(&ModalAction::Cancel)
        );
        app.modal_next();
        assert_eq!(
            app.modal.as_ref().unwrap().selected_action(),
            Some(&ModalAction::Attach),
            "the highlight wraps"
        );
        app.modal_prev();
        assert_eq!(
            app.modal.as_ref().unwrap().selected_action(),
            Some(&ModalAction::Cancel),
            "prev wraps the other way"
        );

        app.close_modal();
        assert!(app.modal.is_none(), "cancel returns to the board");
    }

    // --- hard-delete confirm copy -----------------------------------------

    /// The confirm must let the user PREDICT what `Delete lineage (N)` unlinks.
    /// `lineage_member_ids` sweeps the full store, so `N` counts soft-hidden
    /// members that are not on screen — deliberate, and therefore something to
    /// DISCLOSE rather than to change.
    ///
    /// Pinned as a pure function of `(members, hidden)` so the copy is testable
    /// without a store, a modal or a terminal, and so the three shapes that
    /// matter can be stated side by side: nothing hidden, some hidden, and
    /// everything but the pivot hidden.
    ///
    /// The COUNTS lead the added sentence and the sentence leads the message on
    /// purpose (see `delete_confirm_message`): the message is wrapped to the
    /// modal-width constant, so a narrower terminal clips each row's TAIL, and
    /// only a leading count is safe from being cut into a plausible wrong number.
    #[test]
    fn delete_confirm_message_discloses_hidden_lineage_members_and_stays_quiet_otherwise() {
        // No lineage at all, and a lineage with nothing hidden: the prompt alone.
        // Anything else would put a count in front of a single-session delete.
        assert_eq!(delete_confirm_message(1, 0), DELETE_CONFIRM_PROMPT);
        assert_eq!(
            delete_confirm_message(5, 0),
            DELETE_CONFIRM_PROMPT,
            "a fully visible lineage has nothing to disclose"
        );

        // Some hidden: the counts lead, and the prompt still arrives in full.
        let some = delete_confirm_message(5, 2);
        assert!(
            some.starts_with("5 in this lineage, 2 of them hidden"),
            "the counts must lead the message, where a clip reaches them last: {some:?}"
        );
        assert!(
            some.ends_with(DELETE_CONFIRM_PROMPT),
            "the irreversibility + blast-radius prompt is never displaced: {some:?}"
        );
        // The sentence is a BUDGET, not just copy: it is wrapped into the modal at
        // a fixed width and every wrapped row it adds costs the button strip a
        // terminal size (see `delete_confirm_message`, and the view test that pins
        // the resulting height). Pin the length here too, so a future edit that
        // "just adds a few words" fails next to the wording it changed rather than
        // only in the render test.
        assert_eq!(
            some.len() - DELETE_CONFIRM_PROMPT.len(),
            37,
            "the disclosure's own length is budgeted: {some:?}"
        );

        // Everything but the pivot hidden — the shape where the button's count is
        // most surprising, since only ONE of the three rows is on the board.
        let mostly = delete_confirm_message(3, 2);
        assert!(
            mostly.starts_with("3 in this lineage, 2 of them hidden"),
            "the near-invisible lineage discloses both counts: {mostly:?}"
        );
        // A hidden PIVOT (revealed via show-hidden) is counted too: every member
        // is judged the same way, never all-but-one.
        assert!(
            delete_confirm_message(3, 3).starts_with("3 in this lineage, 3 of them hidden"),
            "a wholly hidden lineage discloses all of it"
        );

        // A single session can never be told about a lineage, whatever the hidden
        // set says about it — there is no lineage button on that modal.
        assert_eq!(
            delete_confirm_message(1, 1),
            DELETE_CONFIRM_PROMPT,
            "a lone session gets no lineage sentence"
        );
    }

    /// The wiring half: the real `open_delete_confirm` counts the hidden members
    /// from the SAME id list the lineage button carries, and the disclosure does
    /// not change what is taken — `(N)` and the action's ids stay the full
    /// lineage, hidden members included.
    ///
    /// The count is an INTERSECTION of `hidden_ids` with the lineage, not a
    /// property of `hidden_ids` at large, so the fixture keeps a hidden session in
    /// a DIFFERENT lineage throughout: `hidden_ids.len()`, or any count that merely
    /// asks whether anything is hidden, would over-report against it.
    #[test]
    fn open_delete_confirm_discloses_hidden_members_without_narrowing_the_lineage() {
        let mut app = app_all(vec![
            session_fork("sbdisc-new", "/tmp/p", "root-1", 300), // newest → head
            session_fork("sbdisc-mid", "/tmp/p", "root-1", 200),
            session_fork("sbdisc-old", "/tmp/p", "root-1", 100),
            // A NON-member: same folder, different root uuid, so a different
            // lineage. Nothing it does may reach the confirm below.
            session_fork("sbdisc-other", "/tmp/p", "root-2", 400),
        ]);
        // Two members soft-hidden: the board shows one row, the button says three.
        app.hidden_ids.insert("sbdisc-mid".to_string());
        app.hidden_ids.insert("sbdisc-old".to_string());
        app.set_selected(Some("sbdisc-new".to_string()));

        app.open_delete_confirm();
        let modal = app.modal.clone().expect("the confirm is open");
        assert!(
            modal
                .message
                .starts_with("3 in this lineage, 2 of them hidden"),
            "the confirm discloses the off-screen members: {:?}",
            modal.message
        );
        assert_eq!(
            modal.choices[1].label, "Delete lineage (3)",
            "the button's count is unchanged — the disclosure tells, it does not narrow"
        );
        assert_eq!(
            modal.choices[1].action,
            ModalAction::DeleteLineage(vec![
                "sbdisc-new".to_string(),
                "sbdisc-mid".to_string(),
                "sbdisc-old".to_string(),
            ]),
            "hidden members are still deleted with the rest of the lineage"
        );

        // MIXED: one member hidden, one NON-member hidden. The disclosure must say
        // one — `hidden_ids.len()` would say two, and so would any count taken
        // before the lineage is intersected in.
        app.hidden_ids.clear();
        app.hidden_ids.insert("sbdisc-mid".to_string());
        app.hidden_ids.insert("sbdisc-other".to_string());
        app.open_delete_confirm();
        let mixed = app.modal.clone().expect("the confirm is open");
        assert!(
            mixed
                .message
                .starts_with("3 in this lineage, 1 of them hidden"),
            "only the hidden LINEAGE member is counted, not the hidden outsider: {:?}",
            mixed.message
        );

        // Reveal the members and leave ONLY the outsider hidden. `hidden_ids` is
        // still non-empty, but none of it is in the family the button takes, so the
        // confirm must say nothing extra: a disjoint hidden set discloses nothing.
        app.hidden_ids.clear();
        app.hidden_ids.insert("sbdisc-other".to_string());
        app.open_delete_confirm();
        let revealed = app.modal.clone().expect("the confirm is open");
        assert_eq!(
            revealed.message, DELETE_CONFIRM_PROMPT,
            "a hidden session outside the lineage is not a member `(N)` takes"
        );
    }

    // --- new-session agent picker -----------------------------------------

    fn def_agent(name: &str) -> DefinedAgent {
        DefinedAgent {
            name: name.to_string(),
            description: None,
        }
    }

    #[test]
    fn pick_default_index_maps_last_agent_to_its_row_or_falls_back_to_default() {
        let agents = vec![def_agent("alpha"), def_agent("beta")];
        // No prior pick -> the default (no-agent) row 0.
        assert_eq!(pick_default_index(None, &agents), 0);
        // A prior pick maps to its row, offset by 1 for the leading default entry.
        assert_eq!(pick_default_index(Some("alpha"), &agents), 1);
        assert_eq!(pick_default_index(Some("beta"), &agents), 2);
        // A prior pick that no longer exists (its file was removed) -> default.
        assert_eq!(pick_default_index(Some("gone"), &agents), 0);
    }

    #[test]
    fn agent_picker_opens_cycles_and_maps_the_selected_agent() {
        let mut app = app_all(vec![session("s", "r", Some("main"), "/tmp/s")]);
        assert!(app.modal.is_none());

        app.open_agent_picker(vec![def_agent("alpha"), def_agent("beta")]);
        let modal = app.modal.clone().expect("picker open");
        assert_eq!(modal.layout, ModalLayout::List, "a vertical picker");
        // No prior pick -> choice 0 (default / no agent).
        assert_eq!(modal.selected, 0);
        assert_eq!(
            modal.selected_action(),
            Some(&ModalAction::New(None)),
            "choice 0 is the default (no-agent) entry"
        );

        // Down cycles default -> alpha -> beta -> (wrap) default.
        app.modal_next();
        assert_eq!(
            app.modal.as_ref().unwrap().selected_action(),
            Some(&ModalAction::New(Some("alpha".to_string())))
        );
        app.modal_next();
        assert_eq!(
            app.modal.as_ref().unwrap().selected_action(),
            Some(&ModalAction::New(Some("beta".to_string())))
        );
        app.modal_next();
        assert_eq!(
            app.modal.as_ref().unwrap().selected_action(),
            Some(&ModalAction::New(None)),
            "the highlight wraps back to the default entry"
        );
        // Up wraps the other way to the last agent.
        app.modal_prev();
        assert_eq!(
            app.modal.as_ref().unwrap().selected_action(),
            Some(&ModalAction::New(Some("beta".to_string())))
        );

        app.close_modal();
        assert!(app.modal.is_none(), "cancel returns to the board");
    }

    #[test]
    fn open_agent_picker_pre_highlights_the_last_picked_agent() {
        let mut app = app_all(vec![session("s", "r", Some("main"), "/tmp/s")]);
        app.set_last_new_agent(Some("beta".to_string()));
        app.open_agent_picker(vec![def_agent("alpha"), def_agent("beta")]);
        // The last pick pre-highlights its row so Ctrl-N then Enter repeats it.
        assert_eq!(
            app.modal.as_ref().unwrap().selected_action(),
            Some(&ModalAction::New(Some("beta".to_string()))),
            "the picker opens on the last-picked agent"
        );
    }

    /// An IN-FLIGHT draft card counts as an active overlay even though the editor
    /// has already closed and no keyboard owner is left.
    ///
    /// It owns the PANE: while the card is drawn the transcript is not, so the
    /// cached link regions describe text no longer on screen and a click resolved
    /// against them would open a link from a session the user cannot see. That
    /// window is exactly the one the editor no longer covers.
    #[test]
    fn an_in_flight_draft_card_still_gates_the_mouse() {
        let mut app = app_all(vec![session("s", "r", Some("main"), "/tmp/s")]);
        app.open_compose(
            super::super::compose::ComposeState::new_background(None),
            Some(NewSessionDraft::default()),
        );
        let launch_id = app.dispatch_draft();
        assert!(!app.is_composing(), "the editor is gone once dispatched");
        assert!(
            app.launching_draft(launch_id).is_some(),
            "the card must be stamped with the id it was dispatched under, or the \
             completion has nothing to match"
        );
        assert!(
            app.overlay_active(),
            "the card still owns the pane, so the mouse must stay gated"
        );
        app.begin_split_drag();
        assert!(
            !app.is_dragging_split(),
            "a stray click over the card must not start a splitter drag"
        );

        app.close_compose();
        assert!(
            !app.overlay_active(),
            "the board is back once the card closes"
        );
    }

    #[test]
    fn agent_picker_counts_as_an_active_overlay_and_blocks_split_drag() {
        let mut app = app_all(vec![session("s", "r", Some("main"), "/tmp/s")]);
        app.open_agent_picker(vec![def_agent("alpha")]);
        assert!(app.overlay_active(), "an open picker is an active overlay");
        app.begin_split_drag();
        assert!(
            !app.is_dragging_split(),
            "a stray click during the picker must not start a splitter drag"
        );
    }

    // --- status dwell --------------------------------------------------------

    /// Task 3.9: a transient status lives for exactly `STATUS_DWELL_TICKS` ticks,
    /// then auto-clears. It must NOT vanish before the final tick.
    #[test]
    fn transient_status_dwells_and_expires() {
        let mut app = app_all(vec![]);
        app.set_status_transient("sent");
        assert_eq!(app.status.as_deref(), Some("sent"));
        assert_eq!(app.status_ttl, Some(STATUS_DWELL_TICKS));

        // One tick short of expiry: still visible.
        for _ in 1..STATUS_DWELL_TICKS {
            app.tick_status();
            assert_eq!(
                app.status.as_deref(),
                Some("sent"),
                "transient status survives until the dwell expires"
            );
        }

        // The final tick clears it.
        app.tick_status();
        assert!(
            app.status.is_none(),
            "transient status must clear after STATUS_DWELL_TICKS ticks"
        );
        assert!(app.status_ttl.is_none());
    }

    /// Task 3.9: a sticky status (failure / refusal) ignores `tick_status` and
    /// stays until an actionable keypress clears it.
    #[test]
    fn sticky_status_survives_the_dwell() {
        let mut app = app_all(vec![]);
        app.set_status("send failed: boom");
        assert_eq!(app.status_ttl, None);

        for i in 0..STATUS_DWELL_TICKS {
            app.tick_status();
            assert_eq!(
                app.status.as_deref(),
                Some("send failed: boom"),
                "sticky status must survive tick {i}"
            );
        }
        assert_eq!(app.status_ttl, None);
    }

    /// Task 3.9: overwriting a transient status with a sticky one restores the
    /// sticky lifetime — the dwell timer must not outlive the new message.
    #[test]
    fn set_status_restores_stickiness_after_transient() {
        let mut app = app_all(vec![]);
        app.set_status_transient("sent");
        app.set_status("send failed: boom");
        assert_eq!(app.status_ttl, None);

        for i in 0..STATUS_DWELL_TICKS {
            app.tick_status();
            assert_eq!(
                app.status.as_deref(),
                Some("send failed: boom"),
                "restored sticky status must survive tick {i}"
            );
        }
    }
}
