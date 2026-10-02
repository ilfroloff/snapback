//! The elm-style update loop (pure state transitions).
//!
//! On `Input` handles keybindings; on `SessionsChanged` reloads the store and
//! re-applies query/scope while preserving selection-by-id and scroll; on
//! `Tick` does nothing costly. Restores selection by locating the selected
//! `session_id` in the new filtered list (clamps to nearest if it vanished).
//! The remaining variants are off-thread deliveries: `ReportedAgents` swaps in the
//! poller's badge/banner map, while `SendFinished`, `InterruptFinished` and
//! `BgLaunchFinished` each land ONE one-shot child's result — the last of which
//! also closes the in-flight new-session draft card it names (and only that one).
//! `CopyFinished` lands a clipboard tool's result too (a `Ctrl-X y` id or a
//! preview drag-selection), but hands it back to the driver
//! ([`Outcome::FinishCopy`]) because only the driver holds the writer its OSC 52
//! fallback needs.
//! Because such a result can arrive after the board it belongs to is gone,
//! [`handle_event`] closes the compose surface on any [`Outcome`] that
//! [ends the board session](Outcome::ends_board_session) as well.
//!
//! This module is the *decision* half of the loop: [`key_to_action`] maps a key
//! to an [`Action`], and [`handle_event`] applies an [`AppEvent`] to the [`App`]
//! and returns an [`Outcome`] telling the driver (in [`crate::tui`]) whether to
//! continue, quit, hand off a resume, fire one of the three no-teardown children
//! (`Send` / `Interrupt` / `BgLaunch`), deliver `Ctrl-K`'s child-free SIGTERM
//! (`Signal`, a `kill(2)` the driver runs inline — see [`Outcome::Signal`]), or
//! start / finish a clipboard copy — `Ctrl-X y`'s id or a preview drag-selection —
//! (`Copy` / `FinishCopy`). All of it is
//! terminal-free and unit tested; the terminal-driving loop that calls it lives in
//! [`crate::tui::run`].
//!
//! ## Keybindings
//!
//! | Key | Action |
//! | --- | ------ |
//! | `Up` / `Down` | move selection (always) |
//! | `Left` / `Right` | move the search query's caret one character back / forward (always). Not a query change: the list, the selection and the preview stay exactly where they were |
//! | `Alt-Left` / `Alt-Right`, `Alt-b` / `Alt-f`, `Ctrl-Left` / `Ctrl-Right` | move the search query's caret one WORD back / forward (always) — the widget's own word hop (see [`App::move_query_caret_by_word`]), so forward lands on the START of the next word, as in the reply box. Not a query change either. Three pairs because `⌥←` / `⌥→` reaches the board as `CSI 1;3D` / `C` or as `ESC b` / `ESC f` depending on the terminal, and `Ctrl-Left` / `Ctrl-Right` is the non-`Alt` twin; `Alt-b` / `Alt-f` and `Ctrl-Left` / `Ctrl-Right` are also the pairs the compose box hops words on |
//! | `Enter` | resume the selected session |
//! | `Ctrl-F` | fork-resume the selected session |
//! | `Ctrl-N` | start a new session in the launch directory. When agents are defined a picker opens first and `Enter` on a pick opens a draft pane for the session's first message; with none defined that draft opens straight away. In the draft, `Enter` starts a BACKGROUND agent without leaving the board, `Ctrl-O` runs it interactively instead, `Esc` cancels |
//! | `Ctrl-O` (in the agent picker) | start the highlighted agent INTERACTIVELY at once, skipping the draft — the same verb `Ctrl-O` names inside the draft, so BOTH routes out of the picker cost exactly one key. Bound on the picker alone — inert on every other modal |
//! | `Ctrl-R` | quick-reply: send a one-shot message to the selected session without leaving the board. An agent whose run is OVER (`done` / `stopped` / `failed`) is stopped first so the reply lands in place; `needs input` confirms first; `working` / `idle` / `interrupted` / an unrecognized qualifier is refused, and so is a session claude reports with no stoppable job id — the refusal points at `Ctrl-K` or Fork (see [`send::reply_gate`]). While this session's OWN reply is still in flight, `Ctrl-R` on it is refused before any of the above (see [`send::reply_in_flight_refusal`]); a reply still in flight to another row refuses nothing here |
//! | `Ctrl-K` | stop / interrupt the selected session's live agent, by whichever handle claude's record carries (see [`send::interrupt_gate`]). A stoppable job id → `claude stop`: an agent whose run is OVER (`done` / `stopped` / `failed`) stops at once, every other live agent confirms first. NO job id but a `pid` → confirm, then re-ask claude at `Enter` and send that pid a SIGTERM (never SIGKILL) only if claude still reports the same pid with no job id; a record that is gone, now carries a job id, or reports another pid refuses instead (see [`send::signal_plan`]). A session claude is not holding, or one it reports with neither a job id nor a pid — or with no job id and a pid no signal could take (`0`, past `i32::MAX`, or the board's own process id) — is refused |
//! | `Tab` | toggle name-only vs. name+content search. Widening to content also opens the preview on the most recent match, exactly as typing does: it goes through the same query funnel, and the mode is the gate that key just opened |
//! | `Ctrl-A` | flip the scope: current folder <-> project (the launch repo and all of its git worktrees). ONE key for both, because the second is a refinement of the same question the first answers, not a separate mode. Launched with `--all`/`-a` it becomes a three-stop cycle through all folders as well — the whole store is on this key only when the launch flag put it there |
//! | `Ctrl-X` then `x`/`d`/`h`/`r`/`y`/`f` | leader chord: hide / hard-delete (this row, or its whole fork lineage) / toggle show-hidden / re-read every transcript from disk / copy session ID (the selected session's full id, to the clipboard; the id also shows on the status line) / fold or expand the selected row's fork lineage (fold an open one, open a folded `(+N)` head, nothing otherwise — see [`App::toggle_selected_lineage`]) (any other key cancels) |
//! | `Ctrl-L` (in a compose box) | pick the model — and optionally the effort — for THIS compose only: the `Ctrl-R` reply or the `Ctrl-N` draft it is pressed in (see [`compose::compose_key_to_action`]). The box's `model:` label names what it runs on: a reply's default is `session (<model>)`, the model its session last answered with, which claude normally restores by itself (`default` when an `ANTHROPIC_MODEL` / `ANTHROPIC_DEFAULT_*_MODEL` override, or a transcript with no answering model, means it would not); a draft's is `default (<value>) (new sessions only)` from the user's `claude` settings. `--model` / `--effort` are sent ONLY for a pick other than that default — on the reply, the draft's background launch and the draft's `Ctrl-O` run. The picker's first row returns to the default, `Enter` sets the highlighted row into the compose, `Esc` returns with the text and the previous pick intact. Every new compose starts at its default; nothing is remembered. `Enter`, `Ctrl-F` and Attach never send a model |
//! | `Left` / `Right` (in the model picker) | step the highlighted MODEL row's `--effort` down / up through unset → `low` → `medium` → `high` → `xhigh` → `max`, wrapping both ways; `Enter` then sets the model and the effort together into the compose. Inert on the picker's default row (no model, so no effort) and on every other list modal, so the agent picker keeps ignoring them; they never reach the board's search caret underneath |
//! | `Shift-Left` / `Shift-Right` | step the pane layout one stop toward a full-width preview / a full-width list, along `0:1 · 1:3 · 1:1 · 3:1 · 1:0` (list:preview; the board starts at `1:1`). A press at either end does nothing. Always — with or without a query, and whatever is marked. The step keeps the reader's place in the preview; leaving `1:0` opens it on the newest turn (see [`App::set_pane_layout`]) |
//! | `PgUp` / `PgDn` | scroll the preview a page (always) |
//! | `Ctrl-U` / `Ctrl-D` | scroll the preview a quarter page (always) |
//! | `Ctrl-T` / `Ctrl-E`, `Home` / `End` | jump the preview to top / bottom (always). Every preview scroll key (these, `PgUp` / `PgDn`, `Ctrl-U` / `Ctrl-D`) also works while a quick reply is open, through the same `App` methods (see [`compose::ComposeAction::PreviewTop`]); there they take the editor's meaning (`Ctrl-U`/`Ctrl-D`/`Ctrl-E`, `Home`/`End`/`PgUp`/`PgDn`) away. A new-session draft keeps them all for its editor |
//! | `Shift-Up` / `Shift-Down` | scroll the preview onto the previous / next MARKED line, but only while the query marks something in the previewed transcript; with nothing marked they fall through to plain selection movement. One stop per marked LINE, not per occurrence — a line saying the query twice is marked, and stopped at, once |
//! | `Backspace` | delete the query character before the caret |
//! | `Alt-Backspace` / `Ctrl-W` / `Alt-H` | delete the query ATOM before the caret — one whole search word, not one character, so a path or a branch name goes in a single press; what follows the caret stays. THREE keys because `TextArea::input` word-deletes on all three in the compose box: binding the same SET here is what makes the gesture reach the board at all, whatever the user's option-as-meta setting turns `Alt-Backspace` into. The set matches; the EXTENT deliberately does not — the board cuts at the search atom and the compose box at `CharKind`'s punctuation boundary, so `feature/fold-fork-lineages` goes whole here and loses only `lineages` there |
//! | printable char | type-to-search (insert at the query's caret) |
//! | terminal paste | inserted as TEXT — never as keystrokes (see below) |
//! | mouse: click a folded node's header | unfold the node — a subagent's hand-back (`◆`) or context claude injected (`◇`) — where it sits, and a second click folds it back; a click on a header toggles its node and never opens a link. On the RELEASE, like a link, and ahead of one (see [`click_effect`]). Works while a quick reply is open, like every pointer action over the transcript |
//! | mouse: click a preview link | open its url in the browser — `http`/`https` only: any other scheme opens nothing and says so on a sticky status line. On the RELEASE, since only then is it known that the press was a click and not the start of a drag (see [`mouse_effect`]). Works while a quick reply is open |
//! | mouse: drag in the preview | select transcript text in reading order, reverse-videoed — DRAWN text only: each row ends at its last drawn character, never at the pane's edge, and a drag over blank space alone selects nothing, so its release copies nothing. HOLD the drag past the transcript's top or bottom edge and the pane glides that way, a small step every `AUTOSCROLL_FRAME` — faster the further past the edge — so the selection keeps growing into rows that were never on screen until the button comes up (see [`App::autoscroll_preview_selection`]); a plain move while the button is held counts as the release the terminal lost. The release copies the WHOLE selection the way `Ctrl-X y` copies — [`Outcome::Copy`], the clipboard tool first, OSC 52 as the fallback — with a transient status. Never the in-flight reply's tail. A wheel notch, any key or a resize ends it. A drag that starts on a node header or a link selects, and toggles or opens nothing. Off under any overlay and a new-session draft's card, but ON while a quick reply is open (it previews the real transcript; the drag neither moves its caret nor touches its text), and never started on the pinned row or a docked compose zone (see [`press_starts_selection`]) |
//! | mouse: double-click in the preview | select the WORD under the pointer (Unicode word boundaries, on the drawn row) and copy it on release, exactly as a drag copies. Two presses on the SAME cell within [`DOUBLE_CLICK_INTERVAL`] (see [`is_double_click`]); a blank word selects nothing; a quick third click keeps the word. The FIRST release is a plain click — it toggles a node header or opens a link, as above — and the SECOND copies the word and toggles or opens nothing, so a node header double-clicked is opened once and stays open. Any key, wheel notch or reload resets the count (a fold toggle does not: it is the first click's own effect), and a press that turned into a drag is not a first click. Same gate as a drag ([`press_starts_selection`]), so it too works while a quick reply is open |
//! | `Esc` | clear the search query when one is typed; quit when it is already empty |
//! | `Ctrl-C` | quit (always) |
//!
//! No bare printable character is a command: every one of them types into the
//! query, so no search term can navigate or quit on its way in. Arrows, `Enter`,
//! `Tab`, and every `Ctrl-` binding work regardless of the query, so search is
//! never blocked either. `Shift-Up` / `Shift-Down` are the one conditional binding,
//! disambiguated by whether there is anything marked to move between — and they
//! fall through to the unshifted binding when there is not, so a terminal that
//! drops the modifier still moves the selection. `Shift-Left` / `Shift-Right` are
//! NOT conditional: they step the layout whatever the query or the marks, and their
//! arms sit above the plain `Left` / `Right` ones because the first matching arm
//! wins. A terminal that drops THAT modifier delivers a plain `Left` / `Right`,
//! which moves the search caret instead — a working key, just not the one
//! pressed. Caret movement and the lineage fold never share a key: the fold is
//! `Ctrl-X f`. The `Alt` arrows' word-hop arms sit BETWEEN the two: below the
//! shifted ones, so `Shift-Alt` still steps the layout, and above the plain ones,
//! which match an `Alt` arrow too and would step it one character. `Ctrl-Left` /
//! `Ctrl-Right` hop from inside the `Ctrl` block, which returns before any of them
//! is tried.
//!
//! ## Terminal paste
//!
//! `tui::init_terminal` enables BRACKETED PASTE, so the terminal hands a clipboard
//! drop over as ONE [`crossterm::event::Event::Paste`] rather than as a stream of
//! `KeyEvent`s. There is NO `Ctrl-V` binding and there must not be one: the paste
//! is the terminal's own (`Cmd+V`, middle-click, …), which keeps working over SSH
//! and inside tmux where an app-side clipboard read would not.
//!
//! [`handle_paste`] routes it through the SAME six-owner precedence the key arm
//! uses, and the row above is deliberately terse because the interesting part is
//! that routing — the four overlay owners swallow a paste, the compose zone inserts
//! it at the caret, and the board inserts it at the query's caret with newlines
//! flattened to spaces. A paste can never submit, resume, or quit.

use std::io::Write;
use std::time::{Duration, Instant};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};

use crate::defined_agents;
use crate::delete;
use crate::resume::{self, Ready};
use crate::send::{
    self, BgLaunchRequest, InterruptGate, InterruptRequest, ReplyGate, SendRequest, SignalPlan,
};
use crate::store::{preview, SessionStore};
use crate::watch::{AppEvent, CopyPayload};

use super::app::{
    screen_at, App, ClickRecord, InterruptRoute, Interrupting, ModalAction, ModalLayout,
};
use super::{clipboard, compose, view};

/// A decoded intent from a single keypress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Quit the app.
    Quit,
    /// Move the selection up one row.
    MoveUp,
    /// Move the selection down one row.
    MoveDown,
    /// Move the search query's caret one character toward the head of the line
    /// (`←`).
    CaretBack,
    /// Move the search query's caret one character toward the tail of the line
    /// (`→`).
    CaretForward,
    /// Move the search query's caret one WORD toward the head of the line
    /// (`Alt-←` / `Alt-b` / `Ctrl-←`).
    CaretWordBack,
    /// Move the search query's caret one WORD toward the tail of the line, onto
    /// the start of the next word (`Alt-→` / `Alt-f` / `Ctrl-→`).
    CaretWordForward,
    /// Resume (or fork-resume) the selected session. The refusal gate and the
    /// `claude` hand-off are decided in [`apply_action`]; a confirmed plan
    /// surfaces as [`Outcome::Resume`].
    Resume {
        /// Whether to fork the session (`Ctrl-F`) rather than plain resume.
        fork: bool,
    },
    /// Start a brand-new `claude` session in the launch directory (`Ctrl-N`).
    /// When defined agents exist, [`apply_action`] opens the agent picker first;
    /// otherwise (or once a pick is confirmed) the launch-dir existence gate and
    /// the `claude` hand-off are decided there and a confirmed plan surfaces as
    /// [`Outcome::Resume`].
    NewSession,
    /// Open the quick-reply compose zone for the selected session (`Ctrl-R`).
    /// [`apply_action`] runs the reply gate ([`send::reply_gate`]) first, because
    /// `claude -p -r` REFUSES a session claude is holding as an agent: an agent
    /// whose run is over is stopped first and compose opens, a `needs input` one
    /// confirms before that stop, and a still-live one is refused with a hint.
    /// Before that gate, a reply of the selected session's own still in flight
    /// refuses ([`send::reply_in_flight_refusal`]); a reply in flight to another
    /// row does not.
    Reply,
    /// Stop / interrupt the selected session's live agent (`Ctrl-K`).
    /// [`apply_action`] runs the interrupt gate ([`send::interrupt_gate`]): on a
    /// background job id, an agent whose run is over is stopped immediately and
    /// every other live agent confirms first; a reported session with no job id
    /// but a `pid` confirms, then has that pid re-verified and sent a SIGTERM; a
    /// non-live session, one reported with neither handle, or one whose pid no
    /// signal could take, is refused with a hint.
    Interrupt,
    /// Toggle name-only vs. name+content search.
    ToggleSearchMode,
    /// Flip the scope: current folder <-> project (`Ctrl-A`), or cycle it
    /// through all folders as well when the board was launched with
    /// `--all`/`-a`. The project state spans the launch repo's git worktrees;
    /// see [`super::app::Scope::toggled`] for the cycle itself and for why the
    /// widest state is off the key by default.
    ToggleScope,
    /// Step the pane layout one stop toward the full-width preview (`Shift-Left`).
    LayoutTowardPreview,
    /// Step the pane layout one stop toward the full-width list (`Shift-Right`).
    LayoutTowardList,
    /// Scroll the preview up one page (`PgUp`).
    PreviewPageUp,
    /// Scroll the preview down one page (`PgDn`).
    PreviewPageDown,
    /// Scroll the preview up a quarter page (`Ctrl-U`).
    PreviewHalfUp,
    /// Scroll the preview down a quarter page (`Ctrl-D`).
    PreviewHalfDown,
    /// Jump the preview to the top (`Home`).
    PreviewTop,
    /// Jump the preview to the bottom / re-follow the newest turn (`End`).
    PreviewBottom,
    /// Scroll the preview onto the NEXT marked line (`Shift-Down`).
    PreviewMatchNext,
    /// Scroll the preview onto the PREVIOUS marked line (`Shift-Up`).
    PreviewMatchPrev,
    /// Insert a character at the query's caret (type-to-search).
    Insert(char),
    /// Delete the query character before the caret.
    Backspace,
    /// Delete the search ATOM before the query's caret — one whole word, not one
    /// character (`Alt-Backspace` / `Ctrl-W` / `Alt-H`).
    ///
    /// Three keys because the compose box answers all three: `TextArea::input`
    /// maps each of them to its own word delete, so binding the same SET here is
    /// what makes the gesture reach the board on every terminal, whatever the
    /// user's option-as-meta setting turns `Alt-Backspace` into.
    ///
    /// The SET is all the two surfaces share. The EXTENT deliberately differs:
    /// the boundary here is the search atom
    /// ([`search::last_atom_start`](crate::search::last_atom_start)), NOT the
    /// widget's `CharKind` word, which breaks on ASCII punctuation — a path or a
    /// branch name is ONE thing the user typed and one press should take it, so
    /// `feature/fold-fork-lineages` goes whole on the board where a compose box
    /// would leave all but `lineages`.
    BackspaceWord,
    /// Empty the search query (`Esc` while one is typed). With an empty query
    /// `Esc` is [`Action::Quit`] instead, so a first press never quits a board the
    /// user was still searching.
    ClearQuery,
    /// Enter the `Ctrl-X` leader chord: arm [`App::pending_chord`] so the NEXT key
    /// routes through the pure [`chord_key`] machine (hide / hard-delete /
    /// show-hidden / forced rescan / copy session ID / fold toggle / cancel)
    /// instead of the board.
    Chord,
    /// A key with no binding in the current state.
    Ignore,
}

/// What the driver loop should do after handling one event.
///
/// [`Outcome::Resume`] is the return-to-board seam: the refusal gate
/// ([`resume::check`]) has already run while the terminal was up, so this only
/// ever carries a CONFIRMED [`Ready`] plan. The driver in [`crate::tui`] tears
/// the terminal down, spawns `claude` as a child, waits, then re-initializes and
/// keeps looping — a refused resume never reaches here (it sets a board status
/// and stays on [`Outcome::Continue`]).
pub enum Outcome {
    /// Keep running.
    Continue,
    /// Exit the app cleanly.
    Quit,
    /// Tear down the terminal and spawn `claude` for this confirmed plan, then
    /// return to the board.
    Resume(Ready),
    /// Fire a one-shot quick-reply send on a detached thread and KEEP running —
    /// the board never tears down. Handled inline by [`crate::tui::run`] (which
    /// owns the event channel the send reports back on), so unlike
    /// [`Resume`](Self::Resume) it never propagates out to the process driver.
    /// Carried as data (rather than spawned in the handler) so the send DECISION
    /// stays pure and unit-testable, the way [`Resume`](Self::Resume) carries a
    /// confirmed [`Ready`].
    Send(SendRequest),
    /// Fire a one-shot interrupt (`claude stop <job-id>`) on a detached thread and
    /// KEEP running — like [`Send`](Self::Send), the board never tears down. Handled
    /// inline by [`crate::tui::run`]; the stop reports back via
    /// [`AppEvent::InterruptFinished`](crate::watch::AppEvent::InterruptFinished).
    /// Carried as data (rather than spawned in the handler) so the interrupt DECISION
    /// stays pure and unit-testable, the way [`Send`](Self::Send) carries a request.
    Interrupt(InterruptRequest),
    /// Fire a one-shot background-agent launch (`claude [--agent <name>] --bg
    /// <prompt>`) on a detached thread and KEEP running — like [`Send`](Self::Send)
    /// and [`Interrupt`](Self::Interrupt), the board never tears down. Handled
    /// inline by [`crate::tui::run`]; the launch reports back via
    /// [`AppEvent::BgLaunchFinished`](crate::watch::AppEvent::BgLaunchFinished).
    ///
    /// This is why starting a background agent does NOT route through
    /// [`Resume`](Self::Resume): a `--bg` launch returns immediately and needs no
    /// TTY, so tearing the terminal down for it would flash the board away for
    /// nothing. The interactive escape hatch (`Ctrl-O`) still takes
    /// [`Resume`](Self::Resume), because that one really does hand the terminal over.
    BgLaunch(BgLaunchRequest),
    /// Send a re-verified pid a SIGTERM — `Ctrl-K`'s route for a reported session
    /// with no stoppable job id, confirmed and re-probed
    /// ([`send::signal_plan`](crate::send::signal_plan)). The board never tears down.
    ///
    /// **This one is performed SYNCHRONOUSLY by the driver, with no detached thread
    /// and no `AppEvent` round trip — and that is a decision, not an omission.** The
    /// three variants above exist because a `claude` CHILD blocks: it has to be
    /// spawned, waited on, and its streams read, so the work cannot sit on the render
    /// loop. `kill(2)` returns as soon as the signal is queued. There is no completion
    /// to wait for and nothing to report back, so a thread plus a channel round trip
    /// would add two moving parts and a new event source to deliver a result the
    /// driver already holds.
    ///
    /// It is NOT an exception to AGENTS.md's OFF-UI-THREAD rule either. That rule
    /// governs BLOCKING work, and one non-blocking syscall is not blocking work — the
    /// rule is satisfied rather than waived. (The one-shot PROBE that re-verified this
    /// pid does block, briefly, and IS argued as the documented hand-off exception —
    /// at its call site, [`dispatch_signal`].)
    ///
    /// Carried as data for the same reason as the others: the DECISION stays in the
    /// pure handler and unit-testable, while the effect lives in the driver.
    Signal {
        /// The pid to signal, as re-verified against a fresh probe at confirm time.
        pid: u32,
    },
    /// Copy this payload's text to the system clipboard and KEEP running — the
    /// `Ctrl-X y` request (the selected session's FULL id) or a finished preview
    /// drag (the selected transcript text). Like [`Send`](Self::Send) a no-teardown
    /// effect handled inline by [`crate::tui::run`], and ONE path for both kinds:
    /// the driver reads the environment, picks the route
    /// ([`clipboard::clipboard_route`]), and either starts the clipboard-tool worker
    /// ([`clipboard::spawn_tool_copy`], which reports back via
    /// [`AppEvent::CopyFinished`]) or — over SSH, or with no tool for this
    /// OS/display — writes the OSC 52 fallback at once through [`finish_copy`].
    /// Carried as data (rather than copied in the handler) so the copy DECISION
    /// stays pure and unit-testable, the way [`Send`](Self::Send) carries a request.
    Copy(CopyPayload),
    /// Complete a finished copy: set its honest status and, when no tool copied
    /// the text, write the OSC 52 fallback — [`finish_copy`], which the driver runs
    /// with the terminal's own writer on the UI thread, between draws.
    ///
    /// [`handle_event`] returns this for [`AppEvent::CopyFinished`] instead of
    /// completing the copy itself because the fallback is a terminal WRITE and only
    /// the driver holds the terminal. Routing the event through [`handle_event`]
    /// (rather than intercepting it in the driver) keeps that function the ONE place
    /// every [`AppEvent`] is routed, with an exhaustive match and no dead arm.
    FinishCopy {
        /// What the copy carried (a session id or a preview selection).
        payload: CopyPayload,
        /// Whether a clipboard tool copied it (exited 0 with the text on its stdin).
        copied: bool,
    },
}

impl Outcome {
    /// Whether this outcome ENDS the current board session — the terminal comes
    /// down and the merged event channel with it.
    ///
    /// True for [`Quit`](Self::Quit) and every [`Resume`](Self::Resume); false for
    /// the no-teardown effects (`Send`, `Interrupt`, `BgLaunch`, the interrupt's
    /// `Signal`, and the clipboard copy's `Copy` / `FinishCopy`), which keep
    /// drawing on the SAME channel. Pure, so "does the board survive this?" is one
    /// greppable answer rather than a `matches!` repeated per call site.
    #[must_use]
    pub fn ends_board_session(&self) -> bool {
        matches!(self, Outcome::Quit | Outcome::Resume(_))
    }
}

/// Map a keypress to an [`Action`]. `query_empty` gates the SHIFTED arrows alone:
/// match navigation is only meaningful once a query exists to have marked
/// something, so an empty query leaves them plain selection movement.
/// `has_preview_matches` ([`App::has_preview_matches`]) decides whether the
/// SHIFTED arrows have anywhere to go.
///
/// The shifted arrows are bound CONDITIONALLY and fall through to plain selection
/// movement otherwise, which buys two things at once. With no query — or a query
/// the previewed transcript does not say — `Shift-Up` is bit-for-bit the
/// `MoveUp` it has always been, so nothing a user already relies on changes. And
/// on a terminal that drops the modifier (or a multiplexer that eats the `CSI
/// 1;2A` form), the key arrives as a bare arrow and still moves the selection,
/// which is the graceful degradation rather than a dead key.
///
/// The SHIFTED HORIZONTAL arrows take neither condition: stepping the pane layout
/// means the same thing with or without a query, marks or no marks.
#[must_use]
pub fn key_to_action(key: KeyEvent, query_empty: bool, has_preview_matches: bool) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    if ctrl {
        return match key.code {
            KeyCode::Char('f') | KeyCode::Char('F') => Action::Resume { fork: true },
            KeyCode::Char('a') | KeyCode::Char('A') => Action::ToggleScope,
            KeyCode::Char('n') | KeyCode::Char('N') => Action::NewSession,
            KeyCode::Char('r') | KeyCode::Char('R') => Action::Reply,
            KeyCode::Char('k') | KeyCode::Char('K') => Action::Interrupt,
            KeyCode::Char('c') | KeyCode::Char('C') => Action::Quit,
            // Ctrl-X (0x18 CAN) is the board's leader chord: act on the selected
            // row (hide / hard-delete / copy session ID / fold toggle) or on the
            // board (show-hidden / forced rescan). Unbound and terminal-safe —
            // unlike Ctrl-H/I/M, which alias Backspace/Tab/Enter.
            // It only ARMS the chord; the follow-up key decides (see `chord_key`).
            KeyCode::Char('x') | KeyCode::Char('X') => Action::Chord,
            // Quarter-page preview scroll (readline-style). Acts regardless of
            // the query, like the arrows, so search never blocks preview scrolling.
            KeyCode::Char('u') | KeyCode::Char('U') => Action::PreviewHalfUp,
            KeyCode::Char('d') | KeyCode::Char('D') => Action::PreviewHalfDown,
            // Jump-to-top/bottom, alongside `Home`/`End` (a MacBook's `fn+←/→`
            // reaches those, but not every keyboard/terminal makes that
            // convenient) — same actions, same follow-bottom semantics.
            KeyCode::Char('t') | KeyCode::Char('T') => Action::PreviewTop,
            KeyCode::Char('e') | KeyCode::Char('E') => Action::PreviewBottom,
            // Word-delete (readline's `Ctrl-W`, 0x17 ETB), bound INSIDE this
            // block because it early-returns: an arm for it in the lower match
            // could never be reached. It is the one word-delete key that needs no
            // Alt at all, so it works on a terminal configured to send Option as
            // a composed character rather than as Meta.
            KeyCode::Char('w') | KeyCode::Char('W') => Action::BackspaceWord,
            // Word hop (`CSI 1;5D`/`C`), INSIDE this block for the same
            // early-return reason. It is the non-Alt twin of the `Alt` word hops
            // below, and the pair `TextArea::input` hops words on in the reply box.
            KeyCode::Left => Action::CaretWordBack,
            KeyCode::Right => Action::CaretWordForward,
            _ => Action::Ignore,
        };
    }

    match key.code {
        // Search-match navigation, on the SHIFTED arrows and only while there is
        // something marked to move between. It steps between marked LINES, not
        // between every occurrence: a line can carry several marked runs and is
        // still one stop, because a stop is a place to look and a line is what the
        // jump can scroll to. On `Shift` rather than `Alt-↑`/`↓`: snapback never
        // pushes the kitty keyboard protocol and actively clears it on every board
        // (re)entry (`tui::reset_terminal_state`), so on default macOS terminals
        // `Alt` arrives as a composed character that types junk into the query, and
        // a split ESC read surfaces as a bare `Esc` — which quits the board. `Shift`
        // needs none of that: it rides the ordinary `CSI 1;2A`/`B` encoding. The
        // `Alt` word hops on `←`/`→` below are the narrow exception PATTERNS.md §10
        // allows — a gesture the reply box answers, with a non-Alt twin — and no
        // such case exists for the vertical arrows.
        KeyCode::Up if shift && !query_empty && has_preview_matches => Action::PreviewMatchPrev,
        KeyCode::Down if shift && !query_empty && has_preview_matches => Action::PreviewMatchNext,
        KeyCode::Up => Action::MoveUp,
        KeyCode::Down => Action::MoveDown,
        // Pane layout, on the SHIFTED horizontal arrows: one stop along the
        // 0:1 · 1:3 · 1:1 · 3:1 · 1:0 ladder per press (`PaneLayout::stepped`),
        // `←` toward the preview and `→` toward the list. Unconditional — the
        // layout means the same thing with or without a query — and on `Shift` for
        // the same reason the match step is: it rides the ordinary `CSI 1;2D`/`C`
        // encoding, where `Alt` would reach a default macOS terminal as a composed
        // character or a split `Esc`. These arms MUST sit above every other `Left`
        // / `Right` arm below, the `Alt` word hops included: match arms are tried in
        // order and the unguarded arm matches the shifted key too, so the other
        // order would move the search caret and never step the layout.
        KeyCode::Left if shift => Action::LayoutTowardPreview,
        KeyCode::Right if shift => Action::LayoutTowardList,
        // Word hop on `⌥←`/`⌥→` sent as `CSI 1;3D`/`C`; `ESC b`/`ESC f`, the other
        // byte form of the same gesture, is the `Char('b' | 'f') if alt` arm
        // below. Below the `shift` arms, so `Shift-Alt-←` still steps the layout,
        // and ABOVE the plain arms, which match an `Alt` arrow too and would move
        // the caret one character instead.
        KeyCode::Left if alt => Action::CaretWordBack,
        KeyCode::Right if alt => Action::CaretWordForward,
        // The search caret, unconditionally: editing the query is what the board
        // does on every search, so it gets the plain arrows. Not gated on the query
        // either — an empty query has nowhere to move, and the widget treats a move
        // past either end as a no-op. The lineage fold is `Ctrl-X f` (`chord_key`),
        // so the two never share a key.
        KeyCode::Left => Action::CaretBack,
        KeyCode::Right => Action::CaretForward,
        // Preview scroll: page + jump. Bound regardless of query state (they are
        // not printable, so they never collide with type-to-search).
        KeyCode::PageUp => Action::PreviewPageUp,
        KeyCode::PageDown => Action::PreviewPageDown,
        KeyCode::Home => Action::PreviewTop,
        KeyCode::End => Action::PreviewBottom,
        KeyCode::Enter => Action::Resume { fork: false },
        // `Esc` unwinds one level: a typed query first, the board second.
        // `Ctrl-C` is bound above and always quits.
        KeyCode::Esc if !query_empty => Action::ClearQuery,
        KeyCode::Esc => Action::Quit,
        KeyCode::Tab => Action::ToggleSearchMode,
        // Word-delete on the macOS gesture. This arm MUST sit above the plain
        // `Backspace` arm below: match arms are tried in order and the unguarded
        // one matches `Alt-Backspace` too, so the other order would delete a
        // single character and look like it had worked.
        KeyCode::Backspace if alt => Action::BackspaceWord,
        KeyCode::Backspace => Action::Backspace,
        // The third word-delete key, bound for the same reason as the other two:
        // it is in the set `TextArea::input` maps to a word delete, so the reply
        // box already answers it and the board must answer it too. Only the KEY
        // SET is shared — what a press cuts is the board's own rule (see
        // [`Action::BackspaceWord`]).
        // Guarded on `alt` and therefore above the catch-all below, which would
        // otherwise swallow it.
        KeyCode::Char('h' | 'H') if alt => Action::BackspaceWord,
        // readline's word hops (`ESC b` / `ESC f`) — the bytes some terminals
        // send for `⌥←`/`⌥→` (RustRover's does), and the pair `TextArea::input`
        // hops words on in the reply box. Above the catch-all for the same reason
        // as `Alt-H`.
        KeyCode::Char('b' | 'B') if alt => Action::CaretWordBack,
        KeyCode::Char('f' | 'F') if alt => Action::CaretWordForward,
        // Every OTHER alt-modified printable is swallowed rather than typed. An
        // `Alt`-modified key is a GESTURE the user aimed at some binding, not
        // text — inserting the bare letter would answer `Alt-J` by typing `j`
        // into the query, which is both wrong and invisible. Swallowing keeps the
        // unbound half of the Alt namespace inert and free to bind later.
        KeyCode::Char(_) if alt => Action::Ignore,
        KeyCode::Char(c) => Action::Insert(c),
        _ => Action::Ignore,
    }
}

/// Apply one merged [`AppEvent`] to the app, returning the driver's next step.
///
/// * `Input(Key)` (a press/repeat) -> decode + apply an [`Action`].
/// * `Input(Mouse)` -> a wheel notch scrolls the pane under the pointer, a
///   left-button PRESS over the preview transcript only records where it landed,
///   and its RELEASE decides what it was ([`handle_mouse`] over the pure
///   [`mouse_effect`]). A press that DRAGGED selects transcript text and its
///   release copies it through [`Outcome::Copy`], toggling and opening nothing. A
///   plain CLICK (no drag) on a fold node's header — a peer message or injected
///   context — toggles that node open or closed, and otherwise is resolved
///   against the rendered preview links: a hit on an `http`/`https` url opens it
///   in the default browser and reports `opening <url>` transiently, a hit on any
///   other scheme opens NOTHING and reports a sticky refusal naming the url, a
///   line too big to hit-test reports a sticky [`LinkClick::Unresolvable`]
///   message, and a miss writes nothing (the mapping lives in
///   [`note_link_click`]). All are routed independently of the modal key gate, and
///   a press cannot start a selection, toggle a node or open a link while an
///   overlay is open or a new-session draft's card replaces the transcript (the
///   `App::preview_pointer_blocked` gate, behind [`press_starts_selection`]) — but
///   it can while a QUICK REPLY is open, which previews the real transcript.
///   Nothing else: the pane widths belong to the keyboard (`Shift-Left` /
///   `Shift-Right`).
/// * `Input(Resize)` -> clear the preview's mouse selection (a new width re-wraps
///   the transcript its anchors name); the next frame re-lays the board out.
/// * `SessionsChanged` -> reload `store` and re-apply query+scope, preserving
///   selection-by-id and scroll (see [`reload_board`]).
/// * `ModelAliases` -> swap in the alias set the off-thread probe read off the
///   installed `claude`, so the next compose's `Ctrl-L` offers it instead of the
///   seed.
/// * `SettingsModel` -> store the model the user's `claude` settings name for a
///   new session and whether an environment override stops claude restoring a
///   session's model — what a compose box's `model:` label and its picker's first
///   row then show.
/// * `Tick` -> nothing costly: advance the board clock, age a transient status,
///   then replay any completion an earlier board session could not read
///   ([`replay_undelivered`]). It never steps a held drag's autoscroll: the run
///   loop does that after every event and at a frame deadline of its own
///   ([`App::autoscroll_preview_selection`]).
///
/// Every return runs through ONE teardown seam: an outcome that
/// [ends the board session](Outcome::ends_board_session) also tears the compose
/// surface down, because neither half of it can outlive the channel it reports on
/// (see [`dispatch`]).
pub fn handle_event(app: &mut App, event: AppEvent, store: &mut SessionStore) -> Outcome {
    let outcome = dispatch(app, event, store);
    if outcome.ends_board_session() {
        // The compose surface is bounded by the board session, and the IN-FLIGHT
        // draft card is why that has to be enforced here rather than left to each
        // route. It is the one part of the surface that outlives its editor, so
        // `Ctrl-F` / `Enter` on a row stay routable underneath it — and the
        // `BgLaunchFinished` that would have closed it cannot survive the hand-off:
        // `tui::run_inner` builds a fresh `EventLoop` per board session and drops
        // the old receiver, while `lib::run` re-enters the board on the SAME `App`.
        // A card left standing there would replace EVERY session's transcript with
        // a placeholder and hold `preview_pointer_blocked` true (killing link clicks, fold
        // toggles and drag-selections) until another compose was opened and
        // cancelled.
        app.close_compose();
    }
    outcome
}

/// The body of [`handle_event`]: route one event to its handler.
///
/// Split out only so the teardown seam above sees every outcome — the routes below
/// return from several places, and a rule that must hold for ALL of them cannot be
/// a line each of them remembers to run.
fn dispatch(app: &mut App, event: AppEvent, store: &mut SessionStore) -> Outcome {
    match event {
        AppEvent::Input(Event::Key(key)) if is_actionable(key) => {
            // Any keypress ends a mouse text selection, the way a terminal's own
            // selection ends when you type. The selection is anchored to the
            // transcript's content, so a scroll alone would not strand it — but a
            // key can move the row selection to another transcript, re-lay the
            // panes out, or open an overlay over the pane, and it is the user
            // acting on the board rather than on the selection. Done first so it
            // fires whoever owns the keyboard next (board, modal, compose). Ticks
            // (which redraw constantly) deliberately do NOT clear, so a finished
            // selection stays highlighted until the user acts.
            app.clear_preview_selection();
            // While a modal overlay (the running-session choice, the new-session
            // agent picker, or the hard-delete confirm) is open it OWNS the
            // keyboard: keys navigate/confirm/cancel the modal, never the board.
            if app.modal.is_some() {
                return handle_modal_key(app, key, store);
            }
            // A pending `Ctrl-X` leader chord OWNS the next key too: route it through
            // the chord machine BEFORE normal handling so a printable follow-up
            // (`x`/`d`/`h`/`r`/`y`/`f`) completes the chord instead of leaking into
            // the query.
            if app.pending_chord {
                return handle_chord_key(app, key, store);
            }
            // The "stop the waiting agent?" confirmation owns the keyboard until it
            // resolves into compose (Enter) or is dismissed (Esc).
            if app.pending_stop.is_some() {
                return handle_stop_confirm_key(app, key);
            }
            // The "stop this agent?" interrupt confirmation likewise owns the keyboard
            // until it resolves into a stop (Enter) or is dismissed (Esc).
            if app.pending_interrupt.is_some() {
                return handle_interrupt_confirm_key(app, key);
            }
            // The quick-reply compose zone owns the keyboard while open: every key
            // routes to the compose handler (Enter sends, Ctrl-J/Alt+Enter add a
            // newline, Esc cancels, the rest edit the buffer), bypassing
            // `key_to_action` entirely — mirroring the two overlays above.
            if app.is_composing() {
                return compose::handle_compose_key(app, key);
            }
            // A transient status (e.g. a resume refusal) lives exactly until the
            // next key; clear it first so this keypress may set a fresh one.
            app.clear_status();
            let action = key_to_action(key, app.query_input.is_empty(), app.has_preview_matches());
            apply_action(app, action)
        }
        // Mouse wheel scroll, preview fold-node toggles, preview link clicks and
        // preview drag-selection. A dedicated arm BEFORE the input catch-all and
        // INDEPENDENT of the modal overlay gate above: none routes into the modal
        // handler — a wheel just scrolls a pane and never crashes in any mode
        // (query active, modal open, ...), and a press neither toggles a node, opens
        // a link nor starts a selection while an overlay is up
        // (`App::preview_pointer_blocked` gates it; an open QUICK REPLY does not, since
        // none of the three touches what the reply holds). A finished drag answers
        // `Outcome::Copy`, the same copy request `Ctrl-X y` makes.
        AppEvent::Input(Event::Mouse(mouse)) => handle_mouse(app, mouse),
        // A terminal PASTE (bracketed paste, enabled in `tui::init_terminal`). A
        // dedicated arm BEFORE the input catch-all that used to swallow it, and
        // routed by [`handle_paste`] through the SAME precedence the key arm above
        // uses — see that fn for what each keyboard owner does with one.
        AppEvent::Input(Event::Paste(text)) => {
            handle_paste(app, &text);
            Outcome::Continue
        }
        // A RESIZE clears the preview's mouse selection — a finished one and a
        // held one alike. The selection is anchored to rows of the transcript as
        // wrapped at the pane's width, and a new width re-wraps it, so the same row
        // number would name other text. Nothing else here reacts: the next frame
        // re-lays the board out at the new size on its own.
        AppEvent::Input(Event::Resize(..)) => {
            app.clear_preview_selection();
            Outcome::Continue
        }
        AppEvent::Input(_) => Outcome::Continue,
        AppEvent::SessionsChanged => {
            reload_board(app, store);
            Outcome::Continue
        }
        AppEvent::ReportedAgents {
            agents,
            reported_at_ms,
        } => {
            // Delivered off-thread by the agents poller; just swap the map in,
            // with the wall-clock instant the poller stamped it at. The stamp is
            // CARRIED, never read here, so this arm stays clock-free (see
            // `AppEvent::ReportedAgents::reported_at_ms`).
            app.set_reported_agents(agents, reported_at_ms);
            Outcome::Continue
        }
        AppEvent::ModelAliases(aliases) => {
            // Delivered ONCE, off-thread, by the `--model` alias probe; just swap
            // the list in. An empty list is the probe's "could not read it" answer
            // and is stored as-is — the picker reads empty as "keep the seed", so a
            // failed probe degrades rather than emptying the overlay.
            //
            // Deliberately silent: which aliases the picker offers is a fact true
            // over an INTERVAL, rendered by the picker itself, so it never reaches
            // the keypress-scoped status line (STATUS-LINE OWNERSHIP). It also does
            // not touch the selection or trigger a reload — nothing about the board's
            // rows depends on it.
            app.set_model_aliases(aliases);
            Outcome::Continue
        }
        AppEvent::SettingsModel(defaults) => {
            // Delivered ONCE per board session, off-thread, by the settings read;
            // just store both answers. A `None` new-session model is "the settings
            // name no model" and a draft reads it as plain `model: default`; a set
            // restore override turns a reply's `model: session (…)` into
            // `model: default`.
            //
            // Deliberately silent, for the same reason as `ModelAliases`: both facts
            // are true over an INTERVAL and rendered by the compose boxes and their
            // picker, never by the keypress-scoped status line (STATUS-LINE
            // OWNERSHIP).
            app.set_settings_model(defaults.new_session);
            app.set_restore_overridden(defaults.restore_overridden);
            Outcome::Continue
        }
        AppEvent::SendFinished {
            session_id,
            status,
            success,
        } => {
            // A one-shot quick-reply send completed off-thread. Clear THIS session's
            // in-flight entry — only it: a reply still running to another session
            // keeps its own — and surface the mapped result (cost / error) on the
            // status line. Successes are transient confirmations; failures and
            // refusals stay sticky.
            app.clear_sending(&session_id);
            if success {
                app.set_status_transient(status);
            } else {
                app.set_status(status);
            }
            // If the finished send targets the row on screen, re-anchor the
            // preview to the newest turn so the reply — arriving via the separate
            // `SessionsChanged` reload — lands in view. The reply body itself is
            // NOT read here; the watcher → reload → preview path renders it.
            if app.selected.as_deref() == Some(session_id.as_str()) {
                app.preview_bottom();
            }
            Outcome::Continue
        }
        AppEvent::InterruptFinished {
            session_id,
            status,
            success,
        } => {
            // A one-shot interrupt (`claude stop`) completed off-thread; surface its
            // result. The live badge clears on the next agents poll (~5s while the
            // board is active; skipped, and thus unbounded, while idle past
            // AGENTS_IDLE_AFTER), and the transcript is unchanged (stopping keeps
            // the conversation), so there is nothing else to reconcile. Successes
            // are transient; failures sticky.
            //
            // Clear the in-flight guard only when the ids match, so a stale result
            // cannot land on a surface that has moved on — the interrupt twin of the
            // session-keyed `clear_sending` above.
            if app.interrupting_on(&session_id).is_some() {
                app.interrupting = None;
            }
            if success {
                app.set_status_transient(status);
            } else {
                app.set_status(status);
            }
            Outcome::Continue
        }
        AppEvent::BgLaunchFinished {
            launch_id,
            status,
            success,
        } => {
            // A one-shot background-agent launch completed off-thread; surface its
            // result (started / started-but-warned / the failure reason) and close
            // the draft card that was reporting THIS launch in flight. There is
            // deliberately nothing else to do: the new agent has no id the board
            // knows yet, and it arrives on the list through the ordinary watcher →
            // reload path like any other session.
            //
            // The identity check is the launch's twin of the session key
            // `clear_sending` clears by above, and it is load-bearing for the same
            // reason: the card outlives its editor, so a result can land on a
            // surface that is no longer this launch's — a quick reply, or a second
            // draft — and closing blindly would throw away a buffer the user is
            // still typing into.
            if success {
                app.set_status_transient(status);
            } else {
                app.set_status(status);
            }
            if app.launching_draft(launch_id).is_some() {
                app.close_compose();
            }
            Outcome::Continue
        }
        // A clipboard-tool copy finished off-thread. Completing it is the DRIVER's
        // job, not this handler's: when no tool copied the text, the OSC 52 fallback
        // has to be written to the terminal, and only the driver holds the
        // terminal's writer. So the result is handed straight back to it (see
        // `Outcome::FinishCopy`), and the status is set there, from this result.
        AppEvent::CopyFinished { payload, copied } => Outcome::FinishCopy { payload, copied },
        AppEvent::Tick => {
            // The tick already drove a redraw; counting it turns that existing
            // cadence into the board's clock, which `view::blink_visible` phases
            // the live-badge pulse from. `wrapping_add` so a board left running
            // for eons rolls over instead of overflow-panicking in debug.
            app.tick = app.tick.wrapping_add(1);
            // Age transient statuses (confirmations/nudges) on the same cadence.
            // Failures/refusals stay sticky until the next actionable keypress.
            app.tick_status();
            // THEN hand over any completion an earlier board session could not
            // read. After the aging on purpose: a replayed confirmation starts its
            // full dwell here instead of losing a tick of it to this same event.
            replay_undelivered(app, store);
            Outcome::Continue
        }
    }
}

/// Hand every completion an earlier board session could not read to the SAME arm
/// a live one reaches, so it is handled exactly as if it had arrived on this
/// board's channel.
///
/// Such a completion is kept in [`App::take_undelivered`]'s queue by the send
/// thread, or by the teardown drain (see [`crate::send::UndeliveredEvents`]). The
/// `SendFinished` arm clears only the `App::sending` entry keyed by its own
/// `session_id`, so a stale completion cannot clear a reply that is not its own.
/// Its status keeps the live split, transient on success and sticky on failure, and a
/// finished send on the selected row re-anchors the preview as usual.
///
/// Called on every `Tick` (after `tick_status`) and once at board entry, before
/// the first draw ([`crate::tui::run`]). The queue is taken with ONE lock-and-take,
/// and the lock is released before any event is handled, so the render loop never
/// waits on a send thread. Every event in the queue is a `SendFinished`, and its
/// arm answers `Continue`, so discarding `dispatch`'s outcome loses nothing and
/// nothing here can end the board. That holds only because nothing else is ever
/// queued. Two paths fill the queue, and each admits `SendFinished` alone:
/// [`crate::send::spawn_send`] is `deliver`'s only non-test caller, and the
/// teardown drain ([`crate::send::UndeliveredEvents::drain_then_drop`]) discards
/// every other event. Not every completion answers `Continue` (`CopyFinished`
/// answers `FinishCopy`), so queueing another kind must revisit this.
pub fn replay_undelivered(app: &mut App, store: &mut SessionStore) {
    for event in app.take_undelivered() {
        dispatch(app, event, store);
    }
}

/// Reload the board from `store` — the ONE seam every reload path funnels
/// through (the `SessionsChanged` watcher event, the post-delete reload, and the
/// `Ctrl-X r` forced rescan), the way [`App::apply_reload`] is the one funnel on
/// the model side.
///
/// The reload is INCREMENTAL: the store re-parses only the transcripts whose
/// `(mtime, len)` moved and hands back which ones those were, so the derived
/// caches drop exactly those rows. Discovery still runs in full every time, so a
/// created or deleted session always lands on the board.
fn reload_board(app: &mut App, store: &mut SessionStore) {
    app.apply_reload(store.reload());
}

/// Only press/repeat key events act; release events (kitty protocol / Windows)
/// are ignored so a keystroke is never handled twice.
fn is_actionable(key: KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

/// The most CHARACTERS one terminal paste may contribute, to the compose draft and
/// to the board query alike.
///
/// The limit is about COST, not layout: `view::COMPOSE_MAX_TEXT_ROWS` already caps
/// the compose box at 6 rows and the editor scrolls beyond that, so an enormous
/// paste never breaks the geometry. What it does break is the per-frame work behind
/// it — `compose::ComposeState::screen_rows` CLONES the whole `TextArea` (lines,
/// undo history and screen map) on every redraw to probe its wrapped height, at the
/// `watch::TICK` cadence for as long as the draft is open. On the board the same
/// text feeds `search::SearchIndex::set_query`, which rebuilds the pattern and one
/// substring finder per space-separated atom.
///
/// 4096 is chosen to sit far above anything a human composes or pastes into a
/// one-shot reply — a stack trace, a failing test's output, a diff hunk — while
/// keeping both of those costs the same order of magnitude as typed input. ONE
/// const covers both destinations deliberately: "how much text can arrive in a
/// single paste?" deserves one greppable answer, and the board query's cost curve
/// (linear in atoms) is gentler than the draft's, so a cap safe for the draft is
/// safe there too.
const PASTE_MAX_CHARS: usize = 4_096;

/// The status a paste longer than [`PASTE_MAX_CHARS`] reports.
///
/// TRUNCATE, not reject, and say so: rejecting a too-long paste outright throws away
/// the part that DID fit and leaves the user nothing to edit down, while truncating
/// keeps the head they can see. What makes truncation acceptable is that it is never
/// silent — this line takes the help row (a status wins it, even while composing),
/// so a shortened paste is always reported rather than discovered later in a sent
/// reply.
///
/// A fn rather than a `const` because the NUMBER is the message: naming the cap is
/// what turns "some of it was dropped" into "here is exactly how much landed", and
/// a `&'static str` cannot interpolate it.
fn paste_truncated_status() -> String {
    format!("pasted text was too long — kept the first {PASTE_MAX_CHARS} characters")
}

/// One terminal paste, normalized and capped — what the routing below is allowed to
/// insert anywhere.
struct AcceptedPaste {
    /// The accepted text: line endings normalized to `\n`, at most
    /// [`PASTE_MAX_CHARS`] chars.
    text: String,
    /// Whether the cap dropped a tail, so the caller can say so.
    truncated: bool,
}

/// Normalize and cap one pasted string — the SINGLE gate every pasted character
/// passes before it can reach a draft or the query. Pure, so both the line-ending
/// rules and the cap are unit-testable without a terminal.
///
/// Two jobs, deliberately fused so neither can be skipped at a call site:
///
/// * **Line endings collapse to `\n`.** A terminal may deliver a paste with CRLF
///   (Windows clipboards, and anything copied out of a CRLF file) or with a LONE CR
///   — the classic form for an embedded newline inside a bracketed paste. Left
///   as-is, a stray `\r` reaches the draft as an invisible control character and the
///   query as a byte no session label can contain, so the text silently stops
///   matching. `\r\n` collapses to ONE `\n`, never two.
/// * **The cap is counted in CHARS, and taken from the NORMALIZED stream.** Counting
///   chars rather than bytes is what makes truncation UTF-8 safe by construction:
///   there is no index to land mid-codepoint on, so a paste ending in emoji or CJK
///   cannot panic the way a naive byte slice would. Counting AFTER normalization
///   means a CRLF pair costs one character, exactly like the `\n` it becomes.
fn accept_paste(raw: &str) -> AcceptedPaste {
    let mut text = String::new();
    let mut taken = 0usize;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if taken == PASTE_MAX_CHARS {
            // Something is left, so the tail was dropped.
            return AcceptedPaste {
                text,
                truncated: true,
            };
        }
        let c = if c == '\r' {
            // CRLF is ONE newline: swallow the LF that follows a CR.
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            '\n'
        } else {
            c
        };
        text.push(c);
        taken += 1;
    }
    AcceptedPaste {
        text,
        truncated: false,
    }
}

/// Flatten an accepted paste into the SINGLE-LINE board query: every newline
/// becomes a space. Pure.
///
/// The alternative — keep the first line and drop the rest — silently discards what
/// the user pasted, and the query's own tokenization says it is not needed:
/// `search::gate_atoms` splits the query on unescaped spaces into substring atoms
/// that must ALL match, so `foo\nbar` flattens to exactly the `foo bar` the user
/// could have typed, with the same meaning. Nothing is lost, and the result composes
/// with type-to-search rather than defining a second rule beside it. Runs of
/// newlines become runs of spaces, which the atom splitter already drops as empty
/// atoms.
///
/// Only `\n` needs handling: [`accept_paste`] has already turned every `\r` into
/// one.
fn flatten_for_query(text: &str) -> String {
    text.replace('\n', " ")
}

/// Route one terminal paste (bracketed paste, enabled in [`crate::tui::init_terminal`]).
///
/// The paste arm mirrors the KEY arm's precedence exactly, because the reason the
/// key arm has that order applies unchanged: a surface that owns the keyboard must
/// not have text land on the surface behind it. All six owners, in order:
///
/// 1. **Modal** ([`handle_modal_key`]) — IGNORED. A modal is a fixed choice
///    (Attach/Fork/Cancel, the agent picker, the delete confirm); it has no text
///    field, so the only things a paste could do are pick an option the user did not
///    choose or leak into the board's query underneath an overlay that hides it.
/// 2. **`Ctrl-X` leader chord** ([`handle_chord_key`]) — IGNORED, and it does NOT
///    resolve the chord. The chord resolves on exactly one KEY, hit or miss; a paste
///    carries no chord completion, and cancelling on one would let a stray paste
///    silently disarm a chord whose hint is still on screen. The next key still
///    decides.
/// 3. **Stop confirmation** ([`handle_stop_confirm_key`]) — IGNORED. A plain
///    Enter/Esc gate: a paste is neither, and must not stop an agent.
/// 4. **Interrupt confirmation** ([`handle_interrupt_confirm_key`]) — IGNORED, same
///    reason.
/// 5. **Compose** — INSERTED at the caret as text, via [`compose::insert_paste`].
///    This is the fix: the newline inside a paste becomes a newline in the draft
///    instead of the `Enter` that used to submit it.
/// 6. **Board** — INSERTED at the search query's caret, newlines flattened to
///    spaces ([`flatten_for_query`]), exactly as if typed.
///
/// Returns nothing on purpose. A paste can never produce [`Outcome::Send`],
/// [`Outcome::Resume`] or any other board-ending outcome, and that is structural
/// here rather than a promise: no branch reaches a submit path.
fn handle_paste(app: &mut App, raw: &str) {
    // Owners 1-4: swallow. Same order, same reasons, as the key arm in `dispatch`.
    if app.modal.is_some()
        || app.pending_chord
        || app.pending_stop.is_some()
        || app.pending_interrupt.is_some()
    {
        return;
    }

    let accepted = accept_paste(raw);
    if accepted.text.is_empty() {
        return;
    }

    if app.is_composing() {
        // Owner 5: the draft takes it verbatim, newlines and all.
        compose::insert_paste(app, &accepted.text);
    } else {
        // Owner 6: the board. Clear the transient status first, exactly as an
        // actionable keypress does, so a stale refusal does not outlive this input.
        app.clear_status();
        app.push_query_str(&flatten_for_query(&accepted.text));
    }

    if accepted.truncated {
        app.set_status_transient(paste_truncated_status());
    }
}

/// Which pane a mouse wheel targets, resolved by [`wheel_target`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WheelTarget {
    /// Scroll the transcript preview.
    Preview,
    /// Move the list selection.
    List,
    /// Swallow the notch: scroll nothing, move nothing, say nothing. The one
    /// outcome with no effect at all, and it exists for exactly one case —
    /// a notch over the list while a draft is open ([`wheel_target`] owns why).
    Ignore,
}

/// Hit-test a wheel event at `(col, row)` against the pinned pane rects.
///
/// The preview wins when the point is inside `preview`; the list when inside
/// `list`; otherwise the preview is the default surface (it is the primary thing
/// you scroll). The hidden-preview case leaves `preview` EMPTY, so a point over
/// the now-full-width list still routes to the list. Pure so it is unit testable
/// from coordinates + rects without a terminal.
///
/// `composing` is the ONE condition that changes any of that, taken as a
/// parameter the way [`key_to_action`] takes its own (PATTERNS §10), so the whole
/// routing decision stays here and pure. It narrows exactly ONE of the three
/// zones: while a draft is open the LIST is not a wheel target, and a notch over
/// it resolves to [`WheelTarget::Ignore`] — nothing happens. Only the list arm
/// earns that, because alone among the three it does not scroll a viewport: it
/// MOVES THE SELECTION, and the selection is what the preview shows. An open
/// draft targets ONE session id, so a notch that strayed over the list would take
/// the session being replied to off screen (and reset its scroll) while the draft
/// went on addressing it. The notch is DROPPED rather than redirected — a pointer
/// parked over the list is not asking for the preview, and silently scrolling a
/// pane it is not over would be a second surprise in place of the first.
///
/// The other two zones are deliberately untouched. Inside `preview` a notch
/// scrolls the transcript being written to, exactly as always; the docked editor
/// needs no arm of its own because it is drawn INSIDE that rect. OUTSIDE BOTH
/// rects the preview stays the default surface, so the bottom-bar composer, the
/// search line and the help line — all of which render outside the two panes —
/// keep scrolling the transcript mid-draft instead of going dead.
fn wheel_target(col: u16, row: u16, preview: Rect, list: Rect, composing: bool) -> WheelTarget {
    let pos = Position { x: col, y: row };
    if preview.contains(pos) {
        WheelTarget::Preview
    } else if list.contains(pos) {
        if composing {
            WheelTarget::Ignore
        } else {
            WheelTarget::List
        }
    } else {
        WheelTarget::Preview
    }
}

/// What one mouse event asks of the world beyond the [`App`] — decided by
/// [`mouse_effect`] with no process spawned and no terminal touched, so a test can
/// press, drag and release over a drawn link without a browser ever opening.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MouseEffect {
    /// Nothing beyond the state change already applied — which may be a fold node
    /// toggled open or closed, or a link click's status line.
    None,
    /// A plain CLICK — press and release with no drag — over a drawn link whose
    /// scheme snapback opens ([`LinkClick::Opening`]): open this url.
    OpenLink(String),
    /// A finished DRAG: copy this selected text.
    Copy(String),
}

/// Apply a mouse event and carry out what it decided: [`mouse_effect`] owns the
/// decision, this owns the two effects. A click's link goes to the
/// fire-and-forget, off-thread [`resume::open_url`] — the ONE statement in the
/// link path a test cannot assert, which is the point of keeping it alone here
/// (PATTERNS §3); its status was already written by [`note_link_click`], BEFORE
/// this spawn, so the board has the message whatever the opener does with it. A
/// finished drag answers [`Outcome::Copy`] with the selected text — the SAME
/// request `Ctrl-X y` makes, so the copy takes the one clipboard path (tool first,
/// OSC 52 fallback) and [`finish_copy`] reports what really happened. Every other
/// event answers [`Outcome::Continue`]. Fails soft end to end: a bad url or a
/// missing opener never crashes the board.
fn handle_mouse(app: &mut App, mouse: MouseEvent) -> Outcome {
    match mouse_effect(app, mouse, Instant::now()) {
        MouseEffect::None => Outcome::Continue,
        MouseEffect::OpenLink(url) => {
            resume::open_url(&url);
            Outcome::Continue
        }
        MouseEffect::Copy(text) => Outcome::Copy(CopyPayload::Selection(text)),
    }
}

/// Decide a mouse event — the state change it makes, and the effect it asks for.
///
/// A vertical wheel notch scrolls whichever pane the pointer is over — unless a
/// draft is open, which takes the LIST out of the wheel's reach so a notch there
/// is swallowed ([`wheel_target`] owns that rule and the reason for it) — and
/// drops any selection first.
///
/// The left button over the preview transcript is a CLICK or a DRAG, and which one
/// is only known when it comes back UP — so nothing acts on the press:
///
/// - the PRESS records the CONTENT cell it landed on ([`App::begin_preview_press`]),
///   if [`press_starts_selection`] admits it, and toggles or opens nothing;
/// - a DRAG moves the held pointer and extends a selection from that press to the
///   content under it ([`App::extend_preview_selection`]); a pointer held past the
///   transcript's top or bottom edge is then scrolled toward by the run loop's
///   autoscroll step, a small one every `app::AUTOSCROLL_FRAME`
///   ([`App::autoscroll_preview_selection`]), never by the drag events themselves;
/// - the RELEASE resolves it ([`release_preview_press`]): a drag copies the WHOLE
///   selection, redrawn off screen ([`view::preview_selection_copy`],
///   [`MouseEffect::Copy`]) — or does nothing when it holds no drawn text — while a
///   plain click is resolved by [`click_effect`] at the press's content cell, where
///   the pane shows it now: the fold node whose header is under it toggles, else
///   the link under it opens. So a drag that happens to start on a node header or a
///   link selects, and toggles or opens nothing.
///
/// A plain MOVE while the press is still held is also the release. With the
/// button really down the terminal reports a DRAG, never a move, so a move means
/// the release was lost; resolving it here is what keeps a lost release from
/// autoscrolling for as long as the board runs.
///
/// Any other event (other buttons, horizontal wheel, a move with no press held) is
/// ignored: the pane widths belong to the keyboard (`Shift-Left` / `Shift-Right`),
/// so the mouse has no border to drag. Never touches the query, and the press gate
/// keeps a click from toggling a node or opening a link — or a drag or a
/// double-click from selecting — under an overlay: a press it refuses records
/// nothing, so its release has nothing to resolve.
///
/// A SECOND admitted press on the SAME cell as the previous one, within
/// [`DOUBLE_CLICK_INTERVAL`], is a DOUBLE-CLICK ([`is_double_click`]): it records a
/// WORD selection
/// ([`App::begin_word_selection`]), so its release takes the copy arm above and
/// never reaches [`click_effect`] — the first release already toggled or opened
/// whatever sat under the cell, and the second must not do it again.
///
/// `now` is the instant the press is handled at, passed in so the double-click
/// decision ([`is_double_click`]) is a pure function a test can drive with
/// synthetic times; [`handle_mouse`] is the one place that reads the clock.
fn mouse_effect(app: &mut App, mouse: MouseEvent, now: Instant) -> MouseEffect {
    let pos = Position {
        x: mouse.column,
        y: mouse.row,
    };
    match mouse.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            // A wheel notch ends any selection before it scrolls, held or
            // finished. The selection is content-anchored, so it would survive the
            // scroll itself — but a notch during a held drag would then have to
            // extend the drag as well, and it does not: the drag's own autoscroll
            // is how a selection moves the pane.
            app.clear_preview_selection();
            let up = mouse.kind == MouseEventKind::ScrollUp;
            match wheel_target(
                mouse.column,
                mouse.row,
                app.preview_rect,
                app.list_rect,
                app.is_composing(),
            ) {
                WheelTarget::Preview => app.preview_wheel(up),
                WheelTarget::List => app.list_wheel(up),
                // Deliberately empty: no scroll, no selection move, no status
                // line. The notch is spent and the board is byte-for-byte what
                // it was, which is the whole point of the arm.
                WheelTarget::Ignore => {}
            }
        }
        // A left press inside the transcript starts a POTENTIAL drag-select and
        // toggles or opens NOTHING yet: the selection only materializes once the
        // pointer moves, and a press that never moves is a plain click, resolved
        // on release (`click_effect`: fold toggle, else link open). So a single
        // click both toggles a node or opens a link as before and de-highlights
        // any prior selection. `press_starts_selection` owns where a press may
        // land and when — gated by any overlay, so a press while the
        // running-session choice or the agent picker owns input records nothing.
        //
        // A SECOND admitted press on the same cell within `DOUBLE_CLICK_INTERVAL`
        // records a WORD selection instead (`is_double_click`); the view expands it
        // and the release copies it through the same path a drag's does — and,
        // holding a selection, never reaches `click_effect`, so the second click
        // toggles or opens nothing the first one already did.
        MouseEventKind::Down(MouseButton::Left) if press_starts_selection(app, pos) => {
            let transcript = view::preview_transcript_rect(app);
            if is_double_click(app.last_click(), pos, now) {
                app.begin_word_selection(pos, transcript);
            } else {
                app.begin_preview_press(pos, transcript);
            }
            app.note_click(pos, now);
        }
        // A held-button drag moves the pointer and extends the text selection to
        // the content under it. A no-op unless a press is active
        // (`begin_preview_press` set it), so a drag that began on the list, the
        // pinned row or a docked compose zone never selects; the cursor is clamped
        // into the SAME transcript rect the press was gated on, while the raw
        // pointer is kept for the autoscroll.
        MouseEventKind::Drag(MouseButton::Left) => {
            app.extend_preview_selection(pos, view::preview_transcript_rect(app));
        }
        // The release resolves the press, and only a press the gate admitted: a
        // completed drag (or a double-click's word) copies the selection and never
        // toggles a node or opens a link, even one it started on; a plain click
        // with no drag is THE one pane click, resolved at the press's content cell
        // (`release_preview_press`). A MOVE while the press is held stands in for a
        // release the terminal lost (see the doc above); with no press held it
        // finds nothing to take.
        MouseEventKind::Up(MouseButton::Left) | MouseEventKind::Moved => {
            return release_preview_press(app);
        }
        _ => {}
    }
    MouseEffect::None
}

/// Resolve the held preview press as its release, ending the gesture and with it
/// any autoscroll. A completed drag (or a double-click's word) copies the WHOLE
/// selection — redrawn off screen by the rules its highlight is drawn with
/// ([`view::preview_selection_copy`]), so rows scrolled past during the drag are
/// copied too — and never toggles a node or opens a link, even one it started on;
/// a selection over blank cells alone copies nothing. A plain click with no drag
/// is handed to [`click_effect`] at the press's CONTENT cell, found on screen where
/// the pane shows it now ([`screen_at`]) — nothing when that cell has left the
/// transcript rect. [`MouseEffect::None`] when no press was held.
fn release_preview_press(app: &mut App) -> MouseEffect {
    let Some(press) = app.take_preview_press() else {
        return MouseEffect::None;
    };
    if app.has_preview_selection() {
        return view::preview_selection_copy(app).map_or(MouseEffect::None, MouseEffect::Copy);
    }
    screen_at(
        press,
        view::preview_transcript_rect(app),
        app.preview_scroll,
    )
    .map_or(MouseEffect::None, |cell| click_effect(app, cell))
}

/// The longest gap between two presses on the same cell that still makes the
/// second a double-click. crossterm reports no double-click event, so the two
/// presses are timed here. 500 ms is the platform default the user's other
/// programs already use: macOS `NSEvent.doubleClickInterval` is 0.5 s and
/// Windows' default double-click time is 500 ms. The OS setting is deliberately
/// not read.
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);

/// Whether a press at `pos` at `now` is a double-click: the previous admitted
/// press landed on the SAME cell no more than [`DOUBLE_CLICK_INTERVAL`] ago. Pure
/// over its three inputs. A chain keeps going — each press is compared to the one
/// before it — so a quick third click is a double-click too and keeps the word.
/// A `now` earlier than the previous press (a clock that stepped back) is not one.
fn is_double_click(previous: Option<ClickRecord>, pos: Position, now: Instant) -> bool {
    previous.is_some_and(|prev| {
        prev.pos == pos
            && now
                .checked_duration_since(prev.at)
                .is_some_and(|gap| gap <= DOUBLE_CLICK_INTERVAL)
    })
}

/// Whether a left press at `pos` may begin a preview press — the ONE gate in
/// front of all three mouse actions over the preview, since a node toggles, a link
/// opens and a selection starts — a drag's, or a double-click's word — only from a
/// press this admits. A press it refuses is not a click either: it records
/// nothing, so it cannot become the first half of a double-click.
///
/// Three conditions, all required:
///
/// - nothing blocks the pointer ([`App::preview_pointer_blocked`]) — an overlay, a
///   pending confirmation or chord, or the new-session draft CARD (editor or in
///   flight), so no selection starts under a draft card and no node toggles or
///   link opens from a transcript the card hides. An open QUICK-REPLY editor is
///   not one: it previews the real transcript above its own docked box;
/// - a session is selected, so the "No session selected." placeholder is never
///   selectable text;
/// - `pos` is inside the preview's TRANSCRIPT rect,
///   [`view::preview_transcript_rect`] — the same rect [`fold_under_pointer`] and
///   [`resolve_link_click`] resolve a click against, and the one the view drew the
///   text in. The pinned row above it, the pane's own border and a docked compose
///   zone are outside it by construction.
fn press_starts_selection(app: &App, pos: Position) -> bool {
    !app.preview_pointer_blocked()
        && app.selected_session().is_some()
        && view::preview_transcript_rect(app).contains(pos)
}

/// Resolve a plain CLICK — a press released with no drag — at its PRESS cell:
/// THE one owner of a pane click, and the one place its precedence lives. Read
/// end to end: fold toggle -> link open.
///
/// - On a fold node's HEADER — a peer message or injected context — it toggles
///   that node open or closed ([`App::toggle_peer_fold`], which also drops any
///   mouse selection but KEEPS the click chain, so a quick second press on the
///   header is a double-click that selects a word rather than a second toggle)
///   and asks for nothing more.
/// - Otherwise it is resolved by [`resolve_link_click`] and given its status by
///   [`note_link_click`]: a hit on an `http`/`https` link asks [`handle_mouse`]
///   to open its url ([`MouseEffect::OpenLink`]) and reports `opening <url>`
///   transiently; a hit on any OTHER scheme opens nothing and reports a STICKY
///   refusal naming the url (the gate is [`preview::has_openable_scheme`]); a line
///   too big to hit-test reports a sticky [`LinkClick::Unresolvable`] message; and
///   a miss writes nothing.
///
/// Resolved at the PRESS cell because that is the cell the gate vetted: a press
/// is admitted only inside the transcript rect ([`press_starts_selection`]), so a
/// press on the pane's own BORDER or on its pinned row never records at all. The
/// hit-tests keep their own containment behind that gate — `view::content_hit`
/// checks the transcript's INNER rect on both axes — so a border cell could not
/// alias onto content column 0 even if it got this far.
///
/// Fold-before-link is FREE, not a tie-break. A node's header line is built from
/// the marker, the sender, the timestamp and the affordance alone; every link
/// region a node produces belongs to its BODY and is rebased strictly BELOW the
/// header row (`store::preview::peer_node_lines`, `injected_node_lines`), so no
/// content row is ever claimed by both a `FoldRegion` and a `LinkRegion` and
/// neither order can swallow the other's click. The order is written down anyway
/// because that is a property
/// of today's render rather than a guarantee of it: it is what would decide the
/// collision if a header ever did carry a link, and
/// `a_peer_node_header_carries_no_link_regions_at_any_width` and
/// `an_injected_node_header_carries_no_link_regions_at_any_width` — one per node
/// kind, since each builds its header on its own path — are the tests that go red
/// the moment the premise stops holding.
fn click_effect(app: &mut App, press: Position) -> MouseEffect {
    // One derivation of the transcript rect for both halves, so the width the
    // toggle re-renders at is the width the hit-test resolved through.
    let transcript = view::preview_transcript_rect(app);
    if let Some(key) = fold_under_pointer(app, press.x, press.y) {
        app.toggle_peer_fold(&key, transcript.width);
        return MouseEffect::None;
    }
    let click = resolve_link_click(app, press.x, press.y);
    note_link_click(app, &click);
    match click {
        LinkClick::Opening(url) => MouseEffect::OpenLink(url),
        LinkClick::NoLink | LinkClick::RefusedScheme(_) | LinkClick::Unresolvable => {
            MouseEffect::None
        }
    }
}

/// What a left-click on the preview pane amounts to — the ONE outcome the whole click
/// path speaks in, from the hit-test through the status line to the spawn.
///
/// It has four states because the click really has four, and the two that used to
/// share an `Option::None` are the reason this type exists. [`NoLink`](Self::NoLink)
/// says the pointer was over ordinary text; [`Unresolvable`](Self::Unresolvable) says
/// the pointer was over a line too large to hit-test, so WHICH link it landed on — or
/// whether it landed on one at all — is unknown. Folded together they came out as the
/// same SILENCE, and the second one's silence is the exact failure this hit-test
/// exists to remove: a label that IS rendered and IS underlined, clicked, and nothing
/// happens and nothing is said. An abstention is never vacuous either — the probe
/// budget bounds the clicked line's SIZE times its candidate count, so a line with no
/// regions has a product of zero and is always within it
/// and abstaining IMPLIES a rendered link on that line (see [`view::LinkProbe`]).
///
/// Splitting a hit into [`Opening`](Self::Opening) and
/// [`RefusedScheme`](Self::RefusedScheme) is the same argument one step on: both hand
/// the opener nothing visible, so only a distinct message separates "your browser is
/// coming" from "snapback will not open this scheme".
///
/// The value is produced by the pure [`resolve_link_click`], turned into copy by
/// [`note_link_click`] and into an effect by [`click_effect`], and consumed by
/// exactly one impure statement in [`handle_mouse`] — PATTERNS §3's split, with the
/// type as the seam.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LinkClick {
    /// The click resolved to a real cell carrying no link.
    NoLink,
    /// The click resolved to this url and snapback will hand it to the opener.
    Opening(String),
    /// The click resolved to this url and snapback will NOT open its scheme.
    RefusedScheme(String),
    /// The click's line exceeded the hit-test's probe budget, so which link it hit is
    /// unknown — see the type doc for why that is not [`NoLink`](Self::NoLink).
    Unresolvable,
}

/// Decide what a left-click on the preview pane at screen `(col, row)` amounts to,
/// WITHOUT saying or opening anything.
///
/// The DECISION half of a link click ([`click_effect`] asks it on a click's
/// release; [`handle_mouse`] holds the one spawn), split out for the reason
/// PATTERNS §3 gives: everything here is terminal-, process- and status-free and can
/// be asserted directly against all four outcomes, while the caller is left holding
/// one spawn. That is what lets a test drive a real rendered transcript all the way to
/// "this is the url that would be opened" without a browser appearing on the machine
/// running it. It takes `&mut App` only because the width-scoped preview cache is
/// filled on demand; it writes nothing a reader can see.
///
/// The transcript rect comes from [`view::preview_transcript_rect`], which owns why
/// it is not simply the pane's inner rect.
///
/// The wrapped-layout context (the per-line wrapped-row prefix map, the rendered
/// lines and the link regions) comes from the SAME width-scoped cache the view drew
/// from — the very map the draw windowed itself by, so the click and the paint
/// resolve a screen row to a logical line through one shared answer rather than two
/// models. The hit itself comes from the pure [`view::link_at`], whose three answers
/// widen into four here by asking [`preview::has_openable_scheme`] of a hit — the same
/// predicate the autolink parser applies and [`resume::opener_argv`] enforces, asked
/// once, here, so the driver holds no gate of its own.
///
/// A pane with no previewed session at all is [`LinkClick::NoLink`]: there is no
/// transcript to have hit, which is a genuine absence rather than an abstention.
fn resolve_link_click(app: &mut App, col: u16, row: u16) -> LinkClick {
    let transcript = view::preview_transcript_rect(app);
    // Read before the borrow below: the hit context borrows `app` for as long as its
    // lines are in hand, and the offset is a plain `Copy` field.
    let scroll = app.preview_scroll;
    let Some((row_prefix, lines, regions, _folds)) = app.preview_hit_context(transcript.width)
    else {
        return LinkClick::NoLink;
    };
    match view::link_at(col, row, transcript, scroll, row_prefix, lines, regions) {
        view::LinkProbe::NoLink => LinkClick::NoLink,
        view::LinkProbe::Unresolvable => LinkClick::Unresolvable,
        view::LinkProbe::Hit(url) if preview::has_openable_scheme(url) => {
            LinkClick::Opening(url.to_string())
        }
        view::LinkProbe::Hit(url) => LinkClick::RefusedScheme(url.to_string()),
    }
}

/// Prefix of the status a link click reports itself with, so the message names the
/// url that is being handed to the opener rather than only that something happened.
///
/// It is what makes a MISSED hit distinguishable from a FAILED opener.
/// [`resume::open_url`] nulls every child stdio and swallows each error, so "nothing
/// opened" carries no information on its own: it is equally a click that resolved no
/// link and a browser that never launched. With this, the status line separates them —
/// a message with no browser is the opener's fault, no message at all means the click
/// never reached a link (or never reached the handler, the terminal having consumed
/// the press or its release itself).
const LINK_OPENING_PREFIX: &str = "opening ";

/// The two halves of the status a link click reports when it hit a link it will NOT
/// open, wrapped around the url so the reader sees which link and why.
///
/// snapback opens `http`/`https` only ([`preview::has_openable_scheme`] — the rule
/// the autolink parser already applies, and the gate [`resume::opener_argv`] enforces).
/// A `[label](url)` target, though, is authored by the transcript and is scheme-checked
/// nowhere on the way in, so a label CAN render underlined over a url the opener will
/// refuse. This message is what stops that from being SILENT. A rendered, clickable
/// affordance that quietly does nothing is exactly the indistinguishable silence this
/// hit-test exists to remove, and re-creating it here would trade one unreadable state
/// for another.
const LINK_REFUSED_PREFIX: &str = "not opening ";
/// The other half of the refusal (see [`LINK_REFUSED_PREFIX`]): it names the rule, so
/// the reader learns the link is unreachable from snapback rather than broken.
const LINK_REFUSED_SUFFIX: &str = " - only http/https links open";

/// The status a click reports when the hit-test ABSTAINED — the line it landed on was
/// too large to probe within `view`'s budget, so WHICH link it hit is unknown.
///
/// Worded to claim only what is actually known, which rules out both easier sentences.
/// It is NOT "no link here": abstaining implies at least one rendered link region on
/// that line, since a line with none costs nothing and is always within budget. It is
/// NOT "this link is broken" either: the url was never resolved, so nothing is known
/// about it — not even that the pointer was on it rather than on the prose beside it.
/// What IS known is that snapback could not tell, and why, and naming the line as the
/// cause is what stops the reader from blaming the link, the browser, or their aim.
///
/// It carries no url for the same reason: printing one would require having chosen a
/// region, which is precisely the work that was refused.
///
/// STICKY, like every refusal (PATTERNS §11) — and for the reason this whole branch
/// exists: a rendered, underlined affordance that does nothing must say so, and a
/// message that expired unread would leave the silence in place. Should a later reader
/// decide the over-budget case is better off SILENT, that is a one-line change to the
/// [`LinkClick::Unresolvable`] arm of [`note_link_click`] and nothing else — the
/// mapping lives in exactly one place so the decision stays reversible.
const LINK_UNRESOLVED: &str = "cannot tell which link that is - this line is too big to hit-test";

/// Give a resolved [`LinkClick`] its status-line treatment. The ONE place that mapping
/// lives, so each outcome's stickiness is decided once and is re-readable as a table.
/// Terminal- and process-free, so it is unit-testable across all four.
///
/// A click that RESOLVED anything is an ACTIONABLE input, not a notification: the user
/// aimed at something and either it is about to happen or it is refused, exactly as for
/// an actionable keypress. So it may clear whatever the status line held and state its
/// own outcome — which is also what keeps the diagnostic above SOUND. If a pending
/// sticky refusal could suppress the message, "no status" would stop implying "no hit"
/// and the status line would answer neither question.
///
/// [`LinkClick::NoLink`] writes NOTHING. It is a no-op — no scroll, no selection move,
/// no url — and a no-op must not wipe a refusal the reader has not read yet. That
/// asymmetry is the whole resolution of the tension between the two rules: the keypress
/// protocol says a transient message must not evict a sticky one, and it is honoured
/// here because a miss never sets one, while the other three are not mere transient
/// notifications at all.
///
/// The other three split by what the reader must do about them, per the status-line
/// ownership rule:
///
/// - [`Opening`](LinkClick::Opening) is a confirmation — something the user asked for
///   is under way — so it is TRANSIENT and expires after `STATUS_DWELL_TICKS` rather
///   than squatting on the keymap row. Whether the browser actually came up is
///   something only the user can see; `open_url` is fire-and-forget and has no answer
///   to report back.
/// - [`RefusedScheme`](LinkClick::RefusedScheme) is an outcome the reader may have to
///   act on (the link is unreachable from snapback, and only a message says so), so it
///   is STICKY like every other refusal and waits for the next actionable keypress.
/// - [`Unresolvable`](LinkClick::Unresolvable) is STICKY for the same reason and one
///   more: it is the ONLY signal that an underlined label the reader clicked did
///   nothing on purpose. See [`LINK_UNRESOLVED`] for the wording, and for why turning
///   this arm back into silence is deliberately a one-line change.
fn note_link_click(app: &mut App, click: &LinkClick) {
    match click {
        LinkClick::NoLink => {}
        LinkClick::Opening(url) => app.set_status_transient(format!("{LINK_OPENING_PREFIX}{url}")),
        LinkClick::RefusedScheme(url) => {
            app.set_status(format!("{LINK_REFUSED_PREFIX}{url}{LINK_REFUSED_SUFFIX}"));
        }
        LinkClick::Unresolvable => app.set_status(LINK_UNRESOLVED),
    }
}

/// The fold key of the fold node — a peer message or injected context — whose
/// HEADER sits under a pointer at screen `(col, row)`, or `None` when the pointer
/// is over no node header.
///
/// A deliberate mirror of [`resolve_link_click`], sharing every step that decides
/// WHICH LINE a click landed on: the same [`view::preview_transcript_rect`], the
/// same width-scoped [`App::preview_hit_context`] entry, and — inside
/// [`view::fold_at`] — the same `visual_to_content` row lookup its link sibling
/// runs through, so the two can never disagree about which line a cell belongs to.
/// They part on the COLUMN, which `view::content_hit` packs for this path and its
/// sibling resolves by re-rendering; `view::content_hit` owns why the cheaper answer
/// is sufficient for a region spanning its header's whole width. Only the region
/// list it matches against, and what a match yields, differ.
///
/// Terminal- and process-free: it answers a key and changes nothing. Every side
/// effect of acting on that key — the set mutation, the cache eviction, the
/// re-render, the scroll anchor and dropping any mouse selection — belongs to
/// [`App::toggle_peer_fold`], which [`click_effect`] calls on a click's release.
fn fold_under_pointer(app: &mut App, col: u16, row: u16) -> Option<String> {
    let transcript = view::preview_transcript_rect(app);
    // Read before the borrow below, which holds `app` for as long as the regions are
    // in hand; the offset is a plain `Copy` field.
    let scroll = app.preview_scroll;
    let (row_prefix, _lines, _links, folds) = app.preview_hit_context(transcript.width)?;
    view::fold_at(col, row, transcript, scroll, row_prefix, folds).map(str::to_string)
}

/// Apply a decoded [`Action`] to the app.
fn apply_action(app: &mut App, action: Action) -> Outcome {
    match action {
        Action::Quit => Outcome::Quit,
        Action::MoveUp => {
            app.move_selection(-1);
            Outcome::Continue
        }
        Action::MoveDown => {
            app.move_selection(1);
            Outcome::Continue
        }
        // A caret move — a character or a word — is not a query change, so it never
        // reaches the query funnel (`App::apply_query_change`): no re-filter, and
        // the preview stays put.
        Action::CaretBack => {
            app.move_query_caret(false);
            Outcome::Continue
        }
        Action::CaretForward => {
            app.move_query_caret(true);
            Outcome::Continue
        }
        Action::CaretWordBack => {
            app.move_query_caret_by_word(false);
            Outcome::Continue
        }
        Action::CaretWordForward => {
            app.move_query_caret_by_word(true);
            Outcome::Continue
        }
        Action::Resume { fork } => {
            // Smart Enter: `claude -r` REFUSES to plain-resume a LIVE session, so
            // Enter (not Ctrl-F) on a running row opens the Attach/Fork/Cancel
            // choice instead. Ctrl-F fork stays a direct hand-off for ANY session.
            //
            // The gate asks CLAUDE, one-shot, right here — it must NOT read the
            // polled `--all` map. That map is up to ~5.26s stale while the board
            // is active (a ~0.26s shell-out then a 5s sleep), and unboundedly
            // stale while idle past AGENTS_IDLE_AFTER, and its `done` qualifier
            // means "the agent reported completion", NOT "claude will permit `-r`".
            // Deciding from it is a TOCTOU race we lose: claude re-evaluates
            // liveness at spawn time and refuses, and the user hit exactly that
            // on a `● bg done` row. Probing here shrinks the window to ~0.26s and,
            // more importantly, replaces an inference about claude's gate with
            // claude's own answer.
            //
            // On AGENTS.md's "OFF-UI-THREAD blocking work": that rule exists so the
            // 5s POLL never blocks rendering, and the poll is untouched — still one
            // call per cycle, still on its own thread. This is a ONE-SHOT at
            // hand-off, directly analogous to `resume`'s authoritative re-read of
            // `cwd`/`sessionId` at the same moment. Be precise about the cost,
            // though, because it is NOT free on both branches:
            //
            // * Plain resume (the common case): nothing renders between this probe
            //   and the terminal teardown, so the ~0.26s is invisible.
            // * Overlay: the overlay itself draws ~0.26s after Enter — a small,
            //   deliberate hitch, accepted because the alternative is handing the
            //   user claude's refusal instead of the Attach/Fork choice.
            if !fork {
                // Clone the id so the `&Session` borrow ends before the probe and
                // `open_live_choice` touch `app`.
                if let Some(id) = app.selected_session().map(|s| s.session_id.clone()) {
                    if app.is_live_now(&id) {
                        app.open_live_choice(id);
                        return Outcome::Continue;
                    }
                }
            }
            // Non-live (or Ctrl-F): run the refusal gate while the terminal is
            // still up — a deleted worktree / unreadable file becomes a transient
            // board status rather than a teardown/re-init flash. Only a confirmed
            // `Ready` plan escalates to `Outcome::Resume`. The `map` drops the
            // `&Session` borrow before we mutably touch `app` for `set_status`.
            // No model is threaded in: `resume::check` takes none, so a resume and
            // a fork keep the session's own model, which claude normally restores.
            let checked = app.selected_session().map(|s| resume::check(s, fork));
            match checked {
                Some(Ok(ready)) => Outcome::Resume(ready),
                Some(Err(err)) => {
                    app.set_status(err.message().to_string());
                    Outcome::Continue
                }
                None => Outcome::Continue,
            }
        }
        Action::NewSession => new_session(app),
        Action::Reply => reply(app),
        Action::Interrupt => interrupt(app),
        Action::ToggleSearchMode => {
            app.toggle_search_mode();
            Outcome::Continue
        }
        Action::ToggleScope => {
            app.toggle_scope();
            Outcome::Continue
        }
        Action::LayoutTowardPreview => {
            app.step_pane_layout(false);
            Outcome::Continue
        }
        Action::LayoutTowardList => {
            app.step_pane_layout(true);
            Outcome::Continue
        }
        Action::PreviewPageUp => {
            app.preview_page_up();
            Outcome::Continue
        }
        Action::PreviewPageDown => {
            app.preview_page_down();
            Outcome::Continue
        }
        Action::PreviewHalfUp => {
            app.preview_half_up();
            Outcome::Continue
        }
        Action::PreviewHalfDown => {
            app.preview_half_down();
            Outcome::Continue
        }
        Action::PreviewTop => {
            app.preview_top();
            Outcome::Continue
        }
        Action::PreviewBottom => {
            app.preview_bottom();
            Outcome::Continue
        }
        Action::PreviewMatchNext => {
            app.preview_match_step(true);
            Outcome::Continue
        }
        Action::PreviewMatchPrev => {
            app.preview_match_step(false);
            Outcome::Continue
        }
        Action::Insert(c) => {
            app.push_query_char(c);
            Outcome::Continue
        }
        Action::Backspace => {
            app.pop_query_char();
            Outcome::Continue
        }
        Action::BackspaceWord => {
            app.pop_query_word();
            Outcome::Continue
        }
        Action::ClearQuery => {
            app.clear_query();
            Outcome::Continue
        }
        Action::Chord => {
            // Arm the leader chord; `handle_event` routes the next key through
            // `handle_chord_key` before it can reach the board or the query.
            app.pending_chord = true;
            Outcome::Continue
        }
        Action::Ignore => Outcome::Continue,
    }
}

/// Status-line prefix for a `Ctrl-X y` copy a clipboard TOOL confirmed (it exited 0
/// with the id on its stdin), kept as a named `const` so the copy message has ONE
/// source of truth (NO MAGIC VALUES) that [`copy_status`] and any future doc/test
/// reference share. A trailing space separates it from the id. Only a tool's exit
/// code earns "Copied"; the OSC 52 path says "Sent" ([`OSC52_SENT_STATUS_PREFIX`]).
const COPY_STATUS_PREFIX: &str = "Copied session ID ";

/// Status-line opener for a `Ctrl-X y` copy that went out as an OSC 52 escape (over
/// SSH, with no clipboard tool for this OS/display, or after every tool failed).
///
/// It says "Sent", NEVER "Copied": snapback cannot know whether the terminal
/// honoured the escape — some drop it silently, and the write-only discipline
/// forbids asking. The full id follows it directly, so the id always lands in the
/// first columns of the help row.
const OSC52_SENT_STATUS_PREFIX: &str = "Sent session ID ";

/// The caveat after the id on the OSC 52 path: where the id went, and that it may
/// not have arrived. It comes AFTER the id on purpose — an 80-column help row
/// truncates this caveat, never the id, which is the text the user selects by hand
/// when the terminal ignores the escape.
const OSC52_SENT_STATUS_CAVEAT: &str = " to the terminal via OSC 52 (some terminals ignore it)";

/// Status shown when `Ctrl-X y` is pressed with nothing selected (empty or
/// fully-filtered list). A named `const` so the no-op branch and its test agree
/// on one string.
const NO_SELECTION_STATUS: &str = "No session selected";

/// The board status for an `id` a clipboard tool COPIED. Pure so it is
/// unit-testable and the single source of the copy message.
///
/// A `session_id` is a UUID with no interior whitespace, so it survives
/// `view::render_help`'s whitespace flattening intact and reads back verbatim on
/// the help line.
fn copy_status(id: &str) -> String {
    format!("{COPY_STATUS_PREFIX}{id}")
}

/// The board status for an `id` SENT to the terminal as an OSC 52 escape: the full
/// id first, then the caveat. Pure so the wording is assertable without a
/// terminal, and never "Copied" (see [`OSC52_SENT_STATUS_PREFIX`]).
pub(super) fn osc52_sent_status(id: &str) -> String {
    format!("{OSC52_SENT_STATUS_PREFIX}{id}{OSC52_SENT_STATUS_CAVEAT}")
}

/// Status-line opener for a preview drag-SELECTION a clipboard TOOL confirmed (it
/// exited 0 with the text on its stdin). Names a selection, never a session id, so
/// the two copies the board makes never read alike; and like
/// [`COPY_STATUS_PREFIX`], only a tool's exit code earns "Copied".
const SELECTION_COPY_STATUS_PREFIX: &str = "Copied selection ";

/// Status-line opener for a drag-selection that went out as an OSC 52 escape. It
/// says "Sent", NEVER "Copied", for the reason [`OSC52_SENT_STATUS_PREFIX`] does,
/// and it carries the same [`OSC52_SENT_STATUS_CAVEAT`] after the size.
const SELECTION_SENT_STATUS_PREFIX: &str = "Sent selection ";

/// How big a selection was, as the status line states it: `(1 line)` or
/// `(N lines)`, counting the screen rows the drag covered. The selected TEXT itself
/// is never echoed — a multi-row selection cannot fit the one help row, and the
/// highlight in the pane already shows what it was. Pure.
fn selection_size(text: &str) -> String {
    let rows = text.split('\n').count();
    let noun = if rows == 1 { "line" } else { "lines" };
    format!("({rows} {noun})")
}

/// The HONEST status for a finished copy of `payload`: `copied` is whether a
/// clipboard tool took the text (exit 0). Four lines, one per kind and route:
///
/// - a session id a tool copied → [`copy_status`] (`Copied session ID <uuid>`);
/// - a session id sent as OSC 52 → [`osc52_sent_status`] (`Sent session ID …`);
/// - a selection a tool copied → `Copied selection (N lines)`;
/// - a selection sent as OSC 52 → `Sent selection (N lines) to the terminal via
///   OSC 52 (some terminals ignore it)`.
///
/// "Copied" appears ONLY when a tool exited 0; the kind is always named. Pure, so
/// every row is assertable without a terminal or a tool.
pub(super) fn copy_result_status(payload: &CopyPayload, copied: bool) -> String {
    match (payload, copied) {
        (CopyPayload::SessionId(id), true) => copy_status(id),
        (CopyPayload::SessionId(id), false) => osc52_sent_status(id),
        (CopyPayload::Selection(text), true) => {
            format!("{SELECTION_COPY_STATUS_PREFIX}{}", selection_size(text))
        }
        (CopyPayload::Selection(text), false) => format!(
            "{SELECTION_SENT_STATUS_PREFIX}{}{OSC52_SENT_STATUS_CAVEAT}",
            selection_size(text)
        ),
    }
}

/// Whether a finished copy's status line is STICKY (until the next key) or a
/// transient confirmation — the decision PATTERNS §11 (status-line ownership)
/// records, made pure here so it is pinned by a test rather than by a comment.
///
/// A `Ctrl-X y` session id: STICKY, on both routes. The line reports which route
/// ran, and on the OSC 52 path the full id on it is what the user selects by hand
/// (Shift/Option-drag past mouse capture) when the terminal ignores the escape — a
/// `STATUS_DWELL_TICKS` x `watch::TICK` = 4 s dwell is too short for that.
///
/// A drag SELECTION: TRANSIENT, on both routes. The hand-selection reason does not
/// carry over: the line holds only a row count, never the text, and the text the
/// user would re-select natively is still on screen, still highlighted, in the
/// pane. Nor does the line carry a failure or a refusal. The route it names is read
/// the moment the button comes up, while the user is looking, so the dwell is
/// enough — and a sticky line would park on the keymap row after EVERY drag until a
/// key was pressed, which a mouse-only reader may never do.
fn copy_status_is_sticky(payload: &CopyPayload) -> bool {
    matches!(payload, CopyPayload::SessionId(_))
}

/// Decide the `Ctrl-X y` completion for the current selection — the copy's PURE
/// decision half, reached from [`handle_chord_key`]. It performs NO I/O at all.
///
/// With a selection it returns [`Outcome::Copy`] carrying the selected session's
/// FULL `session_id`, owned through the stable-id accessor [`App::selected_session`]
/// (STABLE-ID STATE). The driver performs the copy, and the status comes from its
/// RESULT ([`finish_copy`]), so nothing claims "Copied" before a tool has. It sets
/// no status of its own: [`handle_chord_key`] has already cleared the line.
///
/// No selection is a graceful no-op: no copy is requested (never an empty payload)
/// and the sticky [`NO_SELECTION_STATUS`] is set; it never panics.
fn copy_selected_id(app: &mut App) -> Outcome {
    // Own the id so the `&Session` borrow ends before `app` is mutably re-borrowed
    // below (the clone-then-mutate discipline the resume path uses).
    match app.selected_session().map(|s| s.session_id.clone()) {
        Some(id) => Outcome::Copy(CopyPayload::SessionId(id)),
        None => {
            app.set_status(NO_SELECTION_STATUS);
            Outcome::Continue
        }
    }
}

/// Complete a copy — a `Ctrl-X y` session id or a preview drag-selection — with
/// its HONEST status. This is the one function that performs the OSC 52 write, and
/// the writer is INJECTED (`w`), so a test hands it a `Vec<u8>` and no escape ever
/// reaches the test run's terminal.
///
/// `copied` is a RESULT, never a hope: `true` only when a clipboard tool exited 0
/// with the text on its stdin (see [`AppEvent::CopyFinished`]). Then NO escape is
/// written, because the text is already on the clipboard, and the status says
/// "Copied". Otherwise — SSH, no tool for this OS/display, or every tool missing
/// or failing — the text goes out as a write-only OSC 52 escape
/// ([`clipboard::copy_to_clipboard`]) and the status says it was SENT. The wording
/// is [`copy_result_status`]'s, and whether it sticks is
/// [`copy_status_is_sticky`]'s.
///
/// The driver (`tui::run_inner`) calls this on the UI thread with the terminal's own
/// writer, never from the tool worker. Frame-safety: that is the EVENT-HANDLING
/// phase, BETWEEN ratatui draws, and `copy_to_clipboard` flushes. Being write-only,
/// the escape moves no cursor and mutates no cells, so the next `terminal.draw`
/// diffs against an unchanged screen (same reasoning as `tui::mod`'s `hard_reset`).
/// A write from the worker thread could instead interleave with a frame the UI
/// thread is flushing.
pub fn finish_copy<W: Write>(app: &mut App, w: &mut W, payload: &CopyPayload, copied: bool) {
    if !copied {
        // Best-effort: the status below reports the route whatever happens, and a
        // stdout that cannot take a few bytes cannot draw the board either, so the
        // `io::Result` is deliberately discarded (like `resume::open_url`'s spawn).
        let _ = clipboard::copy_to_clipboard(w, payload.text());
    }
    let status = copy_result_status(payload, copied);
    // A `Ctrl-X y` id's line is STICKY (`set_status`) on purpose — a deliberate
    // exception to PATTERNS §11's "confirmations expire"; a selection's is not.
    // `copy_status_is_sticky` owns both reasons. A sticky line still clears on the
    // next actionable keypress, like any sticky status.
    if copy_status_is_sticky(payload) {
        app.set_status(status);
    } else {
        app.set_status_transient(status);
    }
}

/// The six keys a pending `Ctrl-X` chord binds, plus cancel — the PURE decision
/// half of the leader chord (PATTERNS §10, keys -> actions -> outcomes). The impure
/// completion (hide / open confirm / toggle / rescan / copy session ID / fold
/// toggle) lives in [`handle_chord_key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChordOutcome {
    /// `x` — toggle the selected session's hidden state (soft delete / un-hide).
    Hide,
    /// `d` — open the hard-delete confirm modal for the selected session.
    Delete,
    /// `h` — toggle whether user-hidden sessions are revealed inline.
    ShowHidden,
    /// `r` — drop every cached parse and re-read the whole store.
    Rescan,
    /// `y` — copy session ID: hand the driver the selected session's FULL
    /// `session_id` to copy ([`Outcome::Copy`] — an OS clipboard tool first, OSC 52
    /// only as the fallback), and the status line then says what actually happened.
    Copy,
    /// `f` — fold the selected row's fork lineage if it is open, open it if the
    /// row is a folded `(+N)` head, otherwise nothing
    /// ([`App::toggle_selected_lineage`]).
    Fold,
    /// `Esc` / `Ctrl-C` / any unbound key — abandon the chord with no side effect.
    Cancel,
}

/// Resolve the key that FOLLOWS `Ctrl-X` into a [`ChordOutcome`]. Pure so the
/// leader chord's decision is unit-testable without a terminal.
///
/// Any Ctrl-modified key cancels (so `Ctrl-C` still reads as a quit-shaped abort
/// mid-leader and no completion is bound to a Ctrl combo), and any unbound key
/// cancels too, so a mistyped follow-up abandons the chord rather than doing
/// something surprising. Each binding accepts its shifted form so a held Shift on
/// the follow-up still completes the chord.
fn chord_key(key: KeyEvent) -> ChordOutcome {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return ChordOutcome::Cancel;
    }
    match key.code {
        KeyCode::Char('x') | KeyCode::Char('X') => ChordOutcome::Hide,
        KeyCode::Char('d') | KeyCode::Char('D') => ChordOutcome::Delete,
        KeyCode::Char('h') | KeyCode::Char('H') => ChordOutcome::ShowHidden,
        KeyCode::Char('r') | KeyCode::Char('R') => ChordOutcome::Rescan,
        KeyCode::Char('y') | KeyCode::Char('Y') => ChordOutcome::Copy,
        KeyCode::Char('f') | KeyCode::Char('F') => ChordOutcome::Fold,
        _ => ChordOutcome::Cancel,
    }
}

/// Apply the key that FOLLOWS a pending `Ctrl-X` chord, then LEAVE the chord — a
/// leader chord resolves on exactly one key, hit or miss.
///
/// `x` hides / un-hides the selected session (persisting the change), `d` opens the
/// hard-delete confirm (it does NOT delete here — the confirm handler does), `h`
/// toggles the show-hidden view, `r` forces a full re-read of the store, `y` requests
/// a clipboard copy of the selected session's full id ([`copy_selected_id`], which
/// hands the driver an [`Outcome::Copy`]), `f` folds or expands the selected row's
/// fork lineage, and anything else (`Esc` / `Ctrl-C` / an unbound key) abandons the
/// chord with no side effect. The pending state is cleared FIRST so an early return
/// can never wedge the board in the chord. Routed BEFORE `key_to_action` in
/// [`handle_event`], so a printable completion never leaks into the query.
///
/// `r` is the store cache's ESCAPE HATCH, and it is a user-reachable key rather
/// than an internal call for exactly that reason: reloads reuse the parse of every
/// file whose `(mtime, len)` did not move, so a filesystem that lies about either
/// (a coarse-granularity network volume, a badly skewed clock) could in principle
/// leave a row stale with nothing on the board to say so. One keypress rules that
/// out. It reports the count it landed on, so pressing it is never a no-op on
/// screen — a keypress OUTCOME, hence the status line (PATTERNS §11).
fn handle_chord_key(app: &mut App, key: KeyEvent, store: &mut SessionStore) -> Outcome {
    app.pending_chord = false;
    // The follow-up is an actionable keypress, so clear any transient status first
    // (a hide may then set its own persist-error status).
    app.clear_status();
    match chord_key(key) {
        ChordOutcome::Hide => app.toggle_hidden_selected(),
        ChordOutcome::ShowHidden => app.toggle_show_hidden(),
        ChordOutcome::Delete => app.open_delete_confirm(),
        ChordOutcome::Rescan => {
            store.invalidate();
            reload_board(app, store);
            app.set_status_transient(rescan_status(app.sessions.len()));
        }
        // The one verb whose side effect belongs to the DRIVER: the clipboard copy
        // runs a tool on its own thread or writes an OSC 52 escape, so the request
        // leaves here as data (`Outcome::Copy`) rather than as `Continue`.
        ChordOutcome::Copy => return copy_selected_id(app),
        ChordOutcome::Fold => app.toggle_selected_lineage(),
        ChordOutcome::Cancel => {}
    }
    Outcome::Continue
}

/// The `Ctrl-X r` outcome line. Pure so the wording is assertable without a store.
fn rescan_status(sessions: usize) -> String {
    format!("reloaded {sessions} session(s) from disk")
}

/// A decoded intent while a modal overlay owns the keyboard. Collapses the old
/// `LiveNav` + `AgentNav`, which were variant-identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModalNav {
    /// Move the highlight forward (`→`/`↓`/`Tab`/`l`/`j`; horizontal keys Row-only).
    Next,
    /// Move the highlight backward (`←`/`↑`/`h`/`k`; horizontal keys Row-only).
    Prev,
    /// Adjust the highlighted row's value one step up (`→`, `forward`) or down
    /// (`←`) — the `List` layout's meaning for the horizontal arrows. Bound on
    /// every `List` modal (see [`modal_key`]) and narrowed by
    /// [`App::adjust_modal_effort`] to a model-picker model row, where it steps
    /// that row's `--effort`; on every other row it changes nothing.
    Adjust {
        /// `true` for `→` (up), `false` for `←` (down).
        forward: bool,
    },
    /// Act on the highlighted choice (`Enter`).
    Confirm,
    /// Start the highlighted choice INTERACTIVELY, skipping the draft (`Ctrl-O`) —
    /// the new-session picker's second verb, alongside `Enter`'s background draft.
    /// Bound on the `List` layout only (see [`modal_key`]) and further narrowed to
    /// [`ModalAction::New`] rows by [`launch_pick_interactively`].
    Interactive,
    /// Dismiss the modal (`Esc`/`Ctrl-C`).
    Cancel,
    /// A key with no binding in the modal.
    Ignore,
}

/// Map a keypress to a [`ModalNav`] while a modal is open.
///
/// The vertical keys (`↑`/`↓`, plus `k`/`j` and `Tab` forward) move the highlight in
/// BOTH layouts. The horizontal keys are DERIVED from `layout`, and mean a
/// different thing in each — the two overlays' key maps must not be unioned by
/// accident:
///
/// * a `Row` (button strip) binds `←`/`→`/`h`/`l` to MOVE the highlight, since its
///   choices sit side by side;
/// * a `List` (vertical picker) never moves its highlight sideways. It binds `←`/`→`
///   to [`ModalNav::Adjust`] — step the highlighted row's value — which only the
///   model picker's model rows act on (their `--effort`). `h`/`l` stay UNBOUND
///   there: a list moves on its vertical keys alone, and a letter adjusting a value
///   would be a second, undiscoverable spelling of a key pair the picker already
///   names in its prompt and footer.
///
/// `Left`/`Right` are ALSO bound on the BOARD (`CaretBack`/`CaretForward`, and
/// with `Alt`/`Ctrl` the word hops `CaretWordBack`/`CaretWordForward`);
/// `handle_event`'s modal gate keeps those dispatch contexts apart, so the picker's
/// arrows can never move the search caret underneath it.
///
/// `Ctrl-O` is derived from `layout` for the same reason: only the `List` picker
/// has an interactive start to offer, so it must stay INERT on the running-session
/// Attach/Fork strip and the delete confirm rather than becoming a modal-wide key.
/// The action-level narrowing lives in [`launch_pick_interactively`] — the layout is
/// the key map's business, the choice's meaning is the handler's. `Adjust` follows
/// the same split: the layout binds the arrows, [`App::adjust_modal_effort`] decides
/// which rows they do anything on, so the agent picker keeps ignoring them.
fn modal_key(key: KeyEvent, layout: ModalLayout) -> ModalNav {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') | KeyCode::Char('C') => ModalNav::Cancel,
            KeyCode::Char('o') | KeyCode::Char('O') if matches!(layout, ModalLayout::List) => {
                ModalNav::Interactive
            }
            _ => ModalNav::Ignore,
        };
    }
    let horizontal = matches!(layout, ModalLayout::Row);
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => ModalNav::Prev,
        KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => ModalNav::Next,
        KeyCode::Left | KeyCode::Char('h') if horizontal => ModalNav::Prev,
        KeyCode::Right | KeyCode::Char('l') if horizontal => ModalNav::Next,
        KeyCode::Left if !horizontal => ModalNav::Adjust { forward: false },
        KeyCode::Right if !horizontal => ModalNav::Adjust { forward: true },
        KeyCode::Enter => ModalNav::Confirm,
        KeyCode::Esc => ModalNav::Cancel,
        _ => ModalNav::Ignore,
    }
}

/// Apply a modal keypress: navigation stays on the board; Confirm routes the
/// highlighted choice's action; Esc/Ctrl-C dismiss. The layout (hence the key map)
/// is read off the open modal.
fn handle_modal_key(app: &mut App, key: KeyEvent, store: &mut SessionStore) -> Outcome {
    let Some(layout) = app.modal.as_ref().map(|m| m.layout) else {
        return Outcome::Continue;
    };
    match modal_key(key, layout) {
        ModalNav::Next => {
            app.modal_next();
            Outcome::Continue
        }
        ModalNav::Prev => {
            app.modal_prev();
            Outcome::Continue
        }
        ModalNav::Adjust { forward } => {
            app.adjust_modal_effort(forward);
            Outcome::Continue
        }
        ModalNav::Cancel => {
            app.close_modal();
            Outcome::Continue
        }
        ModalNav::Confirm => confirm_modal(app, store),
        ModalNav::Interactive => launch_pick_interactively(app),
        ModalNav::Ignore => Outcome::Continue,
    }
}

/// Start the picker's highlighted agent INTERACTIVELY (`Ctrl-O`), skipping the
/// draft pane `Enter` opens.
///
/// The second verb on the new-session picker, beside `Enter`'s background draft.
/// It reads the SAME [`ModalAction::New`] payload the confirm handler does (so the
/// agent name still rides the choice, needing no index-to-agent lookup), closes the
/// picker, and runs the ordinary new-session gate — the identical hand-off `Enter`
/// used to perform, moved onto its own key.
///
/// BOTH verbs stay ONE key at the picker, which is what makes the swap safe: the
/// background draft became the default without charging the interactive start a
/// keystroke for it. `Ctrl-O` names the same thing here as it does inside the draft
/// pane ([`compose`]'s `Ctrl-O`) — "open interactive claude" — so the key reads
/// consistently on both surfaces.
///
/// The `ModalAction` match is the second of two gates: [`modal_key`] already
/// restricts the key to the `List` layout, and this restricts it to a choice that
/// actually names a new session. Any other action — Attach, Fork, Delete,
/// DeleteLineage, SetModel, Cancel, or an out-of-range highlight — is a NO-OP, so a
/// `List`-layout modal cannot inherit an interactive start it has no meaning for.
/// The model picker is exactly such a modal, and it relies on this: `Ctrl-O` there
/// must not launch anything.
///
/// The pick is recorded as the last-chosen agent FIRST — BEFORE the gate, so the
/// next `Ctrl-N` repeats it even across a refusal. This is one of the THREE points
/// a new session is actually started (the others are the draft pane's `Enter` and
/// its own `Ctrl-O`), and only those record: merely OPENING a draft must not
/// rewrite that memory, or a cancelled draft would.
///
/// It sends NO model: skipping the draft skips the compose, and a model is only
/// ever picked inside a compose (`Ctrl-L`), so there is no pick to carry — the new
/// session starts on the model the user's `claude` settings name.
fn launch_pick_interactively(app: &mut App) -> Outcome {
    let Some(ModalAction::New(agent)) = app
        .modal
        .as_ref()
        .and_then(super::app::Modal::selected_action)
        .cloned()
    else {
        return Outcome::Continue;
    };
    app.close_modal();
    app.set_last_new_agent(agent.clone());
    launch_new_session(app, agent.as_deref(), None, None)
}

/// Which teardown-safe hand-off a confirmed overlay choice runs.
enum Handoff {
    /// `claude attach <job-id>` — reattach to the running agent in this
    /// terminal, keyed on its short agent-view id (resolved in [`route_handoff`]).
    Attach,
    /// `claude -r <id> --fork-session` — branch off a copy.
    Fork,
}

/// Resolve the highlighted modal choice into a driver [`Outcome`] — the ONE
/// confirm handler behind every modal (it absorbed the old `confirm_live_choice`
/// and `confirm_agent_pick`).
///
/// The modal closes on any confirm. `Cancel` (or an out-of-range highlight) just
/// returns to the board. `Attach`/`Fork` run the terminal-up refusal gate against
/// the modal's target `session_id` and, on success, escalate to [`Outcome::Resume`]
/// so the driver spawns them through the IDENTICAL teardown→spawn→wait→return round
/// trip as a plain resume; a refusal (deleted worktree / unreadable file / no live
/// agent) sets a board status. `New` DRAFTS: it hands the keyboard to the compose
/// zone for the new session's first message, which then chooses `--bg` (`Enter`)
/// or interactive ([`compose`]'s `Ctrl-O`). The picker's own `Ctrl-O`
/// ([`launch_pick_interactively`]) is the one-key bypass to an interactive start.
///
/// `New` records NOTHING here. The memory behind `Ctrl-N`'s pre-highlight is "the
/// agent of the last new session actually STARTED", and a draft can still be
/// cancelled, so it is written at the three real launch points instead.
///
/// The `clone` releases the `app.modal` borrow before `close_modal` /
/// `set_status` / `compose::open_background` / the gates re-borrow `app`,
/// preserving the borrow discipline both old confirm handlers relied on.
fn confirm_modal(app: &mut App, store: &mut SessionStore) -> Outcome {
    let Some(modal) = app.modal.clone() else {
        return Outcome::Continue;
    };
    let action = modal.selected_action().cloned();
    app.close_modal();
    match action {
        None | Some(ModalAction::Cancel) => Outcome::Continue,
        Some(ModalAction::Attach) => match modal.session_id.as_deref() {
            Some(id) => route_handoff(app, id, Handoff::Attach),
            None => Outcome::Continue,
        },
        Some(ModalAction::Fork) => match modal.session_id.as_deref() {
            Some(id) => route_handoff(app, id, Handoff::Fork),
            None => Outcome::Continue,
        },
        Some(ModalAction::Delete) => match modal.session_id.clone() {
            Some(id) => confirm_delete(app, &[id], store),
            None => Outcome::Continue,
        },
        // The lineage's member ids were resolved when the choice was BUILT (see
        // `ModalAction::DeleteLineage`), so nothing is re-derived from a selection
        // a reload may have moved while the modal sat open.
        Some(ModalAction::DeleteLineage(ids)) => confirm_delete(app, &ids, store),
        Some(ModalAction::New(agent)) => {
            // `Enter` opens the DRAFT — the same pane the no-agent fast path opens.
            // Nothing is launched and nothing is recorded yet; the draft's own
            // `Enter` (`--bg`) or `Ctrl-O` (interactive) decides both.
            compose::open_background(app, agent);
            Outcome::Continue
        }
        // The confirm that hands off NOTHING: it writes the row's model and
        // whatever effort `←`/`→` left on it into the compose the picker was opened
        // over (`Ctrl-L`), in one write, and the modal's close returns the keyboard
        // to that compose — text untouched. No status is set: the pick is true over
        // the compose's lifetime rather than at a keypress, so the compose box's
        // `model:` label owns saying it (AGENTS.md STATUS-LINE OWNERSHIP).
        Some(ModalAction::SetModel(pick)) => {
            app.set_compose_model(pick);
            Outcome::Continue
        }
    }
}

/// Execute a confirmed HARD delete of `ids` — one selected session, or every
/// member of its fork lineage — then refresh the board.
///
/// **ONE probe for the whole set, and that is a hard requirement.** The pure
/// [`delete::can_delete_target`] writer guard needs each target's freshly-probed
/// record, so this takes claude's WHOLE active list once ([`App::live_agents_now`])
/// and judges every member against that single map. Asking through the per-session
/// accessor instead would spawn `claude` once per member — N blocking shell-outs
/// on the UI thread, precisely what AGENTS.md's OFF-UI-THREAD rule forbids — and
/// would judge the family against N different instants.
///
/// On PATTERNS.md §6, stated per branch rather than assumed: this is a ONE-SHOT
/// at a hand-off-shaped moment (an irreversible confirm), exactly like the Enter
/// gate and [`route_handoff`]. It adds no tick, no thread and no event source,
/// and leaves the `--all` poll untouched at one call per cycle. Unlike a resume
/// there is no teardown to hide behind: the board REDRAWS after a delete, so the
/// ~0.26s lands as a visible hitch between Enter and the refreshed list on EVERY
/// branch here. That is deliberate — an irreversible unlink must be decided on
/// claude's current answer, not on a snapshot up to ~5.3s old (unboundedly old
/// while idle past `AGENTS_IDLE_AFTER`).
///
/// Each member is guarded INDIVIDUALLY and the pass is partial by design: a
/// refused member is skipped and the rest still go, because all-or-nothing would
/// let one busy fork block a whole lineage. Removal errors are counted apart from
/// refusals (never folded together — see [`delete::status_for_delete`]), and one
/// failure never aborts the remaining members. Each session is CLONED before
/// removal, ending the `&Session` borrow before the mutable reload re-borrows
/// `app`.
///
/// EVERY id is accounted for, including one whose row is no longer on the board:
/// `ids` was captured when the modal opened and a `SessionsChanged` reload can
/// drop a member while it sits there. Such a target is neither removed nor
/// refused, so the loop has nothing to record — [`delete::status_for_delete`]
/// reconciles it out of `ids.len()` instead, which is what stops a 3-member
/// lineage from reporting `2 deleted` with the third silently unmentioned.
///
/// The board reloads ONCE after the loop, and only when something was actually
/// removed, through the SAME [`reload_board`] seam the autorefresh reload uses —
/// so the removed rows leave the board and the selection clamps to a survivor by
/// stable id. A deleted file is simply no longer discovered, so it takes its
/// cached parse with it: the store cache can never resurrect a removed session.
fn confirm_delete(app: &mut App, ids: &[String], store: &mut SessionStore) -> Outcome {
    // ONE shell-out for the whole target set (see the note above).
    let live = app.live_agents_now();
    let mut removed = 0usize;
    let mut refusals: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for id in ids {
        // BOTH writers, not just claude's: a quick reply snapback still has in
        // flight deregisters the job from claude's active list on its way in, so
        // the probe above cannot see it (see `delete::can_delete_target`).
        let reply_in_flight = app.sending_to(id).is_some();
        if let Err(refusal) = delete::can_delete_target(live.get(id), reply_in_flight) {
            refusals.push(refusal);
            continue;
        }
        // A member that is no longer on the board (a reload dropped it while the
        // confirm sat open) is neither a refusal nor an FS failure, so there is
        // nothing to push here — but it IS still one of `ids`, and the status
        // reconciles it back out of that count rather than losing it.
        let Some(session) = app.session_by_id(id).cloned() else {
            continue;
        };
        match delete::remove(&session) {
            Ok(()) => removed += 1,
            Err(err) => errors.push(err.to_string()),
        }
    }

    if removed > 0 {
        reload_board(app, store);
    }
    if let Some(status) = delete::status_for_delete(ids.len(), removed, &refusals, &errors) {
        app.set_status(status);
    }
    Outcome::Continue
}

/// Run the refusal gate for a chosen hand-off and escalate a confirmed plan to
/// [`Outcome::Resume`]; a refusal sets a board status. The `map` drops the
/// `&Session` borrow before `set_status` mutably touches `app`.
///
/// **Attach re-asks claude here, at the hand-off.** Its target is the agent-view
/// job `id` (the SHORT id from `claude agents --json`) taken from
/// [`App::live_agent_now`]'s fresh record — NEVER from the polled `--all` map.
/// That map is the same ~5.3s-stale (unboundedly stale while idle past
/// `AGENTS_IDLE_AFTER`) snapshot the resume gate was moved off, and reading an
/// attach id from it is the identical bug one layer down: an authoritative
/// decision made from stale data. Here it is worse than at the gate, because
/// the overlay can sit open INDEFINITELY while the user decides — even the
/// probe that opened it is stale by the time Attach is chosen, so the window
/// is unbounded rather than ~5.3s. The rule is uniform: every hand-off
/// re-asks, nothing hands off on polled data.
///
/// Three answers, kept distinct because they have distinct causes:
///
/// * **Live, with a job id** — attach to it.
/// * **Live, no job id** (interactive) — [`resume::ATTACH_NO_JOB_ID`], via
///   [`resume::check_attach`]'s own pure gate.
/// * **Not in the active list** — [`resume::ATTACH_NOT_LIVE`]. It finished while
///   the overlay was open, or the probe failed; either way there is no
///   authoritative id, so we must NOT spawn `claude attach` with a dead one.
///
/// Fork deliberately does NOT probe: a fork of a live session is expected to
/// work, so it has no liveness question to ask and stays valid even when Attach
/// has just been refused — which is exactly the route [`resume::ATTACH_NOT_LIVE`]
/// points at.
///
/// On PATTERNS.md §6 (off-UI-thread), stated per branch rather than assumed: this
/// is a ONE-SHOT at hand-off, adding no tick/thread/event source and leaving the
/// `--all` poll untouched at one call per cycle. On the ATTACH branch nothing
/// renders between the probe and the terminal teardown, so its ~0.26s is
/// invisible; on the two REFUSAL branches the board redraws ~0.26s after the
/// keypress — a small, deliberate hitch, accepted because the alternative is
/// handing the user a broken `claude attach`.
fn route_handoff(app: &mut App, session_id: &str, kind: Handoff) -> Outcome {
    let checked = match kind {
        Handoff::Attach => {
            // The probe's record is OWNED, so it holds no borrow on `app` when
            // `session_by_id` re-borrows below.
            let Some(agent) = app.live_agent_now(session_id) else {
                app.set_status(resume::ATTACH_NOT_LIVE.to_string());
                return Outcome::Continue;
            };
            app.session_by_id(session_id)
                .map(|s| resume::check_attach(s, agent.id.as_deref()))
        }
        // Neither carries a model: `check` and `check_attach` take none (see
        // `HandoffCtx::model`), so a fork normally keeps the session's own model
        // and an attach joins a process already running under one.
        Handoff::Fork => app
            .session_by_id(session_id)
            .map(|s| resume::check(s, true)),
    };
    match checked {
        Some(Ok(ready)) => Outcome::Resume(ready),
        Some(Err(err)) => {
            app.set_status(err.message().to_string());
            Outcome::Continue
        }
        None => Outcome::Continue,
    }
}

/// Handle `Ctrl-N`. When defined agents exist for the launch dir, OPEN the agent
/// picker (pre-highlighted on the last pick) and stay on the board; otherwise open
/// the BACKGROUND draft pane straight away, bound to no agent.
///
/// Both branches land on the SAME draft, because drafting is what a new session
/// defaults to now — a one-row picker offering only "default (no agent)" would be
/// pure friction, and skipping it costs nothing: the draft's own `Ctrl-O` still
/// reaches an interactive start in one key, exactly as the picker's `Ctrl-O` does.
/// Discovery is FAIL-SOFT — any error yields an empty list, which just means the
/// draft branch (see [`defined_agents::discover_agents`]).
fn new_session(app: &mut App) -> Outcome {
    let agents = defined_agents::discover_agents(&app.launch_dir);
    if agents.is_empty() {
        // No selectable agents: skip the pointless one-row picker and draft
        // directly, with no agent bound.
        compose::open_background(app, None);
        return Outcome::Continue;
    }
    app.open_agent_picker(agents);
    Outcome::Continue
}

/// Handle `Ctrl-R` (quick reply). Ask claude what it is holding the SELECTED
/// session as, one-shot, then route via [`send::reply_gate`].
///
/// One question comes BEFORE that probe: does THIS session already have a quick
/// reply of its own in flight ([`App::sending_to`])? If so `Ctrl-R` is refused
/// ([`send::reply_in_flight_refusal`]). A reply in flight to ANOTHER session is not
/// asked about: [`App::sending`] keeps one entry per session, so a reply here is
/// tracked beside it, and [`delete::can_delete_target`] keeps refusing to delete
/// either transcript while its own child writes. The same session stays refused
/// because two `claude -p -r` runs would both append to one transcript, and the
/// first to finish would clear the entry the second still needed. Refusing here,
/// rather than at submit, means no compose box opens for a reply that could not be
/// sent, so nothing typed is thrown away, and no probe is spent.
///
/// `claude -p -r <id>` refuses to resume a session registered as a live agent, but
/// `claude stop <job-id>` deregisters the job (keeping the conversation) so the
/// reply can then land in place. Stopping is only safe when nothing is running, so:
///
/// * not held → open compose, reply in place;
/// * `done`, or a TERMINAL `stopped`/`failed` → open compose in stop-then-reply
///   mode (the run is over, so stopping it interrupts nothing);
/// * `needs input` → CONFIRM first ([`App::open_stop_confirm`]) — stopping abandons
///   a waiting agent — then compose;
/// * `working`/`idle`/`interrupted`/an unrecognized qualifier/unstoppable → refuse
///   ([`send::SEND_LIVE_REFUSED`]).
///
/// The probe is the SAME authoritative bare read the resume gate uses
/// ([`App::live_agent_now`]) — never the polled `--all` map — a one-shot at a
/// hand-off (the documented OFF-UI-THREAD exception, PATTERNS.md §6). The id is
/// cloned so the `&Session` borrow ends before the probe and the app mutations.
fn reply(app: &mut App) -> Outcome {
    let Some(id) = app.selected_session().map(|s| s.session_id.clone()) else {
        return Outcome::Continue;
    };
    // ONE in-flight reply per SESSION, refused before the probe (see above).
    if let Some(refusal) = send::reply_in_flight_refusal(app.sending_to(&id).is_some()) {
        app.set_status(refusal);
        return Outcome::Continue;
    }
    match send::reply_gate(app.live_agent_now(&id).as_ref()) {
        ReplyGate::Reply => compose::open(app, id, None),
        ReplyGate::StopThenReply { job_id } => compose::open(app, id, Some(job_id)),
        ReplyGate::ConfirmStopThenReply { job_id } => app.open_stop_confirm(id, job_id),
        ReplyGate::Refuse(message) => app.set_status(message),
    }
    Outcome::Continue
}

/// Apply a keypress while the "stop the waiting agent?" confirmation is open.
///
/// `Enter` confirms — the waiting agent is stopped as part of the send — so it
/// resolves the confirmation into compose in stop-then-reply mode; `Esc`/`Ctrl-C`
/// dismiss and return to the board. Any other key is ignored: this is a deliberate
/// confirmation, not a fat-finger.
fn handle_stop_confirm_key(app: &mut App, key: KeyEvent) -> Outcome {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Enter => {
            if let Some(pending) = app.pending_stop.take() {
                compose::open(app, pending.session_id, Some(pending.job_id));
            }
        }
        KeyCode::Esc => app.stop_confirm_cancel(),
        KeyCode::Char('c' | 'C') if ctrl => app.stop_confirm_cancel(),
        _ => {}
    }
    Outcome::Continue
}

/// Handle `Ctrl-K` (interrupt). Ask claude what it is holding the SELECTED session
/// as, one-shot, then route via [`send::interrupt_gate`].
///
/// Unlike a reply, an interrupt is MEANT to stop live work, so a `working` agent is
/// a valid target here. `claude stop <job-id>` deregisters the job (keeping the
/// conversation); it needs the SHORT agent-view id, which only a background job has:
///
/// * not held, or reported with neither a job id nor a pid → refuse
///   ([`send::INTERRUPT_NOT_LIVE`] / [`send::INTERRUPT_NO_JOB_ID`]);
/// * no stoppable job id and a `pid` no signal could ever take (`0`, past
///   `i32::MAX`, or this board's own, [`App::own_pid`]) → refuse
///   ([`send::INTERRUPT_PID_UNUSABLE`]), so no confirm opens;
/// * `done`, or a TERMINAL `stopped`/`failed` → stop immediately (harmless;
///   nothing runs);
/// * any other live state → CONFIRM first ([`App::open_interrupt_confirm`]) —
///   stopping abandons live work — then stop on confirm;
/// * no stoppable job id but a reported `pid` a signal could take → CONFIRM, then
///   re-probe and SIGTERM that pid ([`dispatch_signal`]). The pid captured here is
///   a claim to re-verify, never a target — see [`send::signal_plan`].
///
/// The probe is the SAME authoritative bare read the resume/reply gates use
/// ([`App::live_agent_now`]) — never the polled `--all` map — a one-shot at a
/// hand-off (the documented OFF-UI-THREAD exception, PATTERNS.md §6). The id is
/// cloned so the `&Session` borrow ends before the probe and the app mutations.
fn interrupt(app: &mut App) -> Outcome {
    let Some(id) = app.selected_session().map(|s| s.session_id.clone()) else {
        return Outcome::Continue;
    };
    match send::interrupt_gate(app.live_agent_now(&id).as_ref(), app.own_pid) {
        InterruptGate::StopNow { job_id } => dispatch_interrupt(app, &id, &job_id),
        InterruptGate::Confirm { job_id } => {
            app.open_interrupt_confirm(id, InterruptRoute::Job { job_id });
            Outcome::Continue
        }
        // No stoppable job id, but claude reported a pid: confirm, then re-probe and
        // signal it (see `dispatch_signal`). The pid is CAPTURED here and re-verified
        // at `Enter` — it is never signalled on the strength of this read.
        InterruptGate::ConfirmSignal { pid } => {
            app.open_interrupt_confirm(id, InterruptRoute::Signal { pid });
            Outcome::Continue
        }
        InterruptGate::Refuse(message) => {
            app.set_status(message);
            Outcome::Continue
        }
    }
}

/// Build the interrupt request, mark the interrupt in flight, and escalate to the
/// driver so the spawn stays OUT of this pure handler (mirroring how a send returns
/// [`Outcome::Send`]). `claude stop` acts on the global job registry, so the child
/// runs in the launch dir — never a re-read of the session's `cwd`, which a deleted
/// worktree could have removed even while its job is still live.
fn dispatch_interrupt(app: &mut App, session_id: &str, job_id: &str) -> Outcome {
    let req = InterruptRequest {
        argv: send::build_stop_argv(job_id),
        cwd: app.launch_dir.clone(),
        session_id: session_id.to_string(),
    };
    app.interrupting = Some(Interrupting {
        session_id: session_id.to_string(),
    });
    Outcome::Interrupt(req)
}

/// Signal the pid the confirm captured — but only after asking claude AGAIN, right
/// now, whether that pid is still the one it reports for this session.
///
/// The interrupt confirm's SECOND dispatcher, beside [`dispatch_interrupt`], and the
/// re-probe is the only difference between them.
///
/// # Why the re-probe is on THIS route alone
///
/// A stale job id and a stale pid are not equally dangerous, and the asymmetry is the
/// argument for the guard living here:
///
/// * A stale job id FAILS SAFE. `claude stop <dead-job>` exits non-zero with "No job
///   matching" and [`send::status_for_stop`] surfaces that reason. Nothing else is
///   touched, so the job-id arm has never needed a re-probe and does not get one.
/// * A stale pid does NOT fail safe. A pid is a slot the kernel recycles: the confirm
///   can sit open indefinitely (the same unbounded window [`route_handoff`] documents
///   for the attach id), and a process that exits inside it frees its number for
///   anything. A SIGTERM aimed at the captured number could land on an unrelated
///   process, and there is no undo.
///
/// So the two arms are deliberately NOT symmetric. Do not "unify" them: making the job
/// arm re-probe would be harmless but pointless, and making this one stop re-probing
/// deletes the only thing standing between a recycled pid and a signal. The decision
/// itself is pure and unit-tested in [`send::signal_plan`]; all this adds is the fresh
/// record it judges.
///
/// # On PATTERNS.md §6 (off-UI-thread), argued per branch
///
/// [`App::live_agent_now`] here is a ONE-SHOT at a hand-off-shaped moment — the same
/// exception the Enter gate, [`route_handoff`] and `confirm_delete` each argue at their
/// own call sites, and the `Ctrl-K` keypress that opened this confirm already spent one.
/// It adds no tick, no thread and no event source, and leaves the `--all` poller at one
/// call per cycle. Its ~0.26s lands between the keypress and a board that redraws with
/// either a signalled row or the refusal that says why not — the same deliberate hitch
/// every other hand-off pays, accepted here because the alternative is signalling a pid
/// nothing current vouches for.
///
/// The signal itself does NOT go on a thread: see [`Outcome::Signal`] for why a
/// non-blocking syscall needs neither one nor an `AppEvent` round trip.
fn dispatch_signal(app: &mut App, session_id: &str, captured_pid: u32) -> Outcome {
    // The probe's record is OWNED, so it holds no borrow on `app` when `set_status`
    // re-borrows it mutably below.
    let fresh = app.live_agent_now(session_id);
    match send::signal_plan(captured_pid, fresh.as_ref()) {
        SignalPlan::Signal { pid } => Outcome::Signal { pid },
        SignalPlan::Refuse(message) => {
            app.set_status(message);
            Outcome::Continue
        }
    }
}

/// Show what the driver's SIGTERM did on the board status line.
///
/// The driver performs [`Outcome::Signal`] and passes [`send::signal_term`]'s result
/// straight in. Everything after that syscall is decided HERE, from the result
/// as a PARAMETER, so it is unit-tested with hand-built results and no test ever
/// signals anything. [`send::status_for_signal`] maps the result to its text and
/// class, and this applies the class the way every other outcome is applied
/// (STATUS-LINE OWNERSHIP): a neutral one (sent, or already gone) expires after
/// `STATUS_DWELL_TICKS`, while a failure stays until the next actionable keypress,
/// because the process may still be running.
pub(super) fn show_signal_result(app: &mut App, result: Result<(), std::io::Error>) {
    let (status, neutral) = send::status_for_signal(result);
    if neutral {
        app.set_status_transient(status);
    } else {
        app.set_status(status);
    }
}

/// Apply a keypress while the interrupt confirmation is open.
///
/// `Enter` confirms, resolving into the effect its [`InterruptRoute`] names: a
/// `claude stop` ([`Outcome::Interrupt`]) on the job-id route, or a re-probed SIGTERM
/// ([`dispatch_signal`]) on the pid route. `Esc`/`Ctrl-C` dismiss and return to the
/// board. Any other key is ignored: this is a deliberate confirmation, not a
/// fat-finger.
///
/// The route was chosen by [`send::interrupt_gate`] when the confirm opened and travels
/// as a sum type, so this arm dispatches the decision rather than re-deriving it —
/// there is no state here in which both handles, or neither, are available.
fn handle_interrupt_confirm_key(app: &mut App, key: KeyEvent) -> Outcome {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Enter => {
            if let Some(pending) = app.pending_interrupt.take() {
                return match pending.route {
                    InterruptRoute::Job { job_id } => {
                        dispatch_interrupt(app, &pending.session_id, &job_id)
                    }
                    InterruptRoute::Signal { pid } => {
                        dispatch_signal(app, &pending.session_id, pid)
                    }
                };
            }
            Outcome::Continue
        }
        KeyCode::Esc => {
            app.interrupt_confirm_cancel();
            Outcome::Continue
        }
        KeyCode::Char('c' | 'C') if ctrl => {
            app.interrupt_confirm_cancel();
            Outcome::Continue
        }
        _ => Outcome::Continue,
    }
}

/// Run the new-session existence gate for `agent` (`None` = no agent) and an
/// optional first `prompt` while the terminal is still up, and escalate a confirmed
/// plan to [`Outcome::Resume`]; a refusal (a deleted launch dir) sets a transient
/// board status. Shared by the TWO interactive routes — the picker's `Ctrl-O`
/// ([`launch_pick_interactively`]) and the draft pane's `Ctrl-O` — so the gate +
/// status handling live in one place. `check_new` returns an owned `Result`, so
/// the `&launch_dir` borrow is released before we mutably touch `app` for
/// `set_status`.
///
/// Those two callers differ in whether a draft was opened, so `prompt` and `model`
/// are what separate them: the picker's `Ctrl-O` passes `None` for both and emits
/// the bare argv snapback has always emitted for a new session, while the draft
/// pane passes `Some(prompt)` whenever its buffer is non-empty and the model picked
/// in THAT draft (`Ctrl-L`), if any. A `Some(prompt)` AUTO-SUBMITS as the session's
/// first turn (see [`resume::build_new_argv`]).
///
/// The model is a PARAMETER rather than read from the board, because there is no
/// board-wide model: a pick belongs to one compose, so only the caller that owns a
/// compose can have one to pass, and the picker's route — which skips the draft —
/// structurally cannot.
pub(super) fn launch_new_session(
    app: &mut App,
    agent: Option<&str>,
    prompt: Option<&str>,
    model: Option<&resume::ModelPick>,
) -> Outcome {
    match resume::check_new(&app.launch_dir, agent, model, prompt) {
        Ok(ready) => Outcome::Resume(ready),
        Err(err) => {
            app.set_status(err.message().to_string());
            Outcome::Continue
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use ratatui::backend::TestBackend;
    use ratatui::style::Modifier;
    use ratatui::Terminal;

    use crate::agents::ReportedAgent;
    use crate::resume::ModelPick;
    use crate::search::{filter, SearchMode};
    use crate::store::Session;
    use crate::tui::app::{
        NewSessionDraft, PaneLayout, Scope, AUTOSCROLL_FRAME, STATUS_DWELL_TICKS,
    };
    use crate::tui::compose::ComposeTarget;

    /// A store over `root` for a test that drives [`handle_event`]. Most routing
    /// tests never reload at all, so the root is usually a placeholder — where a
    /// reload IS exercised (the delete tests), it is the test's own temp store.
    fn store_at(root: &Path) -> SessionStore {
        SessionStore::new(root)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    /// The SHIFT-modified form crossterm decodes `CSI 1;2A` / `CSI 1;2B` into
    /// (`parse_csi_modifier_key_code` maps the final `A`/`B` to `Up`/`Down` and the
    /// `2` parameter to this modifier), which is what a terminal sends for
    /// `Shift-Up` / `Shift-Down`.
    fn shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    /// The ALT-modified form a terminal in option-as-meta mode sends: the ESC
    /// prefix crossterm folds back into [`KeyModifiers::ALT`] (`ESC 0x7F` for
    /// `Alt-Backspace`, `ESC h` for `Alt-H`).
    fn alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    /// `Alt` with `SHIFT` alongside: what crossterm decodes `ESC B` into (a
    /// capital letter after the ESC prefix carries `SHIFT`), and `CSI 1;4D` /
    /// `C` (`Shift-Alt-←` / `→`).
    fn alt_shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT | KeyModifiers::SHIFT)
    }

    /// `Ctrl-Shift-<key>`: the modifier set a terminal reports when the shifted
    /// letter is held with control. Both this and the plain-CONTROL uppercase
    /// form reach the same arm, which is what the `w`/`W` pattern is for.
    fn ctrl_shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL | KeyModifiers::SHIFT)
    }

    /// A synthetic session addressable by id (cwd/file are not exercised by the
    /// routing tests, which assert overlay STATE rather than a real hand-off).
    fn session(id: &str) -> Session {
        Session {
            file: PathBuf::from(format!("/tmp/{id}.jsonl")),
            session_id: id.to_string(),
            cwd: PathBuf::from(format!("/tmp/{id}")),
            git_branch: Some("main".to_string()),
            timestamp: None,
            repo: "repo".to_string(),
            label: format!("label {id}"),
            root_uuid: None,
            msg_count: 0,
            content_index: String::new(),
            background: false,
            has_agent_name: false,
            has_agent_setting: false,
            failed_task: None,
        }
    }

    /// Pid-less and `startedAt`-less by default; a test that needs either appends
    /// [`ReportedAgent::with_pid`] rather than this growing a parameter every
    /// existing caller would have to pass `None` to.
    fn reported_agent(kind: &str) -> ReportedAgent {
        ReportedAgent {
            kind: kind.to_string(),
            // Interactive by default (no attachable job id); the background
            // helper below supplies one when a test needs an attachable agent.
            id: None,
            state: None,
            status: None,
            pid: None,
            started_at_ms: None,
        }
    }

    /// A record as claude's ACTIVE list (`claude agents --json`, no `--all`)
    /// reports it: a BACKGROUND agent carries the short agent-view job id that
    /// `claude attach` matches, an INTERACTIVE one has no attachable job.
    fn live_agent(kind: &str, job_id: Option<&str>) -> ReportedAgent {
        ReportedAgent {
            kind: kind.to_string(),
            id: job_id.map(str::to_owned),
            state: None,
            status: None,
            pid: None,
            started_at_ms: None,
        }
    }

    /// Claude's ACTIVE list as a map, from `(session_id, kind, job_id)` triples.
    fn live_map(agents: &[(&str, &str, Option<&str>)]) -> HashMap<String, ReportedAgent> {
        agents
            .iter()
            .map(|(id, kind, job)| ((*id).to_string(), live_agent(kind, *job)))
            .collect()
    }

    /// Seed what claude's ACTIVE list reports, as explicit records:
    /// `(session_id, kind, job_id)`.
    ///
    /// The only seam that can state a job id, so any test where the ATTACH TARGET
    /// matters must build its premise here.
    fn seed_live_agents(app: &mut App, agents: &[(&str, &str, Option<&str>)]) {
        let live = live_map(agents);
        app.set_live_probe(move || live.clone());
    }

    /// Seed a probe that answers `first` on its FIRST call and `later` on every
    /// call after it.
    ///
    /// The board asks claude once per HAND-OFF, so the two answers are exactly
    /// "what claude said at the Enter gate" and "what claude says at the hand-off
    /// the user then chose". A session can finish in the gap — the overlay sits
    /// open as long as the user takes to decide — and expressing that gap is the
    /// entire reason the Attach path re-asks instead of reusing either the gate's
    /// answer or the polled map.
    fn seed_live_then(
        app: &mut App,
        first: &[(&str, &str, Option<&str>)],
        later: &[(&str, &str, Option<&str>)],
    ) {
        let first = live_map(first);
        let later = live_map(later);
        let calls = std::cell::Cell::new(0u32);
        app.set_live_probe(move || {
            let nth = calls.get();
            calls.set(nth + 1);
            if nth == 0 {
                first.clone()
            } else {
                later.clone()
            }
        });
    }

    /// Seed claude's ACTIVE list by MEMBERSHIP alone: each id is reported live as
    /// an INTERACTIVE session, carrying no attachable job.
    ///
    /// Membership is the whole of what the resume gate asks, so these records
    /// state nothing the gate tests do not mean. The absent job id is also the
    /// SAFE default for anything that wanders onto the Attach path through this
    /// seam: it refuses (`ATTACH_NO_JOB_ID`) rather than inventing a plausible id
    /// and passing. A test that needs an attachable agent must SAY so, via
    /// `seed_live_agents`.
    ///
    /// Every test that reaches the gate MUST call one of these — `App`'s
    /// test-mode default probe panics rather than spawning `claude` — so each one
    /// states its own premise instead of inheriting a silent "nothing is live".
    fn seed_live(app: &mut App, live: &[&str]) {
        let agents: Vec<(&str, &str, Option<&str>)> =
            live.iter().map(|id| (*id, "interactive", None)).collect();
        seed_live_agents(app, &agents);
    }

    /// An app over one session, optionally joined to a reported agent of `kind`.
    ///
    /// `Some(kind)` means "claude reports this session as a running agent", so
    /// the live set is seeded to match — the badge map and the probe agree here,
    /// which is the STEADY-STATE case. The tests where they deliberately DISAGREE
    /// (the TOCTOU race) build their app via `app_with_agent_state`.
    ///
    /// The agreement extends to the KIND, because the probe's record is what the
    /// Attach hand-off now resolves its job id from: a background agent exposes an
    /// attachable job, an interactive one does not.
    fn app_with(id: &str, agent_kind: Option<&str>) -> App {
        let mut app = App::new(vec![session(id)], Scope::All, PathBuf::from("/tmp"));
        match agent_kind {
            Some(kind) => {
                let mut reported = HashMap::new();
                reported.insert(id.to_string(), reported_agent(kind));
                app.set_reported_agents(reported, None);
                let job = (kind == "background").then_some("job-steady");
                seed_live_agents(&mut app, &[(id, kind, job)]);
            }
            None => seed_live(&mut app, &[]),
        }
        assert_eq!(app.selected.as_deref(), Some(id));
        app
    }

    /// The job id an open REPLY compose zone will `claude stop` before sending.
    ///
    /// `None` covers "no compose open", "a plain in-place reply", and "a background
    /// draft" alike — every caller here is asserting the reply path, where the
    /// first and last cannot occur.
    fn composing_stop_job(app: &App) -> Option<&str> {
        match app.compose.as_ref().map(|c| &c.target) {
            Some(ComposeTarget::Reply { stop_job, .. }) => stop_job.as_deref(),
            _ => None,
        }
    }

    /// Type `text` into the open compose zone one KEYPRESS at a time, through
    /// `handle_event` — so the draft is built by the same routing the user's typing
    /// goes through, not by reaching into the `TextArea` behind it. A `\n` is sent
    /// as the `Ctrl-J` newline chord, since a bare `Enter` would submit.
    fn type_into_draft(app: &mut App, text: &str) {
        for c in text.chars() {
            if c == '\n' {
                press_ctrl(app, KeyCode::Char('j'));
            } else {
                press(app, KeyCode::Char(c));
            }
        }
    }

    fn press(app: &mut App, code: KeyCode) -> Outcome {
        handle_event(
            app,
            AppEvent::Input(Event::Key(key(code))),
            &mut store_at(Path::new("/tmp")),
        )
    }

    fn press_ctrl(app: &mut App, code: KeyCode) -> Outcome {
        handle_event(
            app,
            AppEvent::Input(Event::Key(ctrl(code))),
            &mut store_at(Path::new("/tmp")),
        )
    }

    fn press_alt(app: &mut App, code: KeyCode) -> Outcome {
        handle_event(
            app,
            AppEvent::Input(Event::Key(alt(code))),
            &mut store_at(Path::new("/tmp")),
        )
    }

    /// Type `text` into the BOARD's query one keypress at a time, through
    /// `handle_event` — the board sibling of [`type_into_draft`], for text with no
    /// newline in it.
    fn type_into_board(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    fn press_shift(app: &mut App, code: KeyCode) -> Outcome {
        handle_event(
            app,
            AppEvent::Input(Event::Key(shift(code))),
            &mut store_at(Path::new("/tmp")),
        )
    }

    /// Deliver `text` as ONE terminal paste — the `Event::Paste` crossterm emits
    /// between `ESC[200~` and `ESC[201~` once bracketed paste is enabled. The
    /// whole point of the routing under test is that this is NOT a stream of
    /// keypresses, so the helper never decomposes it into one.
    fn paste(app: &mut App, text: &str) -> Outcome {
        handle_event(
            app,
            AppEvent::Input(Event::Paste(text.to_string())),
            &mut store_at(Path::new("/tmp")),
        )
    }

    /// The open compose buffer's text, joined the way a submit would read it.
    fn draft_text(app: &App) -> String {
        app.compose
            .as_ref()
            .expect("compose is open")
            .textarea
            .lines()
            .join("\n")
    }

    /// A real resumable session file (existing in-file cwd) so `send::plan_send`
    /// reaches `Ready` at Send time. Returns the `Session` and its temp cwd to
    /// clean up.
    fn resumable_session_for_send() -> (Session, PathBuf) {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the unix epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "snapback-update-send-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp cwd");
        let session = resumable_session_in(&dir, "sess-send-e2e", "e2e");
        (session, dir)
    }

    /// Write `<dir>/<id>.jsonl` as a real resumable session whose in-file cwd is
    /// `dir` itself (so it exists), and return its `Session`. `send::plan_send`
    /// reaches `Ready` on it at Send time. The caller owns `dir` and removes it.
    fn resumable_session_in(dir: &Path, id: &str, label: &str) -> Session {
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
        Session {
            file,
            session_id: id.to_string(),
            cwd: dir.to_path_buf(),
            git_branch: None,
            timestamp: None,
            repo: "repo".to_string(),
            label: label.to_string(),
            root_uuid: None,
            msg_count: 0,
            content_index: String::new(),
            background: false,
            has_agent_name: false,
            has_agent_setting: false,
            failed_task: None,
        }
    }

    fn mouse_ev(kind: MouseEventKind, col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// [`mouse_effect`] at the real clock — for tests that do not time clicks.
    fn mouse_at(app: &mut App, mouse: MouseEvent) -> MouseEffect {
        mouse_effect(app, mouse, Instant::now())
    }

    fn wheel(app: &mut App, kind: MouseEventKind, col: u16, row: u16) -> Outcome {
        handle_event(
            app,
            AppEvent::Input(Event::Mouse(mouse_ev(kind, col, row))),
            &mut store_at(Path::new("/tmp")),
        )
    }

    /// A whole left CLICK at `(col, row)` — press, then release, no drag — through
    /// the real `handle_event`, the way a user's click arrives. The press only
    /// records where it landed; the RELEASE is what toggles a node or opens a link
    /// (`click_effect`), so a press alone proves nothing about either. Returns the
    /// release's outcome.
    fn left_click(app: &mut App, col: u16, row: u16) -> Outcome {
        wheel(app, MouseEventKind::Down(MouseButton::Left), col, row);
        wheel(app, MouseEventKind::Up(MouseButton::Left), col, row)
    }

    /// A whole left click at `(col, row)` that lands MORE than
    /// [`DOUBLE_CLICK_INTERVAL`] after the last click the board recorded — a
    /// SEPARATE click, never the second half of a double-click. A test's clicks
    /// arrive microseconds apart, and [`handle_event`] reads the real clock, so a
    /// second [`left_click`] on the same cell IS a double-click (it selects a word
    /// and toggles nothing); this one is driven through the pure [`mouse_effect`]
    /// at an instant past the interval instead. Measured from the recorded click
    /// itself, not from now, so a run of these can never chain into a double-click
    /// either. Returns the release's effect.
    fn separate_left_click(app: &mut App, col: u16, row: u16) -> MouseEffect {
        let now = Instant::now();
        let after = app.last_click().map_or(now, |click| click.at.max(now));
        let at = after + DOUBLE_CLICK_INTERVAL + Duration::from_millis(1);
        mouse_effect(
            app,
            mouse_ev(MouseEventKind::Down(MouseButton::Left), col, row),
            at,
        );
        mouse_effect(
            app,
            mouse_ev(MouseEventKind::Up(MouseButton::Left), col, row),
            at,
        )
    }

    // --- quick reply (Ctrl-R): gate, compose routing, send, completion ----

    /// Ctrl-R on an IDLE session opens the compose zone (bringing a hidden preview
    /// back at 1:1, and targeting the selected session); on a LIVE session it
    /// refuses with the hint and opens nothing.
    #[test]
    fn ctrl_r_opens_compose_on_idle_and_refuses_a_live_session() {
        // Idle: compose opens, and a 1:0 board lands on 1:1 so the pane the reply
        // box docks in is on screen.
        let mut app = app_with("idle", None);
        app.set_pane_layout(PaneLayout::ListOnly); // prove Reply brings the preview back
        press_ctrl(&mut app, KeyCode::Char('r'));
        assert!(
            app.is_composing(),
            "Ctrl-R on an idle session opens compose"
        );
        assert_eq!(
            app.pane_layout(),
            PaneLayout::Even,
            "opening a reply from 1:0 lands on 1:1"
        );
        assert_eq!(
            app.compose.as_ref().map(|c| &c.target),
            Some(&ComposeTarget::Reply {
                session_id: "idle".to_string(),
                stop_job: None,
            }),
            "compose targets the selected session, as a plain in-place reply"
        );

        // Live: refuse with the hint, open nothing (sending in place would branch).
        let mut app = app_with("live-1", Some("background"));
        press_ctrl(&mut app, KeyCode::Char('r'));
        assert!(!app.is_composing(), "Ctrl-R must refuse a live session");
        assert_eq!(app.status.as_deref(), Some(send::SEND_LIVE_REFUSED));
    }

    /// Ctrl-R routes by the held agent's state: not held → reply; `done` → compose
    /// in stop-then-reply mode; `needs input` → the stop confirmation first;
    /// `working`/`idle` → refuse. Stopping a job first is what lets `-p -r` land.
    #[test]
    fn ctrl_r_routes_by_agent_state() {
        // Seed claude's ACTIVE (bare) list with a background agent of a given state,
        // carrying the short job id `claude stop` would target.
        fn held(id: &str, state: &str) -> HashMap<String, ReportedAgent> {
            let mut map = HashMap::new();
            map.insert(
                id.to_string(),
                ReportedAgent {
                    kind: "background".to_string(),
                    id: Some("job-x".to_string()),
                    state: Some(state.to_string()),
                    status: None,
                    pid: None,
                    started_at_ms: None,
                },
            );
            map
        }
        let app_for = |id: &str, state: &str| {
            let mut app = App::new(vec![session(id)], Scope::All, PathBuf::from("/tmp"));
            let live = held(id, state);
            app.set_live_probe(move || live.clone());
            app
        };

        // done -> compose opens straight away, in stop-then-reply mode.
        let mut app = app_for("done-1", "done");
        press_ctrl(&mut app, KeyCode::Char('r'));
        assert!(app.is_composing(), "a done agent goes straight to compose");
        assert_eq!(
            composing_stop_job(&app),
            Some("job-x"),
            "compose carries the job id to stop first"
        );
        assert!(app.pending_stop.is_none());

        // needs input -> the stop confirmation, NOT compose yet.
        for waiting in ["blocked", "waiting"] {
            let mut app = app_for("wait-1", waiting);
            press_ctrl(&mut app, KeyCode::Char('r'));
            assert!(
                !app.is_composing(),
                "{waiting:?} must confirm before composing"
            );
            let pending = app
                .pending_stop
                .as_ref()
                .expect("a waiting agent opens the stop confirmation");
            assert_eq!(pending.session_id, "wait-1");
            assert_eq!(pending.job_id, "job-x");
        }

        // working / idle -> refuse outright.
        for busy in ["working", "idle"] {
            let mut app = app_for("busy-1", busy);
            press_ctrl(&mut app, KeyCode::Char('r'));
            assert!(!app.is_composing(), "{busy:?} must refuse");
            assert!(app.pending_stop.is_none());
            assert_eq!(app.status.as_deref(), Some(send::SEND_LIVE_REFUSED));
        }

        // Not held at all -> a plain in-place reply (no stop).
        let mut app = App::new(vec![session("free-1")], Scope::All, PathBuf::from("/tmp"));
        app.set_live_probe(HashMap::new);
        press_ctrl(&mut app, KeyCode::Char('r'));
        assert!(
            app.is_composing(),
            "an unheld session is replyable in place"
        );
        assert_eq!(
            composing_stop_job(&app),
            None,
            "a plain reply carries no stop job"
        );
    }

    /// Confirming the stop prompt (`Enter`) opens compose in stop-then-reply mode;
    /// `Esc` dismisses it and composes nothing.
    #[test]
    fn stop_confirmation_enter_composes_and_esc_cancels() {
        fn waiting(id: &str) -> HashMap<String, ReportedAgent> {
            let mut map = HashMap::new();
            map.insert(
                id.to_string(),
                ReportedAgent {
                    kind: "background".to_string(),
                    id: Some("job-y".to_string()),
                    state: Some("blocked".to_string()),
                    status: None,
                    pid: None,
                    started_at_ms: None,
                },
            );
            map
        }

        // Enter -> compose opens carrying the job id; the confirmation closes.
        let mut app = App::new(vec![session("w")], Scope::All, PathBuf::from("/tmp"));
        let live = waiting("w");
        app.set_live_probe(move || live.clone());
        press_ctrl(&mut app, KeyCode::Char('r'));
        assert!(app.pending_stop.is_some());
        press(&mut app, KeyCode::Enter);
        assert!(app.pending_stop.is_none(), "confirming closes the prompt");
        assert!(app.is_composing(), "confirming opens compose");
        assert_eq!(composing_stop_job(&app), Some("job-y"));

        // Esc -> dismiss, compose nothing.
        let mut app = App::new(vec![session("w")], Scope::All, PathBuf::from("/tmp"));
        let live = waiting("w");
        app.set_live_probe(move || live.clone());
        press_ctrl(&mut app, KeyCode::Char('r'));
        assert!(app.pending_stop.is_some());
        press(&mut app, KeyCode::Esc);
        assert!(app.pending_stop.is_none(), "Esc dismisses the prompt");
        assert!(!app.is_composing(), "Esc composes nothing");
    }

    /// Ctrl-K routes by the live agent's state: `done` → stop immediately (escalates
    /// to `Outcome::Interrupt`); every OTHER live state → the interrupt confirmation
    /// first; not held → refuse (nothing to stop); interactive (no job id) → refuse.
    /// The stop argv carries the SHORT job id and runs in the launch dir.
    #[test]
    fn ctrl_k_routes_by_agent_state() {
        fn held(id: &str, state: &str, job: Option<&str>) -> HashMap<String, ReportedAgent> {
            let mut map = HashMap::new();
            map.insert(
                id.to_string(),
                ReportedAgent {
                    kind: "background".to_string(),
                    id: job.map(str::to_owned),
                    state: Some(state.to_string()),
                    status: None,
                    pid: None,
                    started_at_ms: None,
                },
            );
            map
        }
        let app_for = |id: &str, state: &str, job: Option<&str>| {
            let mut app = App::new(vec![session(id)], Scope::All, PathBuf::from("/tmp"));
            let live = held(id, state, job);
            app.set_live_probe(move || live.clone());
            app
        };

        // done -> stop immediately: Outcome::Interrupt with the stop argv, no confirm.
        let mut app = app_for("done-1", "done", Some("job-k"));
        let outcome = press_ctrl(&mut app, KeyCode::Char('k'));
        let Outcome::Interrupt(req) = outcome else {
            panic!("a done agent stops immediately");
        };
        assert_eq!(req.argv.join(" "), "claude stop job-k");
        assert_eq!(
            req.cwd,
            PathBuf::from("/tmp"),
            "stop runs in the launch dir"
        );
        assert!(
            app.pending_interrupt.is_none(),
            "done needs no confirmation"
        );
        assert!(
            app.interrupting_on("done-1").is_some(),
            "done agent marks the interrupt in flight"
        );

        // Every OTHER live state confirms first — including `working`, which the reply
        // gate (Ctrl-R) refuses. Interrupting live work is the whole point here.
        for state in ["working", "idle", "blocked", "waiting"] {
            let mut app = app_for("live-1", state, Some("job-k"));
            let outcome = press_ctrl(&mut app, KeyCode::Char('k'));
            assert!(
                matches!(outcome, Outcome::Continue),
                "{state:?} opens a confirm, not an immediate stop"
            );
            let Some(pending) = app.pending_interrupt.as_ref() else {
                panic!("{state:?} must open the interrupt confirmation");
            };
            assert_eq!(pending.session_id, "live-1");
            assert_eq!(
                pending.route,
                InterruptRoute::Job {
                    job_id: "job-k".to_string()
                },
                "a record with a job id confirms on the DELEGATED route, never the signal one"
            );
        }

        // Not held at all -> nothing to stop.
        let mut app = App::new(vec![session("free-1")], Scope::All, PathBuf::from("/tmp"));
        app.set_live_probe(HashMap::new);
        press_ctrl(&mut app, KeyCode::Char('k'));
        assert!(app.pending_interrupt.is_none());
        assert_eq!(app.status.as_deref(), Some(send::INTERRUPT_NOT_LIVE));

        // Live but interactive (no stoppable job id) -> refuse with the right hint.
        let mut app = app_for("inter-1", "working", None);
        press_ctrl(&mut app, KeyCode::Char('k'));
        assert!(app.pending_interrupt.is_none());
        assert_eq!(app.status.as_deref(), Some(send::INTERRUPT_NO_JOB_ID));
    }

    /// Confirming the interrupt prompt (`Enter`) escalates to `Outcome::Interrupt`
    /// carrying the stop argv and closes the prompt; `Esc` dismisses it and stops
    /// nothing.
    #[test]
    fn interrupt_confirmation_enter_stops_and_esc_cancels() {
        fn working(id: &str) -> HashMap<String, ReportedAgent> {
            let mut map = HashMap::new();
            map.insert(
                id.to_string(),
                ReportedAgent {
                    kind: "background".to_string(),
                    id: Some("job-z".to_string()),
                    state: Some("working".to_string()),
                    status: None,
                    pid: None,
                    started_at_ms: None,
                },
            );
            map
        }

        // Enter -> the stop is dispatched carrying the job id; the confirmation closes.
        let mut app = App::new(vec![session("w")], Scope::All, PathBuf::from("/tmp"));
        let live = working("w");
        app.set_live_probe(move || live.clone());
        press_ctrl(&mut app, KeyCode::Char('k'));
        assert!(app.pending_interrupt.is_some());
        let outcome = press(&mut app, KeyCode::Enter);
        let Outcome::Interrupt(req) = outcome else {
            panic!("confirming dispatches the stop");
        };
        assert_eq!(req.argv.join(" "), "claude stop job-z");
        assert!(
            app.pending_interrupt.is_none(),
            "confirming closes the prompt"
        );
        assert!(
            app.interrupting_on("w").is_some(),
            "confirming marks the interrupt in flight"
        );
        assert_eq!(
            app.status, None,
            "no visible 'stopping…' label is set (option c): the badge covers it"
        );

        // Esc -> dismiss, stop nothing.
        let mut app = App::new(vec![session("w")], Scope::All, PathBuf::from("/tmp"));
        let live = working("w");
        app.set_live_probe(move || live.clone());
        press_ctrl(&mut app, KeyCode::Char('k'));
        assert!(app.pending_interrupt.is_some());
        let outcome = press(&mut app, KeyCode::Esc);
        assert!(matches!(outcome, Outcome::Continue));
        assert!(app.pending_interrupt.is_none(), "Esc dismisses the prompt");
    }

    /// A board holding one session claude reports WITHOUT a job id but WITH `pid`,
    /// and whose probe can be re-seeded between the keypress and the confirm.
    ///
    /// The pid is a real one from the `claude 2.1.278` capture, and the record's
    /// shape mirrors it: `kind: "interactive"`, a `status` rather than a `state`, no
    /// `id`.
    fn app_on_the_pid_route(id: &str, pid: u32) -> App {
        let mut app = App::new(vec![session(id)], Scope::All, PathBuf::from("/tmp"));
        seed_probe(&mut app, id, Some(reported_pid_record(pid)));
        app
    }

    /// The observed interactive shape: no attachable job, a `status` rather than a
    /// `state`, and a pid.
    fn reported_pid_record(pid: u32) -> ReportedAgent {
        ReportedAgent::fixture("interactive", None, Some("busy")).with_pid(pid)
    }

    /// Re-seed `app`'s one-shot probe: `Some(record)` for `id`, or an EMPTY active
    /// list. Called a second time to state "the world moved while the confirm was
    /// open", which is the whole subject of the pid route's re-verification.
    fn seed_probe(app: &mut App, id: &str, record: Option<ReportedAgent>) {
        let id = id.to_string();
        app.set_live_probe(move || {
            let mut map = HashMap::new();
            if let Some(record) = record.clone() {
                map.insert(id.clone(), record);
            }
            map
        });
    }

    /// `Ctrl-K` on a reported session with NO stoppable job id but a `pid`: the
    /// confirm opens on the SIGNAL route carrying that pid, and `Enter` escalates to
    /// `Outcome::Signal` — the driver's syscall — rather than to a `claude stop`.
    ///
    /// Also pins that this route no longer refuses. The refusal it used to give
    /// (`INTERRUPT_NO_JOB_ID`) is now reserved for a record with neither handle, so a
    /// pid-carrying record reaching it again would mean the signal route had been
    /// disconnected.
    #[test]
    fn a_reported_pid_with_no_job_id_confirms_then_signals() {
        const PID: u32 = 29628;

        let mut app = app_on_the_pid_route("live-pid", PID);
        let outcome = press_ctrl(&mut app, KeyCode::Char('k'));
        assert!(
            matches!(outcome, Outcome::Continue),
            "the pid route CONFIRMS first — it never signals straight off a keypress"
        );
        let Some(pending) = app.pending_interrupt.as_ref() else {
            panic!("a reported pid with no job id must open the interrupt confirmation");
        };
        assert_eq!(pending.session_id, "live-pid");
        assert_eq!(
            pending.route,
            InterruptRoute::Signal { pid: PID },
            "the confirm must carry the pid, and only the pid"
        );
        assert_eq!(
            app.status, None,
            "the pid route must NOT refuse: INTERRUPT_NO_JOB_ID is now for a record \
             carrying neither a job id nor a pid"
        );

        // Enter: the probe still reports the same record, so the captured pid is
        // re-verified and the driver is handed the syscall.
        let outcome = press(&mut app, KeyCode::Enter);
        assert!(
            matches!(outcome, Outcome::Signal { pid } if pid == PID),
            "confirming an unchanged record must escalate Outcome::Signal for that pid"
        );
        assert!(
            app.pending_interrupt.is_none(),
            "confirming closes the prompt"
        );
        assert!(
            app.interrupting_on("live-pid").is_none(),
            "a signal has no in-flight child to track — `interrupting` is the \
             `claude stop` guard, and nothing would ever clear it here"
        );
    }

    /// The confirm-time re-probe, which is the guard against pid reuse: each of the
    /// three ways the world can move while the confirm sits open must REFUSE, in its
    /// own words, and none of them may reach `Outcome::Signal`.
    ///
    /// The confirm is opened against one probe answer and `Enter` pressed against
    /// another — the unbounded window the route actually has.
    #[test]
    fn the_confirm_time_re_probe_refuses_every_stale_pid() {
        const PID: u32 = 29628;

        for (moved_to, expected) in [
            // The session ended on its own: nothing reports it, so its pid is the
            // likeliest of all to have been recycled.
            (None, send::SIGNAL_RECORD_GONE),
            // It now carries a stoppable job id — the delegated verb, which wins
            // even though the pid is unchanged.
            (
                Some(live_agent("background", Some("job-now")).with_pid(PID)),
                send::SIGNAL_NOW_HAS_JOB,
            ),
            // The record was replaced: same session, different process.
            (Some(reported_pid_record(PID + 1)), send::SIGNAL_PID_MOVED),
        ] {
            let mut app = app_on_the_pid_route("live-pid", PID);
            press_ctrl(&mut app, KeyCode::Char('k'));
            assert!(
                app.pending_interrupt.is_some(),
                "the confirm must be open before the world moves under it"
            );

            seed_probe(&mut app, "live-pid", moved_to.clone());
            let outcome = press(&mut app, KeyCode::Enter);
            assert!(
                matches!(outcome, Outcome::Continue),
                "a stale pid must never reach the syscall (moved_to={moved_to:?})"
            );
            assert_eq!(
                app.status.as_deref(),
                Some(expected),
                "each staleness must refuse in its OWN words (moved_to={moved_to:?})"
            );
            assert!(
                app.pending_interrupt.is_none(),
                "a refused confirm still closes"
            );
        }
    }

    /// The keyboard-owner precedence reaches the SIGNAL route unchanged: its confirm
    /// swallows a typed key and a paste (neither leaks into the query nor resolves
    /// it), and `Esc` and `Ctrl-C` each dismiss it without escalating a signal.
    ///
    /// Pinned on this route because it is the one whose `Enter` ends in `kill(2)`:
    /// a key that slipped past the owner here decides whether a process is signalled,
    /// not just what the hidden query says. Only the returned `Outcome` is asserted —
    /// the syscall lives in the driver, which no test runs.
    #[test]
    fn the_signal_confirm_owns_the_keyboard_and_cancels_on_esc_or_ctrl_c() {
        const PID: u32 = 29628;

        for cancel in [key(KeyCode::Esc), ctrl(KeyCode::Char('c'))] {
            let mut app = app_on_the_pid_route("live-pid", PID);
            press_ctrl(&mut app, KeyCode::Char('k'));
            assert_eq!(
                app.pending_interrupt.as_ref().map(|p| &p.route),
                Some(&InterruptRoute::Signal { pid: PID }),
                "the fixture must really open the SIGNAL route's confirm"
            );

            // Owned: a typed key and a paste are swallowed, so the confirm stands and
            // nothing reaches the board's query.
            assert!(matches!(
                press(&mut app, KeyCode::Char('y')),
                Outcome::Continue
            ));
            assert!(matches!(paste(&mut app, "junk\ntext"), Outcome::Continue));
            assert!(
                app.pending_interrupt.is_some(),
                "neither a typed key nor a paste may resolve the confirm"
            );
            assert!(
                app.query().is_empty(),
                "the confirm owns the keyboard: nothing may leak into the query"
            );

            // Dismissed: the cancel key closes it and escalates nothing.
            let outcome = handle_event(
                &mut app,
                AppEvent::Input(Event::Key(cancel)),
                &mut store_at(Path::new("/tmp")),
            );
            assert!(
                matches!(outcome, Outcome::Continue),
                "{cancel:?} must dismiss the confirm, never signal or quit"
            );
            assert!(
                app.pending_interrupt.is_none(),
                "{cancel:?} must dismiss the confirm"
            );
        }
    }

    /// `Ctrl-K` on a record whose pid NO signal could take — `0`, or past
    /// `i32::MAX` — refuses at the keypress: no confirm opens, and the status says why
    /// in its own words (never "no process id": the record carries one). It stays
    /// until the next key, like every refusal.
    ///
    /// The probe is seeded, so this spawns nothing; and since no confirm opens,
    /// nothing here could reach `Outcome::Signal` either.
    #[test]
    fn a_pid_no_signal_could_take_refuses_without_opening_a_confirm() {
        for pid in [0, i32::MAX as u32 + 1, u32::MAX] {
            let mut app = app_on_the_pid_route("live-pid", pid);
            let outcome = press_ctrl(&mut app, KeyCode::Char('k'));
            assert!(
                matches!(outcome, Outcome::Continue),
                "pid {pid}: a refusal escalates nothing"
            );
            assert!(
                app.pending_interrupt.is_none(),
                "pid {pid} can never be signalled, so no confirm may open for it"
            );
            assert_eq!(
                app.status.as_deref(),
                Some(send::INTERRUPT_PID_UNUSABLE),
                "pid {pid}: the refusal must name what was observed"
            );
            for _ in 0..=STATUS_DWELL_TICKS {
                handle_event(&mut app, AppEvent::Tick, &mut store_at(Path::new("/tmp")));
            }
            assert_eq!(
                app.status.as_deref(),
                Some(send::INTERRUPT_PID_UNUSABLE),
                "pid {pid}: a refusal is sticky, so it must outlive the dwell"
            );
        }
    }

    /// `Ctrl-K` on a record naming the BOARD'S OWN pid refuses at the keypress, with
    /// no confirm, because the board reaches the gate with [`App::own_pid`] — the
    /// press, not the pure gate alone, is what this pins. A SIGTERM to that pid would
    /// end the board without its terminal restore.
    ///
    /// The same board first opens the ordinary signal confirm for that record, so
    /// the refusal is caused by the board's pid and nothing else about the fixture.
    /// The probe is seeded and no confirm survives, so nothing here can reach
    /// `Outcome::Signal`, let alone the driver's syscall.
    #[test]
    fn ctrl_k_on_the_boards_own_pid_refuses_without_opening_a_confirm() {
        const PID: u32 = 29628;

        let mut app = app_on_the_pid_route("live-pid", PID);
        press_ctrl(&mut app, KeyCode::Char('k'));
        assert_eq!(
            app.pending_interrupt.as_ref().map(|p| &p.route),
            Some(&InterruptRoute::Signal { pid: PID }),
            "while the pid is not the board's own, the fixture must open the signal confirm"
        );
        press(&mut app, KeyCode::Esc);

        app.own_pid = PID; // the record's pid is now this board's own
        let outcome = press_ctrl(&mut app, KeyCode::Char('k'));
        assert!(
            matches!(outcome, Outcome::Continue),
            "a refusal escalates nothing"
        );
        assert!(
            app.pending_interrupt.is_none(),
            "no confirm may open for the board's own pid"
        );
        assert_eq!(
            app.status.as_deref(),
            Some(send::INTERRUPT_PID_UNUSABLE),
            "the board's own pid must refuse in the words for a pid no signal could take"
        );
    }

    /// What the user sees after the driver's SIGTERM, for each kind of result: the
    /// status text, and whether it goes away on its own.
    ///
    /// Every result is BUILT BY HAND — `Ok(())`, an errno via
    /// `io::Error::from_raw_os_error`, and an `InvalidInput` like `signal_target`'s
    /// refusal — and handed to `show_signal_result` exactly as the driver hands it the
    /// syscall's result. No test calls `signal_term`, so nothing is signalled.
    ///
    /// Expiry is asserted by driving the SAME `AppEvent::Tick` dwell the board runs,
    /// not by reading a flag: a delivered or already-gone signal is a confirmation and
    /// must clear after `STATUS_DWELL_TICKS`, while a failure means the process may
    /// still be running and must stay until the next key.
    ///
    /// Unix-only, like `send`'s own errno test: the `ESRCH`/`EPERM` rows name `libc`
    /// constants, which exist on unix alone.
    #[cfg(unix)]
    #[test]
    fn a_signal_result_clears_when_neutral_and_stays_when_it_failed() {
        use std::io::{Error, ErrorKind};

        // The syscall's result is built ON DEMAND — once for the board, once for the
        // expected text — because `io::Error` is not `Clone`.
        type MakeResult = fn() -> Result<(), Error>;
        let sent: MakeResult = || Ok(());
        let already_gone: MakeResult = || Err(Error::from_raw_os_error(libc::ESRCH));
        let not_permitted: MakeResult = || Err(Error::from_raw_os_error(libc::EPERM));
        let out_of_range: MakeResult = || {
            Err(Error::new(
                ErrorKind::InvalidInput,
                "reported process id is out of range",
            ))
        };

        // (label, result, does the status clear on its own?)
        for (label, result, clears) in [
            ("sent", sent, true),
            ("ESRCH", already_gone, true),
            ("EPERM", not_permitted, false),
            ("out of range", out_of_range, false),
        ] {
            let (expected, _) = send::status_for_signal(result());
            let mut app = App::new(vec![session("s")], Scope::All, PathBuf::from("/tmp"));
            show_signal_result(&mut app, result());
            assert_eq!(
                app.status.as_deref(),
                Some(expected.as_str()),
                "{label}: the board must show the mapped text"
            );

            for _ in 0..STATUS_DWELL_TICKS {
                handle_event(&mut app, AppEvent::Tick, &mut store_at(Path::new("/tmp")));
            }
            if clears {
                assert_eq!(
                    app.status, None,
                    "{label}: a neutral outcome is a confirmation and must clear after \
                     the dwell"
                );
            } else {
                assert_eq!(
                    app.status.as_deref(),
                    Some(expected.as_str()),
                    "{label}: a failed signal must stay until the next key — the \
                     process may still be running"
                );
            }
        }
    }

    /// A finished `InterruptFinished` carrying a STALE session id must not clear a
    /// newer `app.interrupting` guard. The interrupt twin of the launch-identity
    /// regression tests: the board may have moved on and dispatched another stop,
    /// so attribution is by id, not by "any interrupt is in flight".
    #[test]
    fn a_stale_interrupt_finished_does_not_clear_a_newer_interrupting() {
        let mut app = App::new(
            vec![session("a"), session("b")],
            Scope::All,
            PathBuf::from("/tmp"),
        );
        let live = {
            let mut map = HashMap::new();
            map.insert(
                "a".to_string(),
                ReportedAgent {
                    kind: "background".to_string(),
                    id: Some("job-a".to_string()),
                    state: Some("done".to_string()),
                    status: None,
                    pid: None,
                    started_at_ms: None,
                },
            );
            map.insert(
                "b".to_string(),
                ReportedAgent {
                    kind: "background".to_string(),
                    id: Some("job-b".to_string()),
                    state: Some("done".to_string()),
                    status: None,
                    pid: None,
                    started_at_ms: None,
                },
            );
            map
        };
        app.set_live_probe(move || live.clone());

        // Dispatch the first interrupt for session a.
        assert_eq!(app.selected.as_deref(), Some("a"));
        assert!(
            matches!(
                press_ctrl(&mut app, KeyCode::Char('k')),
                Outcome::Interrupt(_)
            ),
            "Ctrl-K on a done background agent dispatches immediately"
        );
        assert!(
            app.interrupting_on("a").is_some(),
            "the first interrupt is in flight"
        );

        // Move to b and dispatch a second interrupt.
        press(&mut app, KeyCode::Down);
        assert_eq!(app.selected.as_deref(), Some("b"));
        assert!(
            matches!(
                press_ctrl(&mut app, KeyCode::Char('k')),
                Outcome::Interrupt(_)
            ),
            "Ctrl-K on the second session dispatches a second stop"
        );
        assert!(
            app.interrupting_on("b").is_some(),
            "the second interrupt is now in flight"
        );

        // The first interrupt reports back. It must surface its result, but it must
        // NOT clear the guard that belongs to the newer interrupt.
        let out = handle_event(
            &mut app,
            AppEvent::InterruptFinished {
                session_id: "a".to_string(),
                status: "stopped".to_string(),
                success: true,
            },
            &mut store_at(Path::new("/tmp")),
        );
        assert!(matches!(out, Outcome::Continue));
        assert!(
            app.interrupting_on("b").is_some(),
            "a stale interrupt result must not clear the newer interrupting guard"
        );
        assert!(app.interrupting_on("a").is_none());

        // The newer interrupt's own completion clears the guard.
        handle_event(
            &mut app,
            AppEvent::InterruptFinished {
                session_id: "b".to_string(),
                status: "stopped".to_string(),
                success: true,
            },
            &mut store_at(Path::new("/tmp")),
        );
        assert!(
            app.interrupting.is_none(),
            "the matching completion clears the guard"
        );
    }

    /// While composing, ordinary keys edit the buffer, Ctrl-J inserts a newline,
    /// and Esc cancels compose (never the app).
    #[test]
    fn composing_routes_keys_to_the_buffer_and_esc_cancels_only_compose() {
        let mut app = app_with("idle", None);
        press_ctrl(&mut app, KeyCode::Char('r'));
        assert!(app.is_composing());

        // Type "hi", a Ctrl-J newline, then "x".
        press(&mut app, KeyCode::Char('h'));
        press(&mut app, KeyCode::Char('i'));
        press_ctrl(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('x'));
        let text = app
            .compose
            .as_ref()
            .expect("still composing")
            .textarea
            .lines()
            .join("\n");
        assert_eq!(
            text, "hi\nx",
            "keys edit the buffer and Ctrl-J splits the line"
        );

        // Esc dismisses compose and keeps the app running (does NOT quit).
        let outcome = press(&mut app, KeyCode::Esc);
        assert!(
            matches!(outcome, Outcome::Continue),
            "Esc cancels compose, not the app"
        );
        assert!(!app.is_composing(), "Esc closes the compose zone");
    }

    // --- terminal paste (Event::Paste) routing -----------------------------

    /// THE regression: a multi-line paste into the reply compose zone must land in
    /// the draft WHOLE and send NOTHING.
    ///
    /// Without bracketed paste the terminal delivered the clipboard as a stream of
    /// `KeyEvent`s, so the first embedded newline arrived as a bare `Enter` —
    /// `ComposeAction::Send` — which sent line one as the reply, closed compose,
    /// typed the remainder into the board's SEARCH QUERY, and let a further newline
    /// reach `KeyCode::Enter => Action::Resume`, tearing the board down to spawn
    /// `claude`. An ordinary Cmd+V was a truncated send plus an unintended session
    /// hand-off. A pasted newline is DATA, so it must reach the editor as text.
    #[test]
    fn pasting_multiline_text_while_composing_keeps_the_whole_draft() {
        let mut app = app_with("idle", None);
        press_ctrl(&mut app, KeyCode::Char('r'));
        assert!(app.is_composing());

        let outcome = paste(&mut app, "line one\nline two\nline three");

        assert!(
            matches!(outcome, Outcome::Continue),
            "a paste must never submit: no Send, no Resume, no teardown"
        );
        assert!(
            app.is_composing(),
            "a paste must not close the compose zone"
        );
        assert_eq!(
            draft_text(&app),
            "line one\nline two\nline three",
            "every pasted line must survive in the draft"
        );
        assert!(
            app.query().is_empty(),
            "no part of the paste may leak into the board's search query"
        );
    }

    /// The SAME regression on the OTHER submit-capable box, which has the LARGER
    /// blast radius: `compose::compose_key_to_action` is shared by both targets and
    /// maps a bare `Enter` to Send, and `submit_compose` routes a background draft
    /// to [`Outcome::BgLaunch`] — so a clipboard drop arriving as keystrokes did not
    /// merely truncate a reply, it STARTED a background agent on line one and threw
    /// the rest at the board.
    ///
    /// `handle_paste` keys off `is_composing()` alone, so it is target-agnostic by
    /// construction; this pins that rather than trusting it, and the closing `Enter`
    /// proves the draft really was one keypress away from launching.
    #[test]
    fn pasting_multiline_text_into_a_background_draft_launches_nothing() {
        let mut app = app_with("idle", None);
        app.open_agent_picker(vec![def_agent("planner")]);
        press(&mut app, KeyCode::Enter); // the default row: a draft bound to no agent
        assert_eq!(
            app.compose.as_ref().map(|c| &c.target),
            Some(&ComposeTarget::NewBackgroundAgent { agent: None }),
            "the picker's Enter must open the background draft"
        );

        type_into_draft(&mut app, "keep ");
        let outcome = paste(&mut app, "line one\nline two\nline three");

        assert!(
            matches!(outcome, Outcome::Continue),
            "a paste must never launch: no BgLaunch, no Resume, no teardown"
        );
        assert!(app.is_composing(), "a paste must not close the draft");
        assert_eq!(
            draft_text(&app),
            "keep line one\nline two\nline three",
            "every pasted line must survive in the draft"
        );
        assert!(
            app.query().is_empty(),
            "no part of the paste may leak into the board's search query"
        );
        assert_eq!(
            app.status, None,
            "nothing was dispatched, so nothing may report itself in flight"
        );

        // Not a vacuous premise: the very next `Enter` DOES launch, so the paste
        // above walked past a LIVE submit path rather than an inert one.
        assert!(
            matches!(press(&mut app, KeyCode::Enter), Outcome::BgLaunch(_)),
            "Enter on this draft launches — which is exactly what the paste avoided"
        );
    }

    /// A paste lands AT THE CARET, like any other insert — it does not replace the
    /// draft and does not jump to the end.
    #[test]
    fn pasting_while_composing_inserts_at_the_caret() {
        let mut app = app_with("idle", None);
        press_ctrl(&mut app, KeyCode::Char('r'));
        type_into_draft(&mut app, "ab");
        press(&mut app, KeyCode::Left); // caret between a and b
        paste(&mut app, "X\nY");
        assert_eq!(draft_text(&app), "aX\nYb");
    }

    /// On the BOARD the query is a single line, so a multi-line paste is FLATTENED
    /// to spaces rather than truncated to its first line: `search::gate_atoms`
    /// splits the query on spaces into substring atoms that must all match, so
    /// `foo\nbar` becomes exactly the `foo bar` the user could have typed — nothing
    /// pasted is silently discarded.
    #[test]
    fn pasting_on_the_board_flattens_newlines_into_the_query() {
        let mut app = app_with("idle", None);
        let outcome = paste(&mut app, "alpha\nbravo");
        assert!(matches!(outcome, Outcome::Continue));
        assert_eq!(app.query(), "alpha bravo");
        assert!(!app.is_composing(), "a board paste opens no editor");

        // It APPENDS, exactly like type-to-search.
        paste(&mut app, "\ncharlie");
        assert_eq!(app.query(), "alpha bravo charlie");
    }

    /// Every OTHER keyboard owner SWALLOWS a paste, in the same precedence order the
    /// key arm uses. None of them has a text field, so the only alternatives are to
    /// act on a choice the user did not make or to leak the text into the board's
    /// query underneath an overlay that hides it — both worse than nothing.
    #[test]
    fn a_paste_is_swallowed_while_an_overlay_owns_the_keyboard() {
        // Modal: the running-session Attach/Fork/Cancel choice.
        let mut app = app_with("live-1", Some("background"));
        press(&mut app, KeyCode::Enter);
        assert!(app.modal.is_some(), "Enter on a live row opens the choice");
        assert!(matches!(paste(&mut app, "junk\ntext"), Outcome::Continue));
        assert!(app.modal.is_some(), "a paste must not resolve the modal");
        assert!(app.query().is_empty(), "and must not reach the query");

        // Leader chord: `Ctrl-X` is armed and still waiting for its KEY.
        let mut app = app_with("idle", None);
        press_ctrl(&mut app, KeyCode::Char('x'));
        assert!(app.pending_chord);
        assert!(matches!(paste(&mut app, "junk\ntext"), Outcome::Continue));
        assert!(
            app.pending_chord,
            "a paste carries no chord completion, so the chord keeps waiting"
        );
        assert!(app.query().is_empty());

        // Stop confirmation (Ctrl-R on a `needs input` agent).
        let mut app = App::new(vec![session("w")], Scope::All, PathBuf::from("/tmp"));
        let mut waiting = HashMap::new();
        waiting.insert(
            "w".to_string(),
            ReportedAgent {
                kind: "background".to_string(),
                id: Some("job-w".to_string()),
                state: Some("blocked".to_string()),
                status: None,
                pid: None,
                started_at_ms: None,
            },
        );
        app.set_live_probe(move || waiting.clone());
        press_ctrl(&mut app, KeyCode::Char('r'));
        assert!(app.pending_stop.is_some());
        assert!(matches!(paste(&mut app, "junk\ntext"), Outcome::Continue));
        assert!(app.pending_stop.is_some(), "the confirmation still stands");
        assert!(!app.is_composing(), "a paste must not confirm into compose");
        assert!(app.query().is_empty());

        // Interrupt confirmation (Ctrl-K on a `working` agent).
        let mut app = App::new(vec![session("w")], Scope::All, PathBuf::from("/tmp"));
        let mut working = HashMap::new();
        working.insert(
            "w".to_string(),
            ReportedAgent {
                kind: "background".to_string(),
                id: Some("job-z".to_string()),
                state: Some("working".to_string()),
                status: None,
                pid: None,
                started_at_ms: None,
            },
        );
        app.set_live_probe(move || working.clone());
        press_ctrl(&mut app, KeyCode::Char('k'));
        assert!(app.pending_interrupt.is_some());
        assert!(matches!(paste(&mut app, "junk\ntext"), Outcome::Continue));
        assert!(
            app.pending_interrupt.is_some(),
            "the confirmation still stands"
        );
        assert!(app.query().is_empty());
    }

    /// Every line ending collapses to `\n` before a paste is used anywhere: a CRLF
    /// pair becomes ONE newline, and a LONE CR — the classic embedded-newline form
    /// inside a bracketed paste — becomes one too. Left alone, a stray `\r` is an
    /// invisible control char in the draft and an unmatchable byte in the query.
    #[test]
    fn accept_paste_normalizes_every_line_ending_to_lf() {
        let accepted = accept_paste("crlf\r\ncr\rlf\ntail");
        assert_eq!(accepted.text, "crlf\ncr\nlf\ntail");
        assert!(!accepted.truncated);

        // A CRLF is ONE newline, never two.
        assert_eq!(accept_paste("a\r\n\r\nb").text, "a\n\nb");
        // A trailing CR still normalizes (nothing follows it to pair with).
        assert_eq!(accept_paste("a\r").text, "a\n");
    }

    /// The cap counts CHARACTERS, so truncation can never split a UTF-8 codepoint —
    /// the panic a naive byte slice would take on multibyte text. A paste of exactly
    /// the cap is not flagged truncated; one char more is.
    #[test]
    fn accept_paste_caps_by_chars_without_splitting_a_codepoint() {
        // 3-byte chars, so a byte-indexed cap would land mid-codepoint.
        let at_cap: String = "☃".repeat(PASTE_MAX_CHARS);
        let accepted = accept_paste(&at_cap);
        assert_eq!(accepted.text.chars().count(), PASTE_MAX_CHARS);
        assert!(
            !accepted.truncated,
            "a paste of exactly the cap loses nothing"
        );

        let over_cap = format!("{at_cap}☃tail");
        let accepted = accept_paste(&over_cap);
        assert!(accepted.truncated, "one char past the cap is a truncation");
        assert_eq!(accepted.text.chars().count(), PASTE_MAX_CHARS);
        assert_eq!(
            accepted.text.len(),
            PASTE_MAX_CHARS * 3,
            "the cut lands on a char boundary, not at byte PASTE_MAX_CHARS"
        );
        assert_eq!(accepted.text, at_cap);

        // A CRLF pair costs ONE char against the cap, like the newline it becomes:
        // 2 * PASTE_MAX_CHARS raw chars normalize to exactly the cap and fit.
        let crlfs = "\r\n".repeat(PASTE_MAX_CHARS);
        let accepted = accept_paste(&crlfs);
        assert!(
            !accepted.truncated,
            "the cap counts NORMALIZED chars, so a CRLF pair costs one"
        );
        assert_eq!(accepted.text, "\n".repeat(PASTE_MAX_CHARS));
    }

    /// The board query is one line, so newlines flatten to spaces.
    #[test]
    fn flatten_for_query_turns_newlines_into_spaces() {
        assert_eq!(flatten_for_query("alpha\nbravo"), "alpha bravo");
        assert_eq!(flatten_for_query("no newlines"), "no newlines");
        // Runs of newlines become runs of spaces; `search::gate_atoms` drops the
        // empty atoms between them, so this needs no collapsing of its own.
        assert_eq!(flatten_for_query("a\n\n\nb"), "a   b");
    }

    /// A pasted multi-line snippet still finds the session it was copied FROM,
    /// now that the content AND is bounded to a proximity window.
    ///
    /// This is the path the window's proportional half exists for. The paste
    /// flattens to a one-line query, `search::gate_atoms` makes every word its own
    /// atom, and a dozen atoms that must co-occur is exactly the shape a fixed
    /// window would break — so the window grows with the query instead. The
    /// negative half is what proves it is still a window at all.
    #[test]
    fn a_flattened_multi_line_paste_still_matches_the_text_it_came_from() {
        let pasted = "the deployment pipeline failed while building the release candidate image\n\
             on the long-lived integration branch the nightly smoke suite also targets\n\
             right after the database migration step rewrote the session index table";
        let query = flatten_for_query(pasted);
        assert!(!query.contains('\n'), "the board query is one line");
        // The transcript says those words, but NOT byte for byte: the content
        // index interleaves what the user selected with text they did not, so the
        // run is wider than the query. That is the shape the window's
        // proportional half exists for -- a fixed floor would not reach across it.
        let mut said = session("said");
        said.content_index = query
            .split_whitespace()
            .map(|word| format!("{word} (noted) "))
            .collect();
        assert_eq!(
            filter(
                &query,
                std::slice::from_ref(&said),
                SearchMode::NameAndContent
            ),
            vec![0],
            "a remembered snippet must still find the session it was said in"
        );

        // The SAME words, scattered across a transcript rather than said together,
        // are a coincidence: the window is still doing its job.
        let mut scattered = session("scattered");
        scattered.content_index = query
            .split_whitespace()
            .map(|word| format!("{word}{}", "x".repeat(5_000)))
            .collect();
        assert!(
            filter(
                &query,
                std::slice::from_ref(&scattered),
                SearchMode::NameAndContent
            )
            .is_empty(),
            "the same words spread across a whole transcript must not match"
        );
    }

    /// A CR-only paste reaches the DRAFT as real newlines. `TextArea::insert_str`
    /// splits on `\n` and strips a trailing `\r` per line, so it handles CRLF by
    /// itself — a lone CR it does NOT, and this is what normalizing before the
    /// insert buys.
    #[test]
    fn pasting_cr_line_endings_becomes_real_newlines_in_the_draft() {
        let mut app = app_with("idle", None);
        press_ctrl(&mut app, KeyCode::Char('r'));
        paste(&mut app, "first\rsecond\r\nthird");
        assert_eq!(draft_text(&app), "first\nsecond\nthird");
    }

    /// An over-long paste is TRUNCATED rather than rejected — the head still lands —
    /// and never silently: the status line says the tail was dropped, on both
    /// destinations.
    #[test]
    fn an_over_long_paste_is_truncated_and_says_so() {
        let huge = "x".repeat(PASTE_MAX_CHARS + 100);

        // Into the draft.
        let mut app = app_with("idle", None);
        press_ctrl(&mut app, KeyCode::Char('r'));
        paste(&mut app, &huge);
        assert_eq!(
            draft_text(&app).chars().count(),
            PASTE_MAX_CHARS,
            "the head still lands — truncate, never reject"
        );
        assert_eq!(
            app.status.as_deref(),
            Some(paste_truncated_status().as_str()),
            "a shortened paste must not be silent"
        );

        // Into the board query.
        let mut app = app_with("idle", None);
        paste(&mut app, &huge);
        assert_eq!(app.query().chars().count(), PASTE_MAX_CHARS);
        assert_eq!(
            app.status.as_deref(),
            Some(paste_truncated_status().as_str())
        );

        // A paste WITHIN the cap sets no status at all.
        let mut app = app_with("idle", None);
        paste(&mut app, "small");
        assert_eq!(app.status, None);
    }

    /// The paste-too-long nudge is a transient confirmation, not a sticky refusal.
    /// It expires after `STATUS_DWELL_TICKS` ticks so it does not squat on the
    /// keymap row; a sticky status would survive the same dwell window.
    #[test]
    fn paste_too_long_nudge_expires_after_dwell() {
        let huge = "x".repeat(PASTE_MAX_CHARS + 100);
        let mut app = app_with("idle", None);
        paste(&mut app, &huge);

        assert_eq!(
            app.status.as_deref(),
            Some(paste_truncated_status().as_str()),
            "the nudge appears immediately"
        );
        assert_eq!(
            app.status_ttl,
            Some(STATUS_DWELL_TICKS),
            "the nudge is transient, not sticky"
        );

        // Drain the dwell window through the event loop (not direct tick_status),
        // proving the dispatch wiring ages the paste nudge.
        for _ in 0..STATUS_DWELL_TICKS {
            handle_event(&mut app, AppEvent::Tick, &mut store_at(Path::new("/tmp")));
        }

        assert_eq!(
            app.status, None,
            "the paste nudge must clear after STATUS_DWELL_TICKS ticks"
        );
        assert_eq!(app.status_ttl, None);
    }

    /// The truncation status must name the CAP, so "some of it was dropped" becomes
    /// "here is exactly how much landed" — the number is the whole point of the
    /// message.
    #[test]
    fn the_truncation_status_names_the_cap() {
        let status = paste_truncated_status();
        assert!(
            status.contains(&PASTE_MAX_CHARS.to_string()),
            "the status must state how much was kept, got {status:?}"
        );
    }

    /// The caret MOVES while composing: an arrow key repositions the cursor, so a
    /// following insert lands there rather than at the end. This pins the fix for
    /// forwarding via the editor's FULL `input` handler — `input_without_shortcuts`
    /// drops cursor movement, so with it the caret is stuck and this reads "abX".
    #[test]
    fn composing_arrow_keys_move_the_caret() {
        let mut app = app_with("idle", None);
        press_ctrl(&mut app, KeyCode::Char('r'));
        press(&mut app, KeyCode::Char('a'));
        press(&mut app, KeyCode::Char('b'));
        press(&mut app, KeyCode::Left); // caret between a and b
        press(&mut app, KeyCode::Char('X'));
        let text = app
            .compose
            .as_ref()
            .expect("still composing")
            .textarea
            .lines()
            .join("\n");
        assert_eq!(
            text, "aXb",
            "Left must move the caret so the insert lands between a and b"
        );
    }

    /// A non-empty compose Send re-reads the authoritative id from the file, builds
    /// the send argv, marks the send in flight, closes compose, and returns
    /// `Outcome::Send` for the driver to spawn — the board never tears down.
    #[test]
    fn sending_a_compose_returns_a_send_request_and_clears_compose() {
        let (session, dir) = resumable_session_for_send();
        let mut app = App::new(vec![session], Scope::All, dir.clone());
        seed_live(&mut app, &[]);
        press_ctrl(&mut app, KeyCode::Char('r'));
        press(&mut app, KeyCode::Char('h'));
        press(&mut app, KeyCode::Char('i'));

        let outcome = press(&mut app, KeyCode::Enter);
        let Outcome::Send(req) = outcome else {
            panic!("Send must escalate to Outcome::Send");
        };
        assert_eq!(req.session_id, "sess-send-e2e");
        assert_eq!(
            req.argv.join(" "),
            "claude -p -r sess-send-e2e --output-format json hi"
        );
        assert_eq!(req.cwd, dir, "the child runs in the authoritative cwd");
        assert!(!app.is_composing(), "compose closes on send");
        assert!(
            app.sending_to("sess-send-e2e").is_some(),
            "the send is marked in flight at its real home (the preview pane)"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `stop_job` the reply gate resolved must reach the EMITTED
    /// `SendRequest`, not merely the compose target: the request is the only thing
    /// the driver ever sees, and `send::spawn_send` runs `claude stop <job-id>`
    /// from it to deregister the held job so `-p -r` can reclaim the session.
    /// Dropping it on the way out would leave a stop-then-reply send silently
    /// racing a still-registered job. Both directions are pinned end to end, so
    /// neither a lost id nor an invented one passes.
    #[test]
    fn a_reply_carries_its_stop_job_into_the_emitted_request() {
        let (session, dir) = resumable_session_for_send();
        let id = session.session_id.clone();

        // A `done` background agent: Ctrl-R composes straight away, in
        // stop-then-reply mode, so the send must carry that job id.
        let mut app = App::new(vec![session.clone()], Scope::All, dir.clone());
        let mut live = HashMap::new();
        live.insert(
            id.clone(),
            ReportedAgent {
                kind: "background".to_string(),
                id: Some("job-e2e".to_string()),
                state: Some("done".to_string()),
                status: None,
                pid: None,
                started_at_ms: None,
            },
        );
        app.set_live_probe(move || live.clone());
        press_ctrl(&mut app, KeyCode::Char('r'));
        assert_eq!(composing_stop_job(&app), Some("job-e2e"));
        type_into_draft(&mut app, "hi");
        let Outcome::Send(req) = press(&mut app, KeyCode::Enter) else {
            panic!("a held reply must still escalate to Outcome::Send");
        };
        assert_eq!(
            req.stop_job.as_deref(),
            Some("job-e2e"),
            "the request must carry the job the driver has to stop first"
        );

        // The other direction: a plain in-place reply stops nothing, so an
        // unconditional stop id would be just as wrong as a dropped one.
        let mut app = App::new(vec![session], Scope::All, dir.clone());
        seed_live(&mut app, &[]);
        press_ctrl(&mut app, KeyCode::Char('r'));
        assert_eq!(composing_stop_job(&app), None);
        type_into_draft(&mut app, "hi");
        let Outcome::Send(req) = press(&mut app, KeyCode::Enter) else {
            panic!("a plain reply must escalate to Outcome::Send");
        };
        assert_eq!(
            req.stop_job, None,
            "a reply to an unheld session must stop nothing"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An empty / whitespace Send is a no-op: compose stays open with a nudge, and
    /// nothing is dispatched.
    #[test]
    fn sending_an_empty_compose_keeps_composing_and_sends_nothing() {
        let mut app = app_with("idle", None);
        press_ctrl(&mut app, KeyCode::Char('r'));
        let outcome = press(&mut app, KeyCode::Enter);
        assert!(
            matches!(outcome, Outcome::Continue),
            "an empty send must not dispatch"
        );
        assert!(app.is_composing(), "compose stays open on an empty send");
    }

    /// While a quick reply is in flight, `Ctrl-R` is refused AT THE KEYPRESS on that
    /// reply's OWN row, and only there. Another row opens its compose box exactly as
    /// if nothing were in flight: its reply gate spends its probe and refuses
    /// nothing. On the in-flight row the refusal comes before the probe
    /// `send::reply_gate` reads, so no compose box opens (nothing typed can be
    /// thrown away) and no probe is spent. That refusal speaks about this session
    /// and is sticky like every refusal.
    ///
    /// Nothing here can dispatch: the reply in flight is stated, not sent, the probe
    /// is seeded, and the compose box the other row opens is cancelled unsent.
    #[test]
    fn ctrl_r_refuses_only_the_session_whose_reply_is_in_flight() {
        let mut app = App::new(
            vec![session("first"), session("second")],
            Scope::All,
            PathBuf::from("/tmp"),
        );
        let probes = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let counter = std::rc::Rc::clone(&probes);
        app.set_live_probe(move || {
            counter.set(counter.get() + 1);
            HashMap::new() // claude holds nothing: only the in-flight reply refuses
        });
        app.sending = vec![replying_to("first")];

        // Another row: `first`'s reply in flight is none of its business.
        press(&mut app, KeyCode::Down);
        assert_eq!(app.selected.as_deref(), Some("second"));
        app.status = None;
        let outcome = press_ctrl(&mut app, KeyCode::Char('r'));
        assert!(
            matches!(outcome, Outcome::Continue),
            "second: opening compose escalates nothing"
        );
        assert!(
            app.is_composing(),
            "second: another session's reply in flight must not refuse this one"
        );
        assert_eq!(
            probes.get(),
            1,
            "second: its reply gate asked claude as usual"
        );
        assert_eq!(app.status, None, "second: nothing was refused");
        press(&mut app, KeyCode::Esc); // cancel the draft unsent
        assert!(!app.is_composing(), "precondition: back on the board");

        // The in-flight row itself: refused before the probe.
        press(&mut app, KeyCode::Up);
        assert_eq!(app.selected.as_deref(), Some("first"));
        app.status = None; // the press must set its own refusal
        let outcome = press_ctrl(&mut app, KeyCode::Char('r'));
        assert!(
            matches!(outcome, Outcome::Continue),
            "first: a refusal escalates nothing"
        );
        assert!(
            !app.is_composing() && app.pending_stop.is_none(),
            "first: nothing may open for a reply that cannot be sent"
        );
        assert_eq!(
            probes.get(),
            1,
            "first: the refusal comes before the probe, so none is spent"
        );
        let status = app
            .status
            .clone()
            .expect("the refusal is on the status line");
        assert_eq!(
            status,
            send::SEND_IN_FLIGHT_REFUSED,
            "first: refused in words about this session"
        );
        for _ in 0..=STATUS_DWELL_TICKS {
            handle_event(&mut app, AppEvent::Tick, &mut store_at(Path::new("/tmp")));
        }
        assert_eq!(
            app.status.as_deref(),
            Some(status.as_str()),
            "first: a refusal is sticky, so it must outlive the dwell"
        );
    }

    /// The first row of [`a_reply_in_flight_then_a_second_attempt`]'s board: the
    /// session whose reply is still in flight.
    const IN_FLIGHT_FIRST: &str = "sess-inflight-a";

    /// The second row of that board: the session a second reply is attempted on.
    const IN_FLIGHT_SECOND: &str = "sess-inflight-b";

    /// A board over TWO real resumable sessions that claude holds neither of, with a
    /// quick reply DISPATCHED to the first and a second one then attempted on the
    /// other row while the first is still in flight. No `SendFinished` is ever
    /// delivered, so the first send never ends.
    ///
    /// The second attempt is made the way a user makes it: `Ctrl-R`, then — only if
    /// a compose box opened — type and `Enter`. The `if` is what lets one driver
    /// serve every board. One that refuses at `Ctrl-R` has no draft to type into
    /// (the keys would land in the search query), and one that does not dispatches
    /// the second reply. On a board that tracked ONE send that dispatch reproduced
    /// the overwrite end to end; on this one, which keys its sends by session, it
    /// puts the second reply's entry beside the first. Neither `Outcome::Send` is
    /// executed: the test holds the value and the driver never sees it, so no
    /// `claude` is spawned.
    ///
    /// Both files live in a temp dir this helper creates. Returns the board and
    /// that dir, for the caller to remove.
    fn a_reply_in_flight_then_a_second_attempt() -> (App, PathBuf) {
        let dir = unique_temp_dir("reply-in-flight");
        let first = resumable_session_in(&dir, IN_FLIGHT_FIRST, "the first session");
        let second = resumable_session_in(&dir, IN_FLIGHT_SECOND, "the second session");
        let mut app = App::new(vec![first, second], Scope::All, dir.clone());
        seed_live(&mut app, &[]); // claude holds neither: each reply composes at once

        // One group, no timestamps: the rows sort by id, so the first is selected.
        assert_eq!(app.selected.as_deref(), Some(IN_FLIGHT_FIRST));
        press_ctrl(&mut app, KeyCode::Char('r'));
        type_into_draft(&mut app, "first");
        let Outcome::Send(req) = press(&mut app, KeyCode::Enter) else {
            panic!("the first reply must dispatch");
        };
        assert_eq!(req.session_id, IN_FLIGHT_FIRST);
        assert!(
            app.sending_to(IN_FLIGHT_FIRST).is_some(),
            "precondition: the first reply is in flight"
        );

        press(&mut app, KeyCode::Down);
        assert_eq!(app.selected.as_deref(), Some(IN_FLIGHT_SECOND));
        press_ctrl(&mut app, KeyCode::Char('r'));
        if app.is_composing() {
            type_into_draft(&mut app, "second");
            let _ = press(&mut app, KeyCode::Enter);
        }
        (app, dir)
    }

    /// Task 8.4, the regression: a quick reply still IN FLIGHT stays tracked when a
    /// second reply is sent to another row, and the second is tracked beside it.
    ///
    /// When `App::sending` held ONE send, `Ctrl-R` on another row opened a compose
    /// box and its `Enter` overwrote that slot. The first reply's `claude -p` child
    /// was still running and still appending to its transcript, but the board no
    /// longer tracked it anywhere. Refusing `Ctrl-R` on every row closed that hole
    /// by sending one reply at a time; keying the entries by session closes it
    /// while both replies run.
    #[test]
    fn a_second_reply_to_another_session_is_tracked_beside_the_first() {
        let (app, dir) = a_reply_in_flight_then_a_second_attempt();
        let message_to = |id: &str| app.sending_to(id).map(|sending| sending.message.clone());
        let (first, second) = (message_to(IN_FLIGHT_FIRST), message_to(IN_FLIGHT_SECOND));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            first.as_deref(),
            Some("first"),
            "the reply still in flight must stay tracked"
        );
        assert_eq!(
            second.as_deref(),
            Some("second"),
            "the reply to the other row went out and is tracked too"
        );
        assert_eq!(app.sending.len(), 2, "one entry per session, no more");
    }

    /// Task 8.5, the consequence and the reason this is a fix: the hard-delete
    /// writer guard stays ARMED on the session whose reply is still landing.
    ///
    /// Until the reply child registers with claude, claude's active list does not
    /// show the target. A held job is `claude stop`ped before `claude -p -r` runs,
    /// and an unheld session was never listed. Once registered, the child was
    /// reported as `interactive` at 2.1.278, but no `-p` child was sampled at
    /// 2.1.280: claude's probe cannot be relied on to see it. So the one fact
    /// that reliably stands between `Ctrl-X d` and an unlink of a transcript
    /// snapback's own child is appending to is `App::sending_to`. With it
    /// overwritten, `delete::can_delete_target` judges by the probe alone, and
    /// before the child registers the probe reports nothing, as it does here.
    ///
    /// This asks `can_delete_target` exactly what `confirm_delete` asks it: the
    /// board's own probe and in-flight answer for that id. It does NOT drive the
    /// confirm, so no break of the guard under test can ever reach
    /// `delete::remove`.
    #[test]
    fn a_second_reply_leaves_the_delete_guard_armed_on_the_first() {
        let (app, dir) = a_reply_in_flight_then_a_second_attempt();
        let live = app.live_agents_now();
        let verdict = delete::can_delete_target(
            live.get(IN_FLIGHT_FIRST),
            app.sending_to(IN_FLIGHT_FIRST).is_some(),
        );
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            !live.contains_key(IN_FLIGHT_FIRST),
            "precondition: claude reports the first session as nothing at all"
        );
        assert_eq!(
            verdict,
            Err(delete::DELETE_SENDING_REFUSAL.to_string()),
            "a transcript snapback is still replying to must not be deletable"
        );
    }

    /// Task 5.4: a finished send for the SELECTED session re-anchors the preview to
    /// the newest turn, and that survives the `SessionsChanged` reload that brings
    /// the reply in — so the reply lands in view. It also shows the mapped result.
    #[test]
    fn a_finished_send_reanchors_the_previewed_session_and_survives_reload() {
        let mut app = app_with("s", None);
        // The user scrolled up while the send was in flight (follow-bottom off).
        app.preview_top();
        assert!(!app.preview_follow_bottom);

        handle_event(
            &mut app,
            AppEvent::SendFinished {
                session_id: "s".to_string(),
                status: "sent — $0.0136".to_string(),
                success: true,
            },
            &mut store_at(Path::new("/tmp")),
        );
        assert!(
            app.preview_follow_bottom,
            "a finished send re-anchors the previewed row to the newest turn"
        );
        assert_eq!(
            app.status.as_deref(),
            Some("sent — $0.0136"),
            "the mapped result shows on the status line"
        );

        // A reload that preserves the selection must keep the re-anchor, so the
        // reply renders bottom-anchored.
        app.apply_sessions(vec![session("s")]);
        assert!(
            app.preview_follow_bottom,
            "the reload must not drop the re-anchor"
        );
    }

    /// A finished send for an OFF-SCREEN session shows its status but leaves the
    /// viewed transcript's scroll alone.
    #[test]
    fn a_finished_send_for_another_session_does_not_touch_the_view() {
        let mut app = App::new(
            vec![session("a"), session("b")],
            Scope::All,
            PathBuf::from("/tmp"),
        );
        app.preview_top(); // follow-bottom off, viewing "a"
        let selected = app.selected.clone();

        handle_event(
            &mut app,
            AppEvent::SendFinished {
                session_id: "b".to_string(),
                status: "sent".to_string(),
                success: true,
            },
            &mut store_at(Path::new("/tmp")),
        );
        assert_eq!(app.selected, selected, "selection is unchanged");
        assert!(
            !app.preview_follow_bottom,
            "a send to an off-screen session must not re-anchor the view"
        );
        assert_eq!(app.status.as_deref(), Some("sent"));
    }

    /// A finished reply clears ONLY its own session's entry. With replies in flight
    /// to `a` and `b`, `a`'s `SendFinished` ends `a`'s tracking and leaves `b`'s
    /// standing, because `b`'s `claude -p` child is still writing: its echo stays up
    /// and the hard-delete guard stays armed on its transcript, asked exactly what
    /// `confirm_delete` asks it with claude reporting nothing.
    #[test]
    fn a_finished_reply_leaves_another_sessions_reply_tracked() {
        let mut app = App::new(
            vec![session("a"), session("b")],
            Scope::All,
            PathBuf::from("/tmp"),
        );
        app.sending = vec![replying_to("a"), replying_to("b")];

        handle_event(
            &mut app,
            AppEvent::SendFinished {
                session_id: "a".to_string(),
                status: "sent".to_string(),
                success: true,
            },
            &mut store_at(Path::new("/tmp")),
        );
        assert!(
            app.sending_to("a").is_none(),
            "a's reply finished, so its entry clears"
        );
        assert_eq!(
            app.sending_to("b").map(|sending| sending.message.as_str()),
            Some("still landing"),
            "b's reply is still running, so a's completion must leave it tracked"
        );
        assert_eq!(
            delete::can_delete_target(None, app.sending_to("b").is_some()),
            Err(delete::DELETE_SENDING_REFUSAL.to_string()),
            "b's transcript must stay undeletable while its reply lands"
        );
    }

    // --- a completion that outlived its board session ------------------------
    //
    // Each test below queues a completion the way a send thread does once its
    // board has gone: through the app's handle, over a real channel whose
    // receiver was dropped. None of them spawns a thread or a `claude` child.

    /// Queue `session_id`'s completion as the send thread would after a hand-off
    /// ended the board that dispatched it.
    fn queue_undelivered(app: &App, session_id: &str, status: &str, success: bool) {
        let (tx, rx) = std::sync::mpsc::channel();
        drop(rx); // the board that dispatched the reply has gone
        app.undelivered_handle().deliver(
            &tx,
            AppEvent::SendFinished {
                session_id: session_id.to_string(),
                status: status.to_string(),
                success,
            },
        );
    }

    /// A quick reply in flight to `session_id`, stated rather than dispatched.
    fn replying_to(session_id: &str) -> crate::tui::app::Sending {
        crate::tui::app::Sending {
            session_id: session_id.to_string(),
            message: "still landing".to_string(),
            baseline_msg_count: 0,
        }
    }

    fn tick(app: &mut App) -> Outcome {
        handle_event(app, AppEvent::Tick, &mut store_at(Path::new("/tmp")))
    }

    /// Task 10.4: the next board's `Tick` hands a queued completion to the SAME arm
    /// a live one reaches. It clears the reply it belongs to, re-anchors the
    /// preview on the selected row, and leaves the queue empty. Its confirmation
    /// then dwells for the full `STATUS_DWELL_TICKS`, because the replay runs
    /// AFTER that tick has aged the status line.
    #[test]
    fn a_queued_completion_is_replayed_on_the_next_tick_and_clears_its_reply() {
        let mut app = app_with("s", None);
        app.sending = vec![replying_to("s")];
        app.preview_top(); // the reader scrolled up while the reply was in flight
        queue_undelivered(&app, "s", "sent — $0.0136", true);
        assert!(
            app.sending_to("s").is_some(),
            "precondition: nothing is replayed before the tick"
        );

        let outcome = tick(&mut app);
        assert!(
            matches!(outcome, Outcome::Continue),
            "a tick never ends the board"
        );
        assert!(
            app.sending.is_empty(),
            "the replayed completion clears the reply it belongs to"
        );
        assert!(
            app.preview_follow_bottom,
            "a replayed completion for the selected row re-anchors it like a live one"
        );
        assert!(
            app.take_undelivered().is_empty(),
            "the replay empties the queue"
        );
        for i in 0..STATUS_DWELL_TICKS {
            assert_eq!(
                app.status.as_deref(),
                Some("sent — $0.0136"),
                "a replayed confirmation gets its full dwell: tick {i}"
            );
            tick(&mut app);
        }
        assert!(
            app.status.is_none(),
            "a replayed confirmation expires after STATUS_DWELL_TICKS, like a live one"
        );
    }

    /// Task 10.4: a replayed FAILURE clears its reply too (the child has finished
    /// either way) and stays sticky past the dwell, exactly as a live one does.
    #[test]
    fn a_queued_failed_completion_is_replayed_sticky() {
        let mut app = app_with("s", None);
        app.sending = vec![replying_to("s")];
        queue_undelivered(&app, "s", "send failed: boom", false);

        tick(&mut app);
        assert!(
            app.sending.is_empty(),
            "a failed reply has finished too, so its entry clears"
        );
        for i in 0..=STATUS_DWELL_TICKS {
            assert_eq!(
                app.status.as_deref(),
                Some("send failed: boom"),
                "a replayed failure is sticky: tick {i}"
            );
            tick(&mut app);
        }
    }

    /// Task 10.4: a queued completion for ANOTHER session is still replayed (its
    /// status shows, and the queue empties), but it leaves the reply in flight
    /// alone. Like a live event, the replay reaches `App::clear_sending`, which
    /// removes only the entry for the completion's own session.
    #[test]
    fn a_queued_completion_for_another_session_leaves_the_reply_in_flight_alone() {
        let mut app = App::new(
            vec![session("a"), session("b")],
            Scope::All,
            PathBuf::from("/tmp"),
        );
        app.sending = vec![replying_to("a")];
        queue_undelivered(&app, "b", "sent", true);

        tick(&mut app);
        assert_eq!(
            app.sending_to("a").map(|sending| sending.message.as_str()),
            Some("still landing"),
            "a completion for another session must not clear this reply"
        );
        assert_eq!(app.status.as_deref(), Some("sent"), "it was still replayed");
        assert!(
            app.take_undelivered().is_empty(),
            "the replay empties the queue"
        );
    }

    /// Task 10.4 / 10.6: the board-entry call is the SAME replay, and it works
    /// with no tick at all. A reply that finished while the hand-off's child held
    /// the terminal is settled before the new board's first draw.
    #[test]
    fn the_board_entry_replay_settles_a_queued_completion_before_any_tick() {
        let mut app = app_with("s", None);
        app.sending = vec![replying_to("s")];
        queue_undelivered(&app, "s", "sent", true);

        replay_undelivered(&mut app, &mut store_at(Path::new("/tmp")));
        assert_eq!(app.tick, 0, "no tick was spent");
        assert!(
            app.sending.is_empty(),
            "the entry replay clears the reply it belongs to"
        );
        assert_eq!(app.status.as_deref(), Some("sent"));
        assert!(
            app.take_undelivered().is_empty(),
            "the entry replay empties the queue"
        );
    }

    /// Task 10.5, the review's MAJOR finding pinned end to end: a quick reply is
    /// dispatched, then a hand-off (`Enter`) ends that board session while the
    /// reply is still running, and the reply finishes with no board up to read it.
    ///
    /// Until the next board's tick its entry must still stand, and with it the
    /// hard-delete guard, because nothing has been delivered yet. Clearing it early
    /// would reopen `Ctrl-X d` on a transcript the child may still be writing. After
    /// that tick the entry is gone, and `Ctrl-R` on that session opens a compose
    /// box again instead of refusing it until restart.
    ///
    /// Nothing is executed: both outcomes are held, never handed to a driver, the
    /// probe is seeded, and the delete guard is asked exactly what `confirm_delete`
    /// asks it without driving the confirm. Both session files live in a temp dir
    /// the test creates and removes before asserting.
    #[test]
    fn a_reply_that_finishes_across_a_hand_off_is_settled_by_the_next_board() {
        let dir = unique_temp_dir("reply-across-hand-off");
        let first = resumable_session_in(&dir, IN_FLIGHT_FIRST, "the first session");
        let second = resumable_session_in(&dir, IN_FLIGHT_SECOND, "the second session");
        let mut app = App::new(vec![first, second], Scope::All, dir.clone());
        seed_live(&mut app, &[]); // claude holds neither session

        // 1. The reply, dispatched by keypress. The `Send` is held, never executed.
        let selected_at_start = app.selected.clone();
        press_ctrl(&mut app, KeyCode::Char('r'));
        type_into_draft(&mut app, "first");
        let dispatched = press(&mut app, KeyCode::Enter);

        // 2. A hand-off ends this board session. The `Resume` is held, never launched.
        let handoff = press(&mut app, KeyCode::Enter);

        // 3. The reply finishes with no board up: its send finds the receiver gone.
        queue_undelivered(&app, IN_FLIGHT_FIRST, "sent", true);

        // 4. Before the next board's tick, the reply is still the tracked send.
        let in_flight_before = app.sending_to(IN_FLIGHT_FIRST).is_some();
        let live = app.live_agents_now();
        let verdict_before = delete::can_delete_target(
            live.get(IN_FLIGHT_FIRST),
            app.sending_to(IN_FLIGHT_FIRST).is_some(),
        );

        // 5. The next board's tick, then `Ctrl-R` on the same row.
        tick(&mut app);
        let in_flight_after = !app.sending.is_empty();
        let reopened = press_ctrl(&mut app, KeyCode::Char('r'));
        let composing = app.is_composing();
        let status = app.status.clone();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(selected_at_start.as_deref(), Some(IN_FLIGHT_FIRST));
        assert!(
            matches!(&dispatched, Outcome::Send(req) if req.session_id == IN_FLIGHT_FIRST),
            "precondition: the reply was dispatched"
        );
        assert!(
            matches!(handoff, Outcome::Resume(_)) && handoff.ends_board_session(),
            "precondition: Enter handed off and ended the board session"
        );
        assert!(
            !live.contains_key(IN_FLIGHT_FIRST),
            "precondition: claude reports the session as nothing at all"
        );
        assert!(
            in_flight_before,
            "the entry stands until a completion is delivered"
        );
        assert_eq!(
            verdict_before,
            Err(delete::DELETE_SENDING_REFUSAL.to_string()),
            "until then the transcript must not be deletable"
        );
        assert!(
            !in_flight_after,
            "the next board's tick settles the reply that finished across the hand-off"
        );
        assert!(
            matches!(reopened, Outcome::Continue) && composing,
            "Ctrl-R opens a compose box again"
        );
        assert!(
            !status
                .as_deref()
                .is_some_and(|status| status.contains(send::SEND_IN_FLIGHT_REFUSED)),
            "no reply is in flight any more, so nothing refuses: {status:?}"
        );
    }

    // --- mouse wheel: hit-test routing + preview scroll clamps -------------

    #[test]
    fn wheel_target_routes_preview_list_and_defaults_to_preview() {
        let list = Rect {
            x: 0,
            y: 5,
            width: 50,
            height: 20,
        };
        let preview = Rect {
            x: 50,
            y: 5,
            width: 40,
            height: 20,
        };
        // Inside preview -> preview; inside list -> list.
        assert_eq!(
            wheel_target(60, 10, preview, list, false),
            WheelTarget::Preview
        );
        assert_eq!(
            wheel_target(10, 10, preview, list, false),
            WheelTarget::List
        );
        // Outside both panes (e.g. the header row) -> preview default.
        assert_eq!(
            wheel_target(60, 100, preview, list, false),
            WheelTarget::Preview
        );
        // Hidden preview (empty rect): a point in the full-width list still
        // routes to the list.
        assert_eq!(
            wheel_target(10, 10, Rect::default(), list, false),
            WheelTarget::List
        );
        // Composing narrows exactly ONE of the three zones, so all three are
        // pinned here. Over the preview a notch still scrolls the transcript
        // being written to.
        assert_eq!(
            wheel_target(60, 10, preview, list, true),
            WheelTarget::Preview,
            "a draft does not take the preview's own wheel away"
        );
        // Over the LIST the notch is IGNORED — not redirected to the preview —
        // because that arm moves the SELECTION out from under the draft.
        assert_eq!(
            wheel_target(10, 10, preview, list, true),
            WheelTarget::Ignore,
            "the list is not a wheel target while a draft is open"
        );
        // Outside BOTH rects the default surface survives untouched: that zone
        // covers the bottom-bar composer, the search line and the help line, and
        // a notch over any of them must still scroll the transcript mid-draft.
        assert_eq!(
            wheel_target(60, 100, preview, list, true),
            WheelTarget::Preview,
            "outside both panes a draft changes nothing: still the preview"
        );
    }

    #[test]
    fn mouse_wheel_scrolls_the_preview_and_clamps_both_ends() {
        let mut app = app_with("s", None);
        // Route the wheel into the preview pane.
        app.list_rect = Rect {
            x: 0,
            y: 0,
            width: 50,
            height: 20,
        };
        app.preview_rect = Rect {
            x: 50,
            y: 0,
            width: 40,
            height: 20,
        };
        // Scroll down moves the offset toward newer turns, by the wheel step.
        wheel(&mut app, MouseEventKind::ScrollDown, 60, 10);
        assert_eq!(app.preview_scroll, 2);
        wheel(&mut app, MouseEventKind::ScrollDown, 60, 10);
        assert_eq!(app.preview_scroll, 4);
        // Scrolling back up saturates at 0 (no underflow below the top).
        for _ in 0..10 {
            wheel(&mut app, MouseEventKind::ScrollUp, 60, 10);
        }
        assert_eq!(
            app.preview_scroll, 0,
            "wheel-up cannot underflow past the top"
        );
        // A notch in the range a `u16` offset could not even express is an ORDINARY
        // scroll, not a saturated one: the wheel moves THROUGH it by the step.
        app.preview_scroll = 100_000;
        wheel(&mut app, MouseEventKind::ScrollDown, 60, 10);
        assert_eq!(
            app.preview_scroll, 100_002,
            "a notch past u16::MAX advances by the wheel step rather than pinning"
        );
        // Repeated down notches near the ceiling never overflow past u32::MAX.
        app.preview_scroll = u32::MAX - 1;
        wheel(&mut app, MouseEventKind::ScrollDown, 60, 10);
        assert_eq!(app.preview_scroll, u32::MAX);
        wheel(&mut app, MouseEventKind::ScrollDown, 60, 10);
        assert_eq!(
            app.preview_scroll,
            u32::MAX,
            "wheel-down saturates at u32::MAX"
        );
    }

    #[test]
    fn mouse_wheel_over_the_list_moves_the_selection() {
        let mut app = App::new(
            vec![session("a"), session("b"), session("c")],
            Scope::All,
            PathBuf::from("/tmp"),
        );
        app.list_rect = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 20,
        };
        app.preview_rect = Rect {
            x: 40,
            y: 0,
            width: 40,
            height: 20,
        };
        let first = app.selected.clone();
        wheel(&mut app, MouseEventKind::ScrollDown, 5, 5);
        assert_ne!(
            app.selected, first,
            "a wheel over the list advances the selection"
        );
        assert!(app.modal.is_none(), "a list wheel must not open an overlay");
    }

    /// An open draft takes the LIST out of the wheel's reach: a notch there does
    /// NOTHING — it neither walks the selection off the session being replied to
    /// nor is quietly redirected into the preview. The compose zone targets ONE
    /// session id, so a wheel that moved the selection would leave the user typing
    /// at a transcript that is no longer on screen. The preview keeps its own
    /// wheel throughout, which the first notch below proves.
    #[test]
    fn a_wheel_over_the_list_while_composing_does_nothing() {
        let mut app = App::new(
            vec![session("a"), session("b"), session("c")],
            Scope::All,
            PathBuf::from("/tmp"),
        );
        seed_live(&mut app, &[]);
        app.list_rect = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 20,
        };
        app.preview_rect = Rect {
            x: 40,
            y: 0,
            width: 40,
            height: 20,
        };
        // Open the quick reply through the real routing, not by poking `compose`.
        press_ctrl(&mut app, KeyCode::Char('r'));
        assert!(app.is_composing(), "Ctrl-R on an idle session composes");

        // Mid-draft the PREVIEW still takes its own notches, and this one parks
        // the scroll off zero so a redirected or reset notch below cannot hide.
        wheel(&mut app, MouseEventKind::ScrollDown, 60, 5);
        assert_eq!(
            app.preview_scroll, 2,
            "a draft does not take the preview's own wheel away"
        );
        let target = app.selected.clone();
        let status = app.status.clone();

        // A notch squarely inside the list pane.
        wheel(&mut app, MouseEventKind::ScrollDown, 5, 5);

        assert_eq!(
            app.selected, target,
            "a wheel over the list must not move the selection out from under a draft"
        );
        assert_eq!(
            app.preview_scroll, 2,
            "and it must not be redirected into the preview either — it does nothing"
        );
        assert_eq!(app.status, status, "an ignored notch reports nothing");
        assert!(app.is_composing(), "and the draft is still open");

        // Only the LIST goes dead. A notch OUTSIDE both rects — where the
        // bottom-bar composer, the search line and the help line are drawn —
        // still reaches the default surface and scrolls the transcript.
        wheel(&mut app, MouseEventKind::ScrollDown, 60, 30);
        assert_eq!(
            app.preview_scroll, 4,
            "outside both panes the default surface survives the draft"
        );
    }

    #[test]
    fn mouse_wheel_is_independent_of_the_overlay_gate() {
        // Scroll must NOT route into the overlay handler: it scrolls a pane and
        // leaves the overlay untouched, even while the overlay owns the keyboard.
        let mut app = app_with("live-1", Some("background"));
        app.list_rect = Rect {
            x: 0,
            y: 0,
            width: 50,
            height: 20,
        };
        app.preview_rect = Rect {
            x: 50,
            y: 0,
            width: 40,
            height: 20,
        };
        press(&mut app, KeyCode::Enter); // open the overlay
        assert!(app.modal.is_some());
        let highlight = app.modal.as_ref().unwrap().selected;

        wheel(&mut app, MouseEventKind::ScrollDown, 60, 10);
        assert!(app.modal.is_some(), "a wheel must not dismiss the overlay");
        assert_eq!(
            app.modal.as_ref().unwrap().selected,
            highlight,
            "a wheel must not move the overlay highlight"
        );
        assert_eq!(
            app.preview_scroll, 2,
            "the wheel scrolled the preview instead"
        );
    }

    // --- preview link hit-testing across the pinned banner ------------------

    /// Board size for the link hit-tests: wide enough for a usable preview pane,
    /// short enough that `link_session`'s transcript overflows it.
    const BOARD: (u16, u16) = (100, 20);

    /// The url behind `link_session`'s one markdown link.
    const LINK_URL: &str = "https://example.com/page";

    /// A url a transcript can author in a `[label](url)` link and snapback will NOT
    /// open. A bracket target is scheme-checked nowhere on the way in, so this is the
    /// shape that reaches the opener verbatim unless something refuses it.
    const REFUSED_URL: &str = "file:///etc/passwd";

    /// Filler lines ahead of that link. Enough that the rendered transcript is
    /// TALLER than `BOARD`'s preview pane, so the default bottom anchor resolves
    /// to a NON-ZERO scroll offset — the hit-test has to survive a scrolled pane,
    /// not just a pristine one.
    const LINK_FILLER_LINES: usize = 24;

    /// An isolated temp dir for the link fixture (PATTERNS: never touch the real
    /// `~/.claude/projects`).
    fn unique_temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the unix epoch")
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "snapback-update-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// A session carrying ONE user turn whose message body is `body`, written to a
    /// real file under `dir`.
    ///
    /// A real file, not a synthetic `LinkRegion`: the hit-test resolves clicks
    /// through the SAME width-scoped preview cache the view draws from, so only a
    /// real render can prove the two agree about where the link landed. Everything but
    /// the body is fixed, so two fixtures built through here differ in exactly what
    /// their bodies differ in.
    fn link_fixture(dir: &Path, body: &str) -> Session {
        let file = dir.join("sess-link.jsonl");
        let jsonl = format!(
            concat!(
                r#"{{"type":"user","sessionId":"sess-link","cwd":"/tmp","#,
                r#""timestamp":"2026-07-01T10:00:00.000Z","#,
                r#""message":{{"role":"user","content":"{body}"}}}}"#,
                "\n",
            ),
            body = body,
        );
        std::fs::write(&file, jsonl).expect("write the link fixture");
        Session {
            file,
            session_id: "sess-link".to_string(),
            cwd: PathBuf::from("/tmp"),
            git_branch: Some("main".to_string()),
            timestamp: None,
            repo: "repo".to_string(),
            label: "link session".to_string(),
            root_uuid: None,
            msg_count: 0,
            content_index: String::new(),
            background: false,
            has_agent_name: false,
            has_agent_setting: false,
            failed_task: None,
        }
    }

    /// The sibling of [`link_session`] whose link label starts its own line, so the
    /// label occupies content column 0 — the pane's FIRST content column, one inside
    /// the preview's left border.
    ///
    /// A real file for the same reason its sibling is one: only a real render can
    /// prove the draw and the hit-test agree about where the label went. The link is
    /// its own paragraph (a blank line above it) so no soft-wrapped predecessor can
    /// push it off column 0.
    fn link_at_column_zero_session(dir: &Path) -> Session {
        let file = dir.join("sess-link-col0.jsonl");
        let mut body: String = (1..=LINK_FILLER_LINES)
            .map(|i| format!("filler line {i}\\n"))
            .collect();
        body.push_str(&format!("\\n[docs]({LINK_URL}) opens the report"));
        let jsonl = format!(
            concat!(
                r#"{{"type":"user","sessionId":"sess-link-col0","cwd":"/tmp","#,
                r#""timestamp":"2026-07-01T10:00:00.000Z","#,
                r#""message":{{"role":"user","content":"{body}"}}}}"#,
                "\n",
            ),
            body = body,
        );
        std::fs::write(&file, jsonl).expect("write the column-zero link fixture");
        let mut s = session("sess-link-col0");
        s.file = file;
        s
    }

    /// A session whose transcript overflows the preview pane and ends in ONE
    /// markdown link.
    ///
    /// The link TARGET is a parameter so the same real render can be driven with a url
    /// the opener accepts and with one it refuses. Only the url varies — the label, the
    /// filler and the session id stay put — so a difference in outcome between two such
    /// sessions can only have come from the scheme.
    fn link_session(dir: &Path, url: &str) -> Session {
        let mut body: String = (1..=LINK_FILLER_LINES)
            .map(|i| format!("filler line {i}\\n"))
            .collect();
        body.push_str(&format!("open [docs]({url}) here"));
        link_fixture(dir, &body)
    }

    /// How many markdown links the OVER-BUDGET fixture crowds onto ONE logical line.
    ///
    /// The hit-test's budget is a PRODUCT — the clicked line's SIZE times the candidate
    /// regions on it — so a fixture has to move BOTH factors to cross it and neither
    /// number means anything alone. That is also what makes a crossing fixture cheap
    /// enough to render in a unit test: each link here draws as a one-character label
    /// plus a space, so N of them both make the line about `2N` long AND put N
    /// candidates on it. The cost therefore grows as N SQUARED, and a few hundred links
    /// buy a product in the hundreds of thousands.
    ///
    /// Deliberately stated without the budget's UNIT (`view` owns whether that size is
    /// counted in wrapped rows or in bytes) and without the pane's exact width, because
    /// the crossing is never ASSUMED here: the test runs the same click against
    /// [`CROWDED_CONTROL_LINKS`] first and requires an ordinary hit. Retune the budget,
    /// its unit, the board or this number and that premise goes red, rather than the
    /// case quietly becoming vacuous.
    const CROWDED_LINE_LINKS: usize = 400;

    /// The control count for [`CROWDED_LINE_LINKS`]: the same fixture shape, comfortably
    /// WITHIN the budget, so the two runs differ in the crossing and nothing else.
    const CROWDED_CONTROL_LINKS: usize = 4;

    /// A session whose whole transcript is ONE logical line carrying `links` markdown
    /// links — all the same url, so only their COUNT varies between runs.
    ///
    /// No filler: the crowded line must be the line a click lands on, and a transcript
    /// that is nothing else cannot resolve the click to some other row by accident.
    fn crowded_link_session(dir: &Path, links: usize) -> Session {
        let body = vec![format!("[x]({LINK_URL})"); links].join(" ");
        link_fixture(dir, &body)
    }

    /// An app over [`crowded_link_session`], in `App`'s DEFAULT scroll state.
    fn crowded_link_app(dir: &Path, links: usize) -> App {
        let app = App::new(
            vec![crowded_link_session(dir, links)],
            Scope::All,
            PathBuf::from("/tmp"),
        );
        assert_eq!(app.selected.as_deref(), Some("sess-link"));
        app
    }

    /// An app over [`link_session`], optionally joined to a REPORTED agent in
    /// `state`.
    /// Left in `App`'s DEFAULT scroll state — bottom-anchored, as a user sees it.
    fn link_app(dir: &Path, agent_state: Option<&str>) -> App {
        link_app_to(dir, agent_state, LINK_URL)
    }

    /// [`link_app`] over an arbitrary link target (see [`link_session`]).
    fn link_app_to(dir: &Path, agent_state: Option<&str>, url: &str) -> App {
        let mut app = App::new(
            vec![link_session(dir, url)],
            Scope::All,
            PathBuf::from("/tmp"),
        );
        if let Some(state) = agent_state {
            let mut reported = HashMap::new();
            reported.insert(
                "sess-link".to_string(),
                ReportedAgent {
                    kind: "background".to_string(),
                    id: None,
                    state: Some(state.to_string()),
                    status: None,
                    pid: None,
                    started_at_ms: None,
                },
            );
            app.set_reported_agents(reported, None);
        }
        assert_eq!(app.selected.as_deref(), Some("sess-link"));
        app
    }

    /// Render the whole board into an in-memory terminal exactly as the real loop
    /// does. `view::render` is what writes `App::preview_rect` and the resolved
    /// `App::preview_scroll` back into the app, so the hit-tests below run against
    /// the geometry the user is actually looking at.
    fn render_board(app: &mut App) -> ratatui::buffer::Buffer {
        let (width, height) = BOARD;
        let mut terminal = Terminal::new(TestBackend::new(width, height))
            .expect("build an in-memory test terminal");
        terminal
            .draw(|frame| view::render(frame, app))
            .expect("render must not panic");
        terminal.backend().buffer().clone()
    }

    /// Where the transcript's link label was actually DRAWN, as screen
    /// `(col, row)`.
    ///
    /// Found by the UNDERLINED modifier the preview marks a link label with
    /// (`store::preview` underlines the label and hides the url), scanning the
    /// pane the view reported. Never a COMPUTED row — that would just restate the
    /// geometry under test and pass no matter what it drifted to.
    fn drawn_link_cell(buffer: &ratatui::buffer::Buffer, preview: Rect) -> (u16, u16) {
        let found = (preview.y..preview.bottom())
            .flat_map(|y| (preview.x..preview.right()).map(move |x| (x, y)))
            .find(|&(x, y)| {
                buffer
                    .cell((x, y))
                    .is_some_and(|c| c.modifier.contains(Modifier::UNDERLINED))
            });
        found.expect(
            "the fixture's link label must be drawn inside the preview pane, \
             or these tests prove nothing",
        )
    }

    /// Press, then release, the left button at `(col, row)` — a plain CLICK — and
    /// return what the RELEASE decided. Driven through the pure [`mouse_effect`], so
    /// a url it resolves is returned, never handed to a browser.
    fn click(app: &mut App, col: u16, row: u16) -> MouseEffect {
        let pressed = mouse_at(
            app,
            mouse_ev(MouseEventKind::Down(MouseButton::Left), col, row),
        );
        assert_eq!(pressed, MouseEffect::None, "a press alone must never act");
        mouse_at(
            app,
            mouse_ev(MouseEventKind::Up(MouseButton::Left), col, row),
        )
    }

    #[test]
    fn a_click_on_a_drawn_link_opens_it_for_a_banner_less_pane() {
        // No banner: the transcript owns the pane's whole inner rect, and the
        // hit-test must NOT shift by a row that was never reserved. An in-flight
        // quick reply is the one banner-less pane that still draws the transcript
        // (its inline echo turns take the banner's place).
        let dir = unique_temp_dir("link-plain");
        let mut app = link_app(&dir, None);
        app.sending = vec![crate::tui::app::Sending {
            session_id: "sess-link".to_string(),
            message: "thanks".to_string(),
            baseline_msg_count: 0,
        }];
        let buffer = render_board(&mut app);
        assert!(
            view::preview_banner(&app).is_none(),
            "an in-flight reply reserves no banner row"
        );
        assert!(
            app.preview_scroll > 0,
            "the fixture must overflow the pane, or this never tests a scrolled hit"
        );

        let (col, row) = drawn_link_cell(&buffer, app.preview_rect);
        assert_eq!(
            resolve_link_click(&mut app, col, row),
            LinkClick::Opening(LINK_URL.to_string()),
            "a click on the cell the link was DRAWN on must resolve to its url"
        );
        assert_eq!(
            click(&mut app, col, row),
            MouseEffect::OpenLink(LINK_URL.to_string()),
            "a click (press + release) on the cell the link was DRAWN on must open \
             its url"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_click_on_a_drawn_link_opens_it_beneath_an_unreported_sessions_pinned_row() {
        // A session claude does NOT report pins the row too — it keys on the
        // selection — so its transcript starts one row lower, and the hit-test
        // must follow it there exactly as it does for a reported one.
        let dir = unique_temp_dir("link-unreported");
        let mut app = link_app(&dir, None);
        let buffer = render_board(&mut app);
        assert!(
            app.reported_agent("sess-link").is_none(),
            "the session must really be unreported, or this is the reported case"
        );
        assert!(
            view::preview_banner(&app).is_some(),
            "an unreported selected session must pin the row"
        );
        assert!(
            app.preview_scroll > 0,
            "the fixture must overflow the pane, or this never tests a scrolled hit"
        );

        let (col, row) = drawn_link_cell(&buffer, app.preview_rect);
        assert_eq!(
            resolve_link_click(&mut app, col, row),
            LinkClick::Opening(LINK_URL.to_string()),
            "a click on the cell the link was DRAWN on must resolve to its url \
             even though the pinned row pushed the transcript down a row"
        );
        // Precision, not just presence: the row ABOVE the label is a different
        // transcript line, so it must NOT resolve to the same link.
        assert_ne!(
            resolve_link_click(&mut app, col, row - 1),
            LinkClick::Opening(LINK_URL.to_string()),
            "the row above the label is another transcript line, not the link"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_click_on_a_drawn_link_opens_it_beneath_a_pinned_status_banner() {
        // A reported session pins a banner to the pane's first inner row, so its
        // transcript starts one row lower. The click must follow the transcript,
        // not the pane.
        let dir = unique_temp_dir("link-live");
        let mut app = link_app(&dir, Some("blocked"));
        let buffer = render_board(&mut app);
        assert!(
            view::preview_banner(&app).is_some(),
            "a joined reported agent must pin a banner, or this is just the plain case"
        );
        assert!(
            app.preview_scroll > 0,
            "the fixture must overflow the pane, or this never tests a scrolled hit"
        );

        let (col, row) = drawn_link_cell(&buffer, app.preview_rect);
        assert_eq!(
            resolve_link_click(&mut app, col, row),
            LinkClick::Opening(LINK_URL.to_string()),
            "a click on the cell the link was DRAWN on must resolve to its url \
             even though the pinned banner pushed the transcript down a row"
        );
        assert_eq!(
            click(&mut app, col, row),
            MouseEffect::OpenLink(LINK_URL.to_string()),
            "and a click (press + release) there must open it"
        );
        // Precision, not just presence: the row ABOVE the label is a different
        // transcript line. `NoLink` and not merely "not this url" — a real render of
        // an ordinary prose line is the one place the NO-LINK outcome can be pinned
        // against something actually drawn, and it must not come back as the
        // abstention either.
        assert_eq!(
            resolve_link_click(&mut app, col, row - 1),
            LinkClick::NoLink,
            "the row above the label is another transcript line, not the link"
        );
        // So a click there must NOT open the same link.
        assert_ne!(
            click(&mut app, col, row - 1),
            MouseEffect::OpenLink(LINK_URL.to_string()),
            "the row above the label is another transcript line, not the link"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A link label drawn on the pane's FIRST content column is clickable.
    ///
    /// The older half of the marker cell's bug: the mouse splitter this board once
    /// had claimed `seam + 1`, the pane's first content column, in its grab band, so
    /// a link that started its line was swallowed by the seam arm — years before any
    /// peer node was drawn there. The pane widths now belong to the keyboard and no
    /// arm runs ahead of the pane arm; this pins that the column stays the
    /// transcript's.
    ///
    /// Pinned at the pure [`resolve_link_click`] seam like its sibling link tests,
    /// and then through a whole click at the pure [`mouse_effect`] — press, then
    /// release — which returns the url instead of handing it to `resume::open_url`,
    /// so no browser is spawned. The press gate is asked first: the cell must lie
    /// inside the transcript rect [`press_starts_selection`] admits presses in, the
    /// one rect every pane click is resolved against.
    #[test]
    fn a_click_on_a_link_starting_at_content_column_zero_opens_it() {
        let dir = unique_temp_dir("link-col0");
        let mut app = App::new(
            vec![link_at_column_zero_session(&dir)],
            Scope::All,
            PathBuf::from("/tmp"),
        );
        let buffer = render_board(&mut app);

        let (col, row) = drawn_link_cell(&buffer, app.preview_rect);
        assert_eq!(
            col,
            app.preview_rect.x + 1,
            "the fixture must really draw its label on the pane's first CONTENT \
             column, or this probes an ordinary interior link"
        );
        assert!(
            view::preview_transcript_rect(&app).contains(Position { x: col, y: row }),
            "the press gate admits presses in the transcript rect alone, so a cell \
             outside it would make the link unreachable however precisely the user \
             clicked it"
        );
        assert_eq!(
            resolve_link_click(&mut app, col, row),
            LinkClick::Opening(LINK_URL.to_string()),
            "and the pane click then resolves the cell the label was DRAWN on to its url"
        );
        assert_eq!(
            click(&mut app, col, row),
            MouseEffect::OpenLink(LINK_URL.to_string()),
            "so a whole click there — press, then release — opens it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The PRESS opens nothing: whether it was a click or the start of a drag is
    /// only known when the button comes back up, so the link waits for the
    /// release. The release of that same press then opens it.
    #[test]
    fn a_press_alone_on_a_drawn_link_opens_nothing_until_the_release() {
        let dir = unique_temp_dir("link-press");
        let mut app = link_app(&dir, None);
        let buffer = render_board(&mut app);
        let (col, row) = drawn_link_cell(&buffer, app.preview_rect);

        assert_eq!(
            mouse_at(
                &mut app,
                mouse_ev(MouseEventKind::Down(MouseButton::Left), col, row)
            ),
            MouseEffect::None,
            "a press on a link must not open it"
        );
        assert_eq!(
            mouse_at(
                &mut app,
                mouse_ev(MouseEventKind::Up(MouseButton::Left), col, row)
            ),
            MouseEffect::OpenLink(LINK_URL.to_string()),
            "the release of that press is what opens it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every [`LinkClick`] but the no-op reports itself, each with the stickiness its
    /// outcome earns. The ONE mapping, asserted as the table it is.
    ///
    /// The silent arm is what makes a MISSED HIT tellable from a FAILED OPENER.
    /// `resume::open_url` nulls all child stdio and swallows every error, so with no
    /// status a hit and a dead browser are the same observation — "no browser
    /// appeared". With it, a message and no browser indicts the opener, and NO MESSAGE
    /// means the click resolved no link (or never reached the handler at all). That
    /// second reading is only sound while every OTHER outcome ALWAYS produces a
    /// visible status, which is why each is allowed to overwrite a pending sticky
    /// refusal: were they suppressible, silence would stop meaning "no link" and the
    /// status line would answer neither question.
    ///
    /// The converse is the keypress protocol, kept intact: `NoLink` is a no-op — no
    /// scroll, no selection move, no url — so it must not evict a refusal the reader
    /// has not read yet.
    ///
    /// The three speaking arms run in sequence against the SAME app on purpose: each
    /// starts from the message the one before it left, so what is pinned is that each
    /// can displace the last, not merely that it can write to an empty line.
    #[test]
    fn note_link_click_speaks_for_every_outcome_but_the_no_op() {
        const REFUSAL: &str = "a refusal the reader has not read yet";
        let mut app = app_with("sess-link-status", None);
        app.set_status(REFUSAL);

        note_link_click(&mut app, &LinkClick::NoLink);
        assert_eq!(
            app.status.as_deref(),
            Some(REFUSAL),
            "a click that hit nothing is a no-op and must not wipe an unread refusal"
        );
        assert_eq!(
            app.status_ttl, None,
            "nor may a no-op start a dwell on someone else's sticky message"
        );

        note_link_click(&mut app, &LinkClick::Opening(LINK_URL.to_string()));
        assert_eq!(
            app.status.as_deref(),
            Some(format!("{LINK_OPENING_PREFIX}{LINK_URL}").as_str()),
            "a hit must ALWAYS show the url it hands to the opener, whatever the \
             status line held before it"
        );
        assert_eq!(
            app.status_ttl,
            Some(STATUS_DWELL_TICKS),
            "and it is a confirmation, so it dwells rather than squatting on the \
             keymap row"
        );

        note_link_click(&mut app, &LinkClick::RefusedScheme(REFUSED_URL.to_string()));
        assert_eq!(
            app.status.as_deref(),
            Some(format!("{LINK_REFUSED_PREFIX}{REFUSED_URL}{LINK_REFUSED_SUFFIX}").as_str()),
            "a refused scheme must name the url and the rule, or an underlined label \
             that does nothing is silent again"
        );
        assert_eq!(
            app.status_ttl, None,
            "and a refusal is sticky — it waits for the next actionable keypress"
        );

        note_link_click(&mut app, &LinkClick::Unresolvable);
        assert_eq!(
            app.status.as_deref(),
            Some(LINK_UNRESOLVED),
            "an abstention is the ONLY signal that an underlined label the reader \
             clicked did nothing on purpose, so it must speak too"
        );
        assert_eq!(
            app.status_ttl, None,
            "and it is a refusal, not a confirmation: a message that expired unread \
             would leave exactly the silence it exists to break"
        );
    }

    /// A drag that STARTS on a link is a selection, not a click: its release copies
    /// the selected text and opens nothing.
    #[test]
    fn a_drag_that_starts_on_a_link_copies_instead_of_opening_it() {
        let dir = unique_temp_dir("link-drag");
        let mut app = link_app(&dir, None);
        let buffer = render_board(&mut app);
        let (col, row) = drawn_link_cell(&buffer, app.preview_rect);

        let pressed = mouse_at(
            &mut app,
            mouse_ev(MouseEventKind::Down(MouseButton::Left), col, row),
        );
        assert_eq!(pressed, MouseEffect::None);
        render_board(&mut app);
        mouse_at(
            &mut app,
            mouse_ev(MouseEventKind::Drag(MouseButton::Left), col + 3, row),
        );
        render_board(&mut app);
        let released = mouse_at(
            &mut app,
            mouse_ev(MouseEventKind::Up(MouseButton::Left), col + 3, row),
        );

        let MouseEffect::Copy(text) = released else {
            panic!("a drag's release must copy, not open: got {released:?}");
        };
        assert_eq!(text, "docs", "the four drawn cells of the link label");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_left_click_in_the_preview_body_without_a_link_is_a_harmless_no_op() {
        // The preview-link arm must never open the overlay or panic when the
        // pointer is not over a link. The synthetic session's file does not exist,
        // so the preview has no link regions — the click resolves to nothing.
        const REFUSAL: &str = "a refusal the reader has not read yet";
        let mut app = app_with("s", None);
        // A list/preview pair mirroring `render_body`'s no-gap split: the preview
        // starts exactly where the list ends.
        app.list_rect = Rect {
            x: 0,
            y: 0,
            width: 50,
            height: 20,
        };
        app.preview_rect = Rect {
            x: 50,
            y: 0,
            width: 40,
            height: 20,
        };
        app.set_status(REFUSAL);

        // Well inside the preview body (col 70 of the 50..90 preview): a whole
        // click — press, then release — through the real handler.
        wheel(&mut app, MouseEventKind::Down(MouseButton::Left), 70, 10);
        let released = wheel(&mut app, MouseEventKind::Up(MouseButton::Left), 70, 10);
        assert!(
            matches!(released, Outcome::Continue),
            "a click with no link and no drag asks nothing of the driver"
        );
        assert!(
            app.modal.is_none(),
            "a preview-body click must not open the overlay"
        );
        // The miss half of the wiring, asserted from the same end the user sees:
        // a real click — `Down(Left)` then `Up(Left)` — through `handle_event`, not
        // a direct call to the decision fn. A no-op must leave an unread refusal
        // exactly where it was.
        assert_eq!(
            app.status.as_deref(),
            Some(REFUSAL),
            "a click that hit nothing must not wipe an unread refusal"
        );
        assert_eq!(
            app.status_ttl, None,
            "nor start a dwell on someone else's sticky message"
        );
    }

    /// The HIT half of the wiring, END TO END through a real click — a `Down(Left)`
    /// and its `Up(Left)`, since the RELEASE is what resolves a click.
    ///
    /// `note_link_click_speaks_for_every_outcome_but_the_no_op` pins the MAPPING — it
    /// calls `note_link_click` directly — which leaves the CALL SITE uncovered:
    /// deleting `note_link_click(...)` out of the driver left that test, and the whole
    /// suite, green. This drives the mouse events instead, so the chain from
    /// `handle_event` through `resolve_link_click` to the status line is what is
    /// asserted. The PRESS alone is asserted to say nothing first: a click is only
    /// known to be one when the button comes back up.
    ///
    /// It clicks a REFUSED scheme on purpose, and that is not a compromise: the route
    /// through `handle_event` reaches `resume::open_url`, and an accepted url there
    /// would launch a real browser on whoever ran the suite. A refused one exercises
    /// exactly the same wiring — `link_at` resolves it, `note_link_click` speaks for
    /// it — while the opener gets nothing. The ACCEPTED url is asserted one level
    /// down, in the test below, where no spawn can happen.
    #[test]
    fn a_click_on_a_link_snapback_will_not_open_says_so_and_opens_nothing() {
        let dir = unique_temp_dir("link-refused");
        let mut app = link_app_to(&dir, None, REFUSED_URL);
        let buffer = render_board(&mut app);
        let (col, row) = drawn_link_cell(&buffer, app.preview_rect);
        // The premise: the label IS drawn, underlined, and the click DOES resolve to
        // the refused url. Without this the test could pass on a link that was never
        // rendered at all, and prove nothing about the refusal.
        assert_eq!(
            resolve_link_click(&mut app, col, row),
            LinkClick::RefusedScheme(REFUSED_URL.to_string()),
            "a non-http target still renders and still records a clickable region"
        );

        wheel(&mut app, MouseEventKind::Down(MouseButton::Left), col, row);
        assert_eq!(app.status, None, "the press alone resolves nothing yet");
        wheel(&mut app, MouseEventKind::Up(MouseButton::Left), col, row);
        assert_eq!(
            app.status.as_deref(),
            Some(format!("{LINK_REFUSED_PREFIX}{REFUSED_URL}{LINK_REFUSED_SUFFIX}").as_str()),
            "an underlined label that does nothing is the silence this whole \
             hit-test exists to remove, so a refusal must SAY which url and why"
        );
        assert_eq!(
            app.status_ttl, None,
            "and a refusal is sticky, not a confirmation that dwells"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same wiring for a url the opener DOES accept, run one level DOWN from
    /// `handle_event` — `resolve_link_click` then `note_link_click`, which is the
    /// driver minus its single spawn, so no browser is ever launched by the test suite
    /// (PATTERNS: test the pure helper, not the impure driver). The url inside the
    /// `Opening` it returns IS what `click_effect` hands `handle_mouse` as
    /// `MouseEffect::OpenLink` for `resume::open_url`, so this pins both what is said
    /// and what is opened.
    #[test]
    fn a_click_on_a_drawn_http_link_announces_the_url_it_hands_the_opener() {
        let dir = unique_temp_dir("link-opening");
        let mut app = link_app(&dir, None);
        let buffer = render_board(&mut app);
        let (col, row) = drawn_link_cell(&buffer, app.preview_rect);

        let click = resolve_link_click(&mut app, col, row);
        assert_eq!(
            click,
            LinkClick::Opening(LINK_URL.to_string()),
            "an http(s) hit is the url handed to the opener"
        );
        note_link_click(&mut app, &click);
        assert_eq!(
            app.status.as_deref(),
            Some(format!("{LINK_OPENING_PREFIX}{LINK_URL}").as_str()),
            "and the click must name it, or a dead opener is indistinguishable \
             from a missed hit"
        );
        assert_eq!(
            app.status_ttl,
            Some(STATUS_DWELL_TICKS),
            "it is a confirmation, so it dwells rather than squatting on the keymap row"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The FOURTH outcome: a click on a line too large to hit-test resolves to
    /// `Unresolvable` and SAYS so, rather than falling silent.
    ///
    /// The silence is the bug. An over-budget line still has its links RENDERED and
    /// UNDERLINED, so the reader clicks an affordance that is plainly there and gets
    /// nothing back — the very state the hit-test exists to remove, re-created one
    /// level up. The abstention itself is right (a guessed hit hands a browser a url
    /// nobody aimed at); only its muteness was wrong.
    ///
    /// The CONTROL run is the whole proof. Without it, `Unresolvable` here could just
    /// as well mean the fixture failed to render, or that clicks on this shape never
    /// resolve at all. The same click on the same shape with [`CROWDED_CONTROL_LINKS`]
    /// links is an ordinary `Opening`, so the only difference between the two runs is
    /// the budget crossing — and this cannot pass vacuously if the fixture stops
    /// crossing it.
    ///
    /// The second half drives a REAL click — `Down(Left)` then `Up(Left)`, since the
    /// release is what resolves it — through `handle_event`, because the decision
    /// being right proves nothing if the driver never asks for it. No browser can be
    /// launched: `Unresolvable` is precisely the outcome that spawns nothing.
    #[test]
    fn a_click_on_a_line_too_large_to_hit_test_says_so_instead_of_falling_silent() {
        let control_dir = unique_temp_dir("link-control");
        let mut control = crowded_link_app(&control_dir, CROWDED_CONTROL_LINKS);
        let control_buffer = render_board(&mut control);
        let (control_col, control_row) = drawn_link_cell(&control_buffer, control.preview_rect);
        assert_eq!(
            resolve_link_click(&mut control, control_col, control_row),
            LinkClick::Opening(LINK_URL.to_string()),
            "the same fixture shape UNDER the budget must be an ordinary hit, or the \
             run below says nothing about the budget"
        );
        let _ = std::fs::remove_dir_all(&control_dir);

        let dir = unique_temp_dir("link-crowded");
        let mut app = crowded_link_app(&dir, CROWDED_LINE_LINKS);
        let buffer = render_board(&mut app);
        let (col, row) = drawn_link_cell(&buffer, app.preview_rect);
        assert_eq!(
            resolve_link_click(&mut app, col, row),
            LinkClick::Unresolvable,
            "past the budget the click must resolve to the ABSTENTION and not to \
             `NoLink` — the control run just hit a link on this very shape, so \
             claiming there is none would be a false statement about the transcript"
        );

        // A sticky message the reader has not read yet, so the assertion below also
        // shows the abstention is actionable enough to displace one.
        const REFUSAL: &str = "a refusal the reader has not read yet";
        app.set_status(REFUSAL);
        left_click(&mut app, col, row);
        assert_eq!(
            app.status.as_deref(),
            Some(LINK_UNRESOLVED),
            "an underlined label that does nothing must SAY that snapback could not \
             tell which link it was, or the click is indistinguishable from a dead \
             opener and from a click that landed on nothing"
        );
        assert_eq!(
            app.status_ttl, None,
            "and it is a refusal, not a confirmation: a message that dwelled away \
             unread would leave exactly the silence it exists to break"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- drag-selection: ONE transcript rect, shared with the link hit-test ---

    /// Row `y` of `pane` exactly as drawn, border columns included.
    fn drawn_row(buffer: &ratatui::buffer::Buffer, pane: Rect, y: u16) -> String {
        (pane.x..pane.right())
            .filter_map(|x| buffer.cell((x, y)).map(|c| c.symbol().to_string()))
            .collect()
    }

    /// A press on the PINNED BANNER row never starts a selection — the banner is
    /// not transcript, and the selection's rect is the link hit-test's rect, which
    /// already starts one row lower. The control half proves the fixture CAN
    /// select: the same gesture from the first transcript row copies.
    #[test]
    fn a_press_on_the_pinned_banner_row_never_starts_a_selection() {
        let dir = unique_temp_dir("select-banner");
        let mut app = link_app(&dir, Some("blocked"));
        let buffer = render_board(&mut app);
        let pane = app.preview_rect;
        // The pinned row as DRAWN: the pane's first inner row carries text — the
        // marker of the turn at the top of the viewport, which is what this row
        // shows whenever the transcript has one, else the reported status — and
        // the transcript starts on the row below it.
        assert!(
            view::preview_banner(&app).is_some(),
            "premise: a joined reported agent pins a row"
        );
        let banner_row = pane.y + 1;
        assert!(
            drawn_row(&buffer, pane, banner_row)
                .trim_matches(|c: char| c == '│' || c.is_whitespace())
                .chars()
                .count()
                > 0,
            "premise: the pinned row is drawn on the pane's first inner row"
        );
        assert_eq!(
            view::preview_transcript_rect(&app).y,
            banner_row + 1,
            "premise: and the transcript starts right under it"
        );

        let col = pane.x + 3;
        let released = drag_and_release(&mut app, (col, banner_row), (col + 8, banner_row + 2));
        assert!(
            !app.has_preview_selection(),
            "a press on the banner row must not start a selection"
        );
        assert!(
            !matches!(released, Outcome::Copy(_)),
            "and its release must copy nothing"
        );

        // Control: one row lower is transcript, and the same drag selects.
        let released = drag_and_release(&mut app, (col, banner_row + 1), (col + 8, banner_row + 3));
        assert!(
            matches!(released, Outcome::Copy(CopyPayload::Selection(_))),
            "the first transcript row under the banner is selectable"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The row EVERY selected session pins above its transcript — here an
    /// UNREPORTED one, whose pinned row names the turn at the top of the viewport —
    /// is never selectable and never clickable: a drag from it selects and copies
    /// nothing, and a click on it resolves nothing. It is chrome that restates a
    /// turn marker, not transcript, and the one transcript rect every pointer
    /// action reads starts below it. The control drag proves the fixture CAN
    /// select one row lower.
    #[test]
    fn the_pinned_row_of_an_unreported_session_never_starts_a_selection_or_a_click() {
        let dir = unique_temp_dir("select-pinned");
        let mut app = link_app(&dir, None);
        let buffer = render_board(&mut app);
        let pane = app.preview_rect;
        assert!(
            app.reported_agent("sess-link").is_none() && view::preview_banner(&app).is_some(),
            "premise: an unreported session still pins the row"
        );
        let pinned_row = pane.y + 1;
        let drawn_pinned = drawn_row(&buffer, pane, pinned_row);
        assert!(
            drawn_pinned.contains("you \u{b7} "),
            "premise: the pinned row names the top turn — the fixture's one user \
             turn: {drawn_pinned:?}"
        );
        assert_eq!(
            view::preview_transcript_rect(&app).y,
            pinned_row + 1,
            "premise: and the transcript starts right under it"
        );

        let col = pane.x + 3;
        let released = drag_and_release(&mut app, (col, pinned_row), (col + 8, pinned_row + 2));
        assert!(
            !app.has_preview_selection() && !matches!(released, Outcome::Copy(_)),
            "a drag from the pinned row must select and copy nothing"
        );
        assert_eq!(
            click(&mut app, col, pinned_row),
            MouseEffect::None,
            "and a click on it resolves nothing"
        );
        assert_eq!(app.status, None, "not even a status line");

        let released = drag_and_release(&mut app, (col, pinned_row + 1), (col + 8, pinned_row + 3));
        assert!(
            matches!(released, Outcome::Copy(CopyPayload::Selection(_))),
            "control: the first transcript row under the pinned row is selectable"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No selection starts while the new-session DRAFT CARD is shown — the card
    /// replaces the transcript, so there is nothing of the session's to select, the
    /// same reason a link click is off there. Pinned on a DISPATCHED card, whose
    /// editor is already closed, so the card alone is what gates the press.
    #[test]
    fn no_selection_starts_under_the_new_session_draft_card() {
        let dir = unique_temp_dir("select-draft");
        let mut app = link_app(&dir, None);
        seed_live(&mut app, &[]);
        app.open_agent_picker(vec![def_agent("planner")]);
        press(&mut app, KeyCode::Enter);
        type_into_draft(&mut app, "ship the thing");
        assert!(
            matches!(press(&mut app, KeyCode::Enter), Outcome::BgLaunch(_)),
            "the draft must dispatch for its card to outlive the editor"
        );
        assert!(app.draft.is_some(), "premise: the card is up");
        assert!(!app.is_composing(), "premise: and the editor is closed");

        let buffer = render_board(&mut app);
        let (col, row) = drawn_text_cell(&buffer, app.preview_rect, "new session");
        let released = drag_and_release(&mut app, (col, row), (col + 6, row + 1));

        assert!(
            !app.has_preview_selection(),
            "a press over the draft card must not start a selection"
        );
        assert!(
            !matches!(released, Outcome::Copy(_)),
            "and its release must copy nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With no session selected the pane shows a placeholder sentence, not a
    /// transcript, so a drag over it selects nothing and copies nothing.
    #[test]
    fn the_no_session_placeholder_is_not_selectable() {
        let mut app = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp"));
        let buffer = render_board(&mut app);
        let (col, row) = drawn_text_cell(&buffer, app.preview_rect, "No session selected.");
        let released = drag_and_release(&mut app, (col, row), (col + 8, row));
        assert!(
            !app.has_preview_selection(),
            "the placeholder is not transcript"
        );
        assert!(!matches!(released, Outcome::Copy(_)));
    }

    /// While a reply is DOCKED in the preview pane, the transcript rect every
    /// pointer action reads stops above the compose box: the rows the editor is
    /// drawn in are never transcript. Read off the drawn box (its top-left corner
    /// inside the pane), never computed from the geometry under test. A press in
    /// the box selects nothing either.
    #[test]
    fn the_shared_transcript_rect_stops_above_a_docked_compose_zone() {
        let dir = unique_temp_dir("select-compose");
        let mut app = link_app(&dir, None);
        seed_live(&mut app, &[]);
        press_ctrl(&mut app, KeyCode::Char('r'));
        assert!(
            app.is_composing(),
            "premise: Ctrl-R on an idle session composes"
        );
        let buffer = render_board(&mut app);
        let pane = app.preview_rect;

        // The docked box draws its own border INSIDE the pane: its top-left corner
        // sits one column in from the pane's left border.
        let compose_top = (pane.y + 1..pane.bottom())
            .find(|&y| {
                buffer
                    .cell((pane.x + 1, y))
                    .is_some_and(|c| c.symbol() == "┌")
            })
            .expect("premise: the compose box is docked inside the preview pane");

        let transcript = view::preview_transcript_rect(&app);
        assert!(
            transcript.height > 0,
            "premise: some transcript is still shown"
        );
        assert!(
            transcript.bottom() <= compose_top,
            "the transcript rect ({transcript:?}) must end above the docked compose box \
             (top row {compose_top})"
        );

        let released = drag_and_release(
            &mut app,
            (pane.x + 3, compose_top + 1),
            (pane.x + 9, compose_top + 1),
        );
        assert!(
            !app.has_preview_selection(),
            "a press in the box selects nothing"
        );
        assert!(!matches!(released, Outcome::Copy(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- an open QUICK REPLY leaves the transcript's pointer actions alone ------

    /// Open a quick reply on `link_app`'s session, type `typed` into it, and draw a
    /// frame — the board a user is looking at while they compose. The reply
    /// previews the REAL transcript (no draft card), so everything the pointer does
    /// over it is the same gesture as on the bare board.
    fn composing_reply_over_link_app(dir: &Path, typed: &str) -> App {
        let mut app = link_app(dir, None);
        seed_live(&mut app, &[]);
        press_ctrl(&mut app, KeyCode::Char('r'));
        assert!(
            app.is_composing() && app.draft.is_none(),
            "premise: a quick reply is open and no draft card replaced the transcript"
        );
        type_into_draft(&mut app, typed);
        render_board(&mut app);
        app
    }

    /// What an open reply must be left holding after a pointer gesture: still open,
    /// still addressed to the SAME session, its text untouched, and the row
    /// selection where it was. A gesture over the transcript is not the reply's
    /// business, and none of these may move.
    fn assert_reply_untouched(app: &App, typed: &str) {
        assert!(app.is_composing(), "the gesture must not close the reply");
        assert_eq!(
            app.compose.as_ref().map(|c| &c.target),
            Some(&ComposeTarget::Reply {
                session_id: "sess-link".to_string(),
                stop_job: None,
            }),
            "the reply must still address the session it was opened on"
        );
        assert_eq!(draft_text(app), typed, "the typed text must be untouched");
        assert_eq!(
            app.selected.as_deref(),
            Some("sess-link"),
            "the selected row must not move"
        );
    }

    /// A drag over the transcript while a quick reply is open selects the drawn
    /// text and copies it on release, and leaves the reply alone. The selection is
    /// MOUSE state, so it neither takes the keyboard from the reply nor moves its
    /// caret: the next keystroke still lands in the draft, and — like any key —
    /// ends the selection.
    #[test]
    fn a_drag_selects_and_copies_while_a_reply_is_open() {
        let dir = unique_temp_dir("compose-drag");
        let mut app = composing_reply_over_link_app(&dir, "hello");
        let transcript = view::preview_transcript_rect(&app);
        let buffer = render_board(&mut app);
        let needle = "filler line";
        let (col, row) = drawn_text_cell(&buffer, transcript, needle);
        let last = col + u16::try_from(needle.len()).expect("short") - 1;

        let released = drag_and_release(&mut app, (col, row), (last, row));

        let Outcome::Copy(payload) = released else {
            panic!("a finished drag must request a copy even with a reply open");
        };
        assert_eq!(payload, CopyPayload::Selection(needle.to_string()));
        assert_reply_untouched(&app, "hello");

        press(&mut app, KeyCode::Char('!'));
        assert_eq!(
            draft_text(&app),
            "hello!",
            "the keyboard is still the reply's: the selection did not take it"
        );
        assert!(
            !app.has_preview_selection(),
            "and a key ends the selection, as it does on the board"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A double-click on a transcript word while a quick reply is open copies that
    /// word, exactly as on the bare board.
    #[test]
    fn a_double_click_copies_a_word_while_a_reply_is_open() {
        let dir = unique_temp_dir("compose-dbl");
        let mut app = composing_reply_over_link_app(&dir, "hello");
        let transcript = view::preview_transcript_rect(&app);
        let buffer = render_board(&mut app);
        let (col, row) = drawn_text_cell(&buffer, transcript, "filler line");
        let cell = (col + 1, row);
        let t0 = Instant::now();

        click_at(&mut app, cell, t0);
        button(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            cell,
            t0 + Duration::from_millis(100),
        );
        let released = button(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            cell,
            t0 + Duration::from_millis(150),
        );

        assert_eq!(released, MouseEffect::Copy("filler".to_string()));
        assert_reply_untouched(&app, "hello");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A click on a drawn transcript link while a quick reply is open resolves to
    /// opening it, and leaves the reply alone.
    #[test]
    fn a_link_click_opens_the_url_while_a_reply_is_open() {
        let dir = unique_temp_dir("compose-link");
        let mut app = composing_reply_over_link_app(&dir, "hello");
        let buffer = render_board(&mut app);
        let (col, row) = drawn_link_cell(&buffer, app.preview_rect);
        assert!(
            view::preview_transcript_rect(&app).contains(Position { x: col, y: row }),
            "premise: the link is drawn in the transcript, above the docked reply box"
        );

        assert_eq!(
            click(&mut app, col, row),
            MouseEffect::OpenLink(LINK_URL.to_string())
        );
        assert_reply_untouched(&app, "hello");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A click on a peer message's header (the `@agent` hand-back marker) while a
    /// quick reply is open unfolds it, and a second click folds it back — the
    /// reply is left alone throughout.
    #[test]
    fn a_peer_node_header_toggles_while_a_reply_is_open() {
        let (folder, file) = PEER_FIXTURE;
        let mut app = App::new(
            vec![fixture_session("s1", folder, file)],
            Scope::All,
            PathBuf::from("/tmp"),
        );
        seed_live(&mut app, &[]);
        press_ctrl(&mut app, KeyCode::Char('r'));
        assert!(
            app.is_composing() && app.draft.is_none(),
            "premise: a quick reply is open over the real transcript"
        );
        type_into_draft(&mut app, "hello");
        let buffer = render_board(&mut app);
        let width = view::preview_transcript_rect(&app).width;
        let (col, row) = drawn_peer_handle_cell(&buffer, app.preview_rect);
        assert!(
            view::preview_transcript_rect(&app).contains(Position { x: col, y: row }),
            "premise: the header is drawn in the transcript, above the docked reply box"
        );

        left_click(&mut app, col, row);
        assert!(
            preview_string(&mut app, width).contains(PEER_BODY_PHRASE),
            "a click on the header must open the node under an open reply"
        );
        // No frame between the clicks, like the bare-board twin: the open node
        // moves the header, and the second press is aimed at where the first was.
        let reclosed = separate_left_click(&mut app, col, row);
        assert_eq!(reclosed, MouseEffect::None, "a toggle opens no link");
        assert!(
            !preview_string(&mut app, width).contains(PEER_BODY_PHRASE),
            "and a second click folds it back"
        );
        assert!(app.is_composing(), "the reply is still open");
        assert_eq!(draft_text(&app), "hello", "and its text is untouched");
        assert_eq!(app.selected.as_deref(), Some("s1"));
    }

    /// The reply's own surface is still out of the pointer's reach: a press in the
    /// docked box never starts a selection (pinned beside the unlock, which must not
    /// have widened the transcript rect into the box).
    #[test]
    fn a_press_in_the_docked_reply_box_still_selects_nothing() {
        let dir = unique_temp_dir("compose-box");
        let mut app = composing_reply_over_link_app(&dir, "hello");
        let buffer = render_board(&mut app);
        let pane = app.preview_rect;
        let compose_top = (pane.y + 1..pane.bottom())
            .find(|&y| {
                buffer
                    .cell((pane.x + 1, y))
                    .is_some_and(|c| c.symbol() == "┌")
            })
            .expect("premise: the compose box is docked inside the preview pane");

        let released = drag_and_release(
            &mut app,
            (pane.x + 3, compose_top + 1),
            (pane.x + 9, compose_top + 1),
        );

        assert!(!app.has_preview_selection());
        assert!(!matches!(released, Outcome::Copy(_)));
        assert_reply_untouched(&app, "hello");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// While a new-session draft is being TYPED (editor and card both up), the
    /// transcript is not on screen, so no press starts a selection. The other half
    /// of the quick-reply unlock: the card, not the editor, is what gates the
    /// pointer, and a draft always has one.
    #[test]
    fn no_selection_starts_while_a_new_session_draft_is_open() {
        let dir = unique_temp_dir("select-draft-open");
        let mut app = link_app(&dir, None);
        seed_live(&mut app, &[]);
        app.open_agent_picker(vec![def_agent("planner")]);
        press(&mut app, KeyCode::Enter);
        assert!(
            app.is_composing() && app.draft.is_some(),
            "premise: the draft editor and its card are both up"
        );
        let buffer = render_board(&mut app);
        let (col, row) = drawn_text_cell(&buffer, app.preview_rect, "new session");

        let released = drag_and_release(&mut app, (col, row), (col + 6, row));

        assert!(!app.has_preview_selection());
        assert!(!matches!(released, Outcome::Copy(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The model picker opens OVER a reply (`Ctrl-L`) and owns the keyboard, so the
    /// transcript behind it takes no press until it closes: the modal gate is not
    /// loosened by the reply unlock.
    #[test]
    fn no_selection_starts_while_the_model_picker_is_over_a_reply() {
        let dir = unique_temp_dir("compose-picker");
        let mut app = composing_reply_over_link_app(&dir, "hello");
        // Read the cell off the board BEFORE the picker covers the transcript: the
        // press must be aimed at text that really is behind it.
        let buffer = render_board(&mut app);
        let (col, row) =
            drawn_text_cell(&buffer, view::preview_transcript_rect(&app), "filler line");
        press_ctrl(&mut app, KeyCode::Char('l'));
        assert!(app.modal.is_some(), "premise: the picker is open");
        render_board(&mut app);

        let released = drag_and_release(&mut app, (col, row), (col + 6, row));

        assert!(!app.has_preview_selection());
        assert!(!matches!(released, Outcome::Copy(_)));
        assert!(app.modal.is_some(), "and the picker is undisturbed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- pane layout: Shift-Left / Shift-Right through the whole handler ------

    /// The user's walk, pressed through `handle_event`: three `Shift-←` from the
    /// 1:1 start reach 0:1 and the third changes nothing, then `Shift-→` walks all
    /// five stops to 1:0 and a press past it changes nothing. Asserting the state
    /// each PRESS produced is what pins the `apply_action` arm between the decode
    /// and the `App` method — a swapped arm leaves both halves' own tests green.
    #[test]
    fn shift_arrows_walk_the_layout_ladder_and_stop_at_both_ends() {
        let mut app = app_with("s", None);
        assert_eq!(app.pane_layout(), PaneLayout::Even, "the board starts 1:1");

        let mut walked = Vec::new();
        for _ in 0..3 {
            press_shift(&mut app, KeyCode::Left);
            walked.push(app.pane_layout());
        }
        assert_eq!(
            walked,
            [
                PaneLayout::PreviewWide,
                PaneLayout::PreviewOnly,
                PaneLayout::PreviewOnly
            ],
            "Shift-← stops at 0:1"
        );

        let mut walked = Vec::new();
        for _ in 0..5 {
            press_shift(&mut app, KeyCode::Right);
            walked.push(app.pane_layout());
        }
        assert_eq!(
            walked,
            [
                PaneLayout::PreviewWide,
                PaneLayout::Even,
                PaneLayout::ListWide,
                PaneLayout::ListOnly,
                PaneLayout::ListOnly
            ],
            "Shift-→ walks every stop and stops at 1:0"
        );
        assert_eq!(
            app.selected.as_deref(),
            Some("s"),
            "no step moves the selection"
        );
    }

    /// With a query typed, `Shift-←/→` still step the layout — and leave the query
    /// exactly as it was, so a search in progress survives a change of layout.
    #[test]
    fn shift_arrows_step_the_layout_with_a_query_typed() {
        let mut app = app_with("s", None);
        type_into_board(&mut app, "lab");
        assert_eq!(app.query(), "lab", "premise: a query is typed");
        assert_eq!(
            app.selected.as_deref(),
            Some("s"),
            "premise: the query keeps the row"
        );

        press_shift(&mut app, KeyCode::Left);
        assert_eq!(app.pane_layout(), PaneLayout::PreviewWide);
        press_shift(&mut app, KeyCode::Right);
        press_shift(&mut app, KeyCode::Right);
        assert_eq!(app.pane_layout(), PaneLayout::ListWide);
        assert_eq!(app.query(), "lab", "the query is untouched");
    }

    /// With search hits MARKED in the previewed transcript — the one state in which
    /// `Shift-↑`/`Shift-↓` change meaning — `Shift-←/→` still step the layout.
    ///
    /// A real transcript on disk and a real drawn frame, because the marks exist
    /// only once the pane has been rendered: `has_preview_matches` reads the cache
    /// the draw fills, and a synthetic session with no file never has any.
    #[test]
    fn shift_arrows_step_the_layout_with_search_hits_present() {
        let dir = unique_temp_dir("layout-hits");
        let mut session = link_session(&dir, LINK_URL);
        // The row is admitted by CONTENT, so the query below keeps it on the board
        // in name+content mode.
        session.content_index = "filler line 1".to_string();
        let mut app = App::new(vec![session], Scope::All, PathBuf::from("/tmp"));
        seed_live(&mut app, &[]);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.search_mode, SearchMode::NameAndContent);
        type_into_board(&mut app, "filler");
        render_board(&mut app);
        assert!(
            app.has_preview_matches(),
            "premise: the query is marked in the drawn preview, so the SHIFTED \
             vertical arrows are live"
        );

        press_shift(&mut app, KeyCode::Left);
        assert_eq!(app.pane_layout(), PaneLayout::PreviewWide);
        render_board(&mut app);
        press_shift(&mut app, KeyCode::Right);
        assert_eq!(app.pane_layout(), PaneLayout::Even);
        assert_eq!(app.query(), "filler", "the query is untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- peer-message fold: the pane click's fold-then-link precedence -------

    /// The checked-in peer-node fixture: one `origin.kind:"peer"` hand-back sitting
    /// below a typed prompt, an assistant turn and an `Agent` tool use, so the node
    /// header these tests click is a REAL drawn row rather than a synthetic region.
    const PEER_FIXTURE: (&str, &str) = ("-Users-me-project-epsilon", "sess-peer-handback-1.jsonl");

    /// That hand-back's fold key: `origin.from`, the sending agent's stem — NEVER
    /// the record `uuid`.
    const PEER_KEY: &str = "a03505fe4b1c2d3e0";

    /// A phrase carried ONLY by the hand-back's `origin.body`. `App::expanded_peers`
    /// is private to `app`, so the node's state is asserted the way a user reads it:
    /// this phrase is on screen when the node is OPEN and absent when it is CLOSED.
    ///
    /// It is the report's closing line rather than its heading on purpose — the
    /// fixture's SUMMARY says "webhook retry backoff" too, so a phrase from the
    /// heading would be on screen with the node shut and prove nothing.
    const PEER_BODY_PHRASE: &str = "No pending questions.";

    /// The affordance a COLLAPSED node's header ends in. `store::preview` owns the
    /// literal; it is restated here on purpose, because a test that asserted through
    /// the private const could not tell a rename from a regression.
    const COLLAPSED_AFFORDANCE: &str = "(click to expand)";
    /// The affordance an EXPANDED node's header ends in; see the collapsed sibling.
    const EXPANDED_AFFORDANCE: &str = "(click to collapse)";

    /// A session backed by a checked-in fixture file, so the preview cache has a
    /// real transcript to render (a synthetic [`session`] points at a `/tmp` path
    /// that does not exist and renders to nothing).
    fn fixture_session(id: &str, folder: &str, file: &str) -> Session {
        let mut s = session(id);
        s.file = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("store")
            .join(folder)
            .join(file);
        s
    }

    /// An app over [`PEER_FIXTURE`], laid out by a REAL draw so `preview_rect`,
    /// `list_rect` and the resolved `preview_scroll` are the geometry a user is
    /// looking at. Returns the app beside the buffer that was drawn from it.
    fn peer_app() -> (App, ratatui::buffer::Buffer) {
        let (folder, file) = PEER_FIXTURE;
        let mut app = App::new(
            vec![fixture_session("s1", folder, file)],
            Scope::All,
            PathBuf::from("/tmp"),
        );
        assert_eq!(app.selected.as_deref(), Some("s1"));
        let buffer = render_board(&mut app);
        (app, buffer)
    }

    /// Where a glyph was actually DRAWN inside the preview pane, as screen
    /// `(col, row)`.
    ///
    /// Scanning the drawn buffer — never COMPUTING a row — is the same discipline
    /// [`drawn_link_cell`] keeps: a computed row would restate the geometry under
    /// test and pass however far it drifted.
    fn drawn_cell(
        buffer: &ratatui::buffer::Buffer,
        preview: Rect,
        glyph: &str,
    ) -> Option<(u16, u16)> {
        (preview.y..preview.bottom())
            .flat_map(|y| (preview.x..preview.right()).map(move |x| (x, y)))
            .find(|&(x, y)| buffer.cell((x, y)).is_some_and(|c| c.symbol() == glyph))
    }

    /// The node header's LEFTMOST drawn cell — the marker glyph `store::preview`
    /// opens the line with, which lands on content column 0.
    ///
    /// That is the pane's FIRST CONTENT column, one inside its left border. It is an
    /// ordinary toggle probe, and the strictest one the header has: the marker is
    /// the glyph the "(click to expand)" affordance is promising about, so a node
    /// that cannot be toggled HERE has a header that lies. It is deliberately NOT a
    /// border probe — the border one column to its left is the pane's chrome, and
    /// this column belongs to the transcript.
    fn drawn_peer_marker_cell(buffer: &ratatui::buffer::Buffer, preview: Rect) -> (u16, u16) {
        drawn_cell(buffer, preview, "\u{25c6}").expect(
            "the fixture's peer node must be drawn inside the preview pane, \
             or these tests prove nothing",
        )
    }

    /// A cell in the MIDDLE of the same header — the `@` opening the sender handle,
    /// the one glyph no other turn marker draws.
    ///
    /// Well clear of the pane's left edge, so a click here tests the fold-then-link
    /// precedence rather than the edge. A `FoldRegion` spans the header's whole
    /// display width, so this cell and the marker cell address the same node.
    fn drawn_peer_handle_cell(buffer: &ratatui::buffer::Buffer, preview: Rect) -> (u16, u16) {
        let cell = drawn_cell(buffer, preview, "@")
            .expect("the node header must draw its sender handle, or this probes nothing");
        assert_eq!(
            cell.1,
            drawn_peer_marker_cell(buffer, preview).1,
            "the handle must sit on the header's own row"
        );
        cell
    }

    /// The preview transcript as plain text, joined the way a reader sees it.
    fn preview_string(app: &mut App, width: u16) -> String {
        app.preview_text(width)
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A click on a collapsed node's header OPENS it, and a second, SEPARATE click
    /// on the same header CLOSES it again (resolved decision 4).
    ///
    /// Driven END TO END through [`handle_mouse`] — a press, then its release, which
    /// [`click_effect`] resolves — not through [`fold_under_pointer`] alone, because
    /// the routing is what this phase adds: a resolver nothing routes to would pass
    /// every assertion below and still leave the node inert under a real click. The
    /// second click goes through [`mouse_effect`] at an instant past
    /// [`DOUBLE_CLICK_INTERVAL`] ([`separate_left_click`]), since a quick one on the
    /// same cell is a double-click.
    #[test]
    fn a_click_on_a_peer_node_header_expands_it_and_a_second_click_collapses_it() {
        let (mut app, buffer) = peer_app();
        let width = view::preview_transcript_rect(&app).width;
        let (col, row) = drawn_peer_handle_cell(&buffer, app.preview_rect);
        assert!(
            view::preview_transcript_rect(&app).contains(Position { x: col, y: row }),
            "the probe must be inside the transcript rect the press gate admits"
        );

        let collapsed = preview_string(&mut app, width);
        assert!(
            collapsed.contains(COLLAPSED_AFFORDANCE) && !collapsed.contains(PEER_BODY_PHRASE),
            "a peer node starts CLOSED, or the expand assertion below is vacuous"
        );

        left_click(&mut app, col, row);
        let expanded = preview_string(&mut app, width);
        assert!(
            expanded.contains(PEER_BODY_PHRASE),
            "the first click must open the node's body"
        );
        assert!(
            expanded.contains(EXPANDED_AFFORDANCE),
            "an open node's header must offer the click that closes it again"
        );

        // A SEPARATE second click: one inside the double-click interval selects a
        // word instead and leaves the node open
        // (`a_double_click_on_a_folded_peer_header_opens_it_once_and_copies_the_word`).
        separate_left_click(&mut app, col, row);
        let reclosed = preview_string(&mut app, width);
        assert!(
            !reclosed.contains(PEER_BODY_PHRASE),
            "the second click must CLOSE the node, not re-open it"
        );
        assert!(
            reclosed.contains(COLLAPSED_AFFORDANCE),
            "and the header must go back to offering the expand"
        );
        assert!(
            app.modal.is_none(),
            "toggling a node must not open an overlay"
        );
    }

    /// A body with a markdown link, written as a REAL peer record: the checked-in
    /// fixture's hand-back carries no link (real reports rarely do), and the
    /// precedence test below needs a link that is reachable ONLY once the node is
    /// open.
    fn peer_link_session(dir: &Path) -> Session {
        let file = dir.join("sess-peer-link.jsonl");
        let body = format!(
            "[Subagent hand-back] The report follows:\\n  Findings are in [docs]({LINK_URL}) \
             for review."
        );
        let jsonl = format!(
            concat!(
                r#"{{"type":"user","sessionId":"sess-peer-link","cwd":"/tmp","#,
                r#""timestamp":"2026-07-01T10:00:00.000Z","#,
                r#""origin":{{"kind":"peer","from":"{key}","senderTaskId":"{key}","#,
                r#""handback":true,"body":"{body}"}},"#,
                r#""message":{{"role":"user","content":"ignored: the node renders from origin"}}}}"#,
                "\n",
            ),
            key = PEER_KEY,
            body = body,
        );
        std::fs::write(&file, jsonl).expect("write the peer-link fixture");
        let mut s = session("sess-peer-link");
        s.file = file;
        s
    }

    /// Fold-before-link does NOT swallow a click on a link inside an OPEN node: the
    /// header is the node's click target, the body is not.
    ///
    /// Asserted through the pure [`fold_under_pointer`] / [`resolve_link_click`]
    /// seam rather than through `handle_event`, because taking the click's link
    /// branch there would reach `resume::open_url` and spawn a browser — the same
    /// reason the link hit-tests above stop at this seam.
    #[test]
    fn a_click_on_a_link_inside_an_expanded_peer_node_still_opens_the_link() {
        let dir = unique_temp_dir("peer-link");
        let mut app = App::new(
            vec![peer_link_session(&dir)],
            Scope::All,
            PathBuf::from("/tmp"),
        );
        let buffer = render_board(&mut app);

        // Open the node first: its body — and so its link — does not exist until a
        // click puts it on screen, which is the whole point of the fold.
        let (header_col, header_row) = drawn_peer_handle_cell(&buffer, app.preview_rect);
        left_click(&mut app, header_col, header_row);
        let buffer = render_board(&mut app);

        let (col, row) = drawn_link_cell(&buffer, app.preview_rect);
        assert_eq!(
            fold_under_pointer(&mut app, col, row),
            None,
            "the node's BODY is not its click target — only the header is, or a \
             fold-first arm would swallow every link a report contains"
        );
        assert_eq!(
            resolve_link_click(&mut app, col, row),
            LinkClick::Opening(LINK_URL.to_string()),
            "so the arm falls through to the link, exactly as it does outside a node"
        );

        // And the header is still the node's own target, not the link's.
        let (header_col, header_row) = drawn_peer_handle_cell(&buffer, app.preview_rect);
        assert_eq!(
            fold_under_pointer(&mut app, header_col, header_row).as_deref(),
            Some(PEER_KEY),
        );
        assert_eq!(
            resolve_link_click(&mut app, header_col, header_row),
            LinkClick::NoLink
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The premise the fold-then-link precedence rests on, measured against the
    /// REAL render instead of assumed: a node's header row carries no link region,
    /// in EITHER fold state and at every pane width the node is readable at.
    ///
    /// If this ever goes red the ordering inside `click_effect` stops being free
    /// and starts deciding a collision, which is why its doc comment names this
    /// test by name.
    #[test]
    fn a_peer_node_header_carries_no_link_regions_at_any_width() {
        let dir = unique_temp_dir("peer-premise");
        let mut app = App::new(
            vec![peer_link_session(&dir)],
            Scope::All,
            PathBuf::from("/tmp"),
        );
        // Widths from comfortable down to narrow enough that the header itself
        // soft-wraps, so a wrapped header is covered too.
        for width in [80u16, 40, 24, 16] {
            for open in [false, true] {
                if open {
                    app.toggle_peer_fold(PEER_KEY, width);
                }
                let (_row_prefix, _lines, links, folds) = app
                    .preview_hit_context(width)
                    .expect("the fixture session is selected, so it has a preview");
                assert!(
                    !folds.is_empty(),
                    "the node must render a fold region at width {width}, \
                     or this width proves nothing"
                );
                for fold in folds {
                    assert!(
                        !links.iter().any(|l| l.content_row == fold.content_row),
                        "a header row must carry no link at width {width} \
                         (open={open}), or fold-before-link stops being free"
                    );
                }
                if open {
                    assert!(
                        !links.is_empty(),
                        "the open node's body must contribute links at width \
                         {width}, or the disjointness above is vacuous"
                    );
                    app.toggle_peer_fold(PEER_KEY, width);
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A click on the preview pane's own left BORDER does nothing, even when the row
    /// it lands on is a peer node's header.
    ///
    /// The probe is the preview block's left border column — chrome the pane's
    /// `Block` draws and the transcript never reaches. It is inside `preview_rect`,
    /// the pane's OUTER rect, so it IS a click on the pane, and nothing runs ahead
    /// of the pane click to claim the cell: the pane widths belong to the keyboard,
    /// so the mouse has no border to drag. What keeps the click inert is
    /// containment, twice over. The press gate (`press_starts_selection`) admits
    /// presses in the TRANSCRIPT rect alone, so a border press records nothing and
    /// its release has nothing to resolve; and behind it the hit-test's own
    /// containment — `view::content_hit` refuses every cell outside the
    /// transcript's INNER rect — would still resolve the border to no content
    /// column at all. Clamp that column instead and the border aliases onto content
    /// column 0, the header's marker one cell to its RIGHT, and this click opens the
    /// node.
    ///
    /// The marker itself is the node's own click target
    /// (`a_click_on_a_peer_nodes_marker_cell_expands_it`). The containment guard is
    /// also pinned where it lives, against flush-left regions at content column 0
    /// (the only shape a clamped column can be caught by):
    /// `view::tests::link_at_is_none_left_or_right_of_the_inner_rect` and
    /// `view::tests::fold_at_is_none_on_blank_rows_and_outside_the_pane`.
    #[test]
    fn a_click_on_the_preview_border_beside_a_peer_node_toggles_nothing() {
        let (mut app, buffer) = peer_app();
        let width = view::preview_transcript_rect(&app).width;
        let (marker_col, header_row) = drawn_peer_marker_cell(&buffer, app.preview_rect);
        let col = app.preview_rect.x;
        assert_eq!(
            marker_col,
            col + 1,
            "the header's leftmost drawn cell is the pane's first CONTENT column, \
             one past the border this probe sits on"
        );
        assert!(
            app.preview_rect.contains(Position {
                x: col,
                y: header_row,
            }),
            "the probe must be inside the pane, or this is a click somewhere else \
             and the silence below proves nothing about the border"
        );
        assert!(
            !view::preview_transcript_rect(&app).contains(Position {
                x: col,
                y: header_row,
            }),
            "and it must be outside the transcript rect, or it is not the border"
        );
        let collapsed = preview_string(&mut app, width);
        assert!(
            collapsed.contains(COLLAPSED_AFFORDANCE) && !collapsed.contains(PEER_BODY_PHRASE),
            "a peer node starts CLOSED, or the assertion below is vacuous"
        );

        left_click(&mut app, col, header_row);
        let after = preview_string(&mut app, width);
        assert!(
            after.contains(COLLAPSED_AFFORDANCE) && !after.contains(PEER_BODY_PHRASE),
            "a click on the border must not alias onto the marker beside it and \
             open the node"
        );
        assert!(app.modal.is_none(), "nor open an overlay");
    }

    /// A click on the node header's MARKER cell — content column 0, the glyph the
    /// "(click to expand)" affordance is promising about — opens the node.
    ///
    /// The regression guard for the mouse splitter this board once had. Its grab
    /// band was symmetric around the seam, so it claimed `seam + 1` as well; since
    /// the pane's border is always exactly one column, `seam + 1` is always the
    /// pane's first CONTENT column, and the seam arm ran before the pane arm. The one
    /// cell that said "click to expand" was the one cell that could not be clicked,
    /// at every terminal size and every split ratio. The pane widths now belong to
    /// the keyboard, and this keeps any future arm from taking the cell back.
    ///
    /// Driven END TO END through [`handle_mouse`] — press, then release — not
    /// through [`fold_under_pointer`]: the defect was never in the resolver — it is
    /// which arm claims the click.
    #[test]
    fn a_click_on_a_peer_nodes_marker_cell_expands_it() {
        let (mut app, buffer) = peer_app();
        let width = view::preview_transcript_rect(&app).width;
        let (col, row) = drawn_peer_marker_cell(&buffer, app.preview_rect);
        assert_eq!(
            col,
            app.preview_rect.x + 1,
            "the marker must really be drawn on the pane's first CONTENT column, or \
             this probes an ordinary interior cell"
        );

        let collapsed = preview_string(&mut app, width);
        assert!(
            collapsed.contains(COLLAPSED_AFFORDANCE) && !collapsed.contains(PEER_BODY_PHRASE),
            "a peer node starts CLOSED, or the expand assertion below is vacuous"
        );

        left_click(&mut app, col, row);
        let expanded = preview_string(&mut app, width);
        assert!(
            expanded.contains(PEER_BODY_PHRASE),
            "the marker is content, not chrome: the click must open the node the \
             marker labels"
        );
        assert!(
            expanded.contains(EXPANDED_AFFORDANCE),
            "so the header now offers the click that closes it again"
        );
    }

    /// The PRESS on a node header toggles nothing: whether it is a click or the
    /// start of a drag is only known when the button comes back up, so the fold
    /// waits for the release — exactly as a link does. The release of that same
    /// press then opens the node, and asks the driver for nothing more.
    #[test]
    fn a_press_alone_on_a_peer_node_header_toggles_nothing_until_the_release() {
        let (mut app, buffer) = peer_app();
        let width = view::preview_transcript_rect(&app).width;
        let (col, row) = drawn_peer_handle_cell(&buffer, app.preview_rect);

        let pressed = mouse_at(
            &mut app,
            mouse_ev(MouseEventKind::Down(MouseButton::Left), col, row),
        );
        assert_eq!(pressed, MouseEffect::None, "a press asks for nothing");
        let after_press = preview_string(&mut app, width);
        assert!(
            after_press.contains(COLLAPSED_AFFORDANCE) && !after_press.contains(PEER_BODY_PHRASE),
            "a press alone must leave the node CLOSED"
        );

        let released = mouse_at(
            &mut app,
            mouse_ev(MouseEventKind::Up(MouseButton::Left), col, row),
        );
        assert_eq!(released, MouseEffect::None, "a toggle opens no link");
        let after_release = preview_string(&mut app, width);
        assert!(
            after_release.contains(PEER_BODY_PHRASE) && after_release.contains(EXPANDED_AFFORDANCE),
            "the release of that press is what opens the node"
        );
    }

    /// A drag that STARTS on a node header is a selection, not a click: its release
    /// copies the header text it covered and toggles NOTHING — the fold's twin of
    /// `a_drag_that_starts_on_a_link_copies_instead_of_opening_it`.
    #[test]
    fn a_drag_that_starts_on_a_peer_node_header_selects_and_toggles_nothing() {
        let (mut app, buffer) = peer_app();
        let width = view::preview_transcript_rect(&app).width;
        let (col, row) = drawn_peer_handle_cell(&buffer, app.preview_rect);

        let released = drag_and_release(&mut app, (col, row), (col + 3, row));

        let Outcome::Copy(CopyPayload::Selection(text)) = released else {
            panic!("a drag's release must copy a selection, not toggle the node");
        };
        assert!(
            text.starts_with('@'),
            "the copy is the header text the drag covered, from the handle on: {text:?}"
        );
        let after = preview_string(&mut app, width);
        assert!(
            after.contains(COLLAPSED_AFFORDANCE) && !after.contains(PEER_BODY_PHRASE),
            "a drag that started on the header must leave the node CLOSED"
        );
    }

    /// A fold toggle drops a standing drag-selection. The toggle re-renders the
    /// transcript — lines, row map and scroll all move — so the selection's
    /// content anchors would now highlight, and a later release would copy, other
    /// text. Toggled through `App::toggle_peer_fold` DIRECTLY, not through a click,
    /// on purpose: a click's own press already starts a fresh selection, so only a
    /// direct call can show the clear lives in the toggle itself, where no route
    /// into it can skip it. Asserted on the next frame too, so no stale highlight
    /// survives the redraw.
    #[test]
    fn a_fold_toggle_clears_a_standing_preview_selection() {
        let (mut app, buffer) = peer_app();
        let width = view::preview_transcript_rect(&app).width;
        let (col, row) = drawn_peer_handle_cell(&buffer, app.preview_rect);
        drag_and_release(&mut app, (col, row), (col + 3, row));
        let drawn = render_board(&mut app);
        assert!(
            app.has_preview_selection()
                && !highlighted_cells(&drawn, view::preview_transcript_rect(&app)).is_empty(),
            "premise: a finished drag stays selected and highlighted"
        );

        app.toggle_peer_fold(PEER_KEY, width);

        assert!(
            !app.has_preview_selection(),
            "a fold toggle must drop the selection"
        );
        assert_eq!(
            view::preview_selection_copy(&mut app),
            None,
            "and the text a release would have copied"
        );
        let redrawn = render_board(&mut app);
        assert_eq!(
            highlighted_cells(&redrawn, view::preview_transcript_rect(&app)),
            Vec::<(u16, u16)>::new(),
            "and no highlight may survive into the next frame"
        );
    }

    /// [`peer_app`] with the pane UNPINNED from the newest turn — one wheel notch
    /// up, the way a reader who scrolled up to the node left it. Pinned to the
    /// bottom, an expansion re-pins the pane and scrolls the node's header off the
    /// row it was clicked on; unpinned, the toggle keeps it there
    /// (`fold_scroll_delta`), which is the premise a click on the SAME cell right
    /// after the toggle needs. Returns the app beside the buffer drawn after the
    /// notch.
    fn unpinned_peer_app() -> (App, ratatui::buffer::Buffer) {
        let (mut app, _) = peer_app();
        let rect = view::preview_transcript_rect(&app);
        wheel(&mut app, MouseEventKind::ScrollUp, rect.x, rect.y);
        assert!(
            !app.preview_follow_bottom,
            "premise: a notch up unpins the pane from the newest turn"
        );
        let buffer = render_board(&mut app);
        (app, buffer)
    }

    /// A DOUBLE-click on a FOLDED node's header opens the node ONCE and copies the
    /// header word under the pointer. The first release is a plain click, so it
    /// toggles the node open; the toggle drops the selection but KEEPS the click
    /// chain (`App::drop_preview_selection`), and it keeps the header on the row it
    /// was clicked on, so the second press is a double-click on the same header.
    /// Its release copies the word and toggles nothing — the node stays open. A
    /// toggle that reset the chain would read the second press as a fresh click and
    /// shut the node again, copying nothing.
    #[test]
    fn a_double_click_on_a_folded_peer_header_opens_it_once_and_copies_the_word() {
        let (mut app, buffer) = unpinned_peer_app();
        let width = view::preview_transcript_rect(&app).width;
        let (_, header_row) = drawn_peer_marker_cell(&buffer, app.preview_rect);
        let cell = inside_word(&mut app, "message from @");
        assert_eq!(
            cell.1, header_row,
            "the probe must sit on the node's header"
        );
        let collapsed = preview_string(&mut app, width);
        assert!(
            collapsed.contains(COLLAPSED_AFFORDANCE) && !collapsed.contains(PEER_BODY_PHRASE),
            "a peer node starts CLOSED, or the open-once assertion is vacuous"
        );
        let t0 = Instant::now();

        let first = click_at(&mut app, cell, t0);
        assert_eq!(first, MouseEffect::None, "a toggle asks for nothing more");
        assert!(
            preview_string(&mut app, width).contains(PEER_BODY_PHRASE),
            "the FIRST release is a plain click: it opens the node"
        );
        let opened = render_board(&mut app);
        assert_eq!(
            drawn_peer_marker_cell(&opened, app.preview_rect).1,
            header_row,
            "premise: the toggle kept the header on the row it was clicked on"
        );

        let second = click_at(&mut app, cell, t0 + Duration::from_millis(100));
        assert_eq!(
            second,
            MouseEffect::Copy("message".to_string()),
            "the SECOND press is a double-click: its release copies the header word"
        );
        let after = preview_string(&mut app, width);
        assert!(
            after.contains(PEER_BODY_PHRASE) && after.contains(EXPANDED_AFFORDANCE),
            "and toggles nothing: the node was opened once and stays open"
        );
    }

    /// The chain a fold toggle keeps is still reset by a KEY, as the double-click
    /// rule says: after a click that opened a node, any keypress makes the next
    /// press on the same cell a plain click again — which toggles the node shut.
    #[test]
    fn a_key_after_a_fold_toggle_resets_the_click_chain() {
        let (mut app, buffer) = unpinned_peer_app();
        let width = view::preview_transcript_rect(&app).width;
        let cell = drawn_peer_handle_cell(&buffer, app.preview_rect);
        let t0 = Instant::now();

        click_at(&mut app, cell, t0);
        assert!(
            preview_string(&mut app, width).contains(PEER_BODY_PHRASE),
            "premise: the first click opened the node"
        );
        // `Backspace` on the empty query changes nothing on the board — only the
        // any-key chain reset in `dispatch` acts.
        assert_eq!(app.query(), "");
        press(&mut app, KeyCode::Backspace);

        let second = click_at(&mut app, cell, t0 + Duration::from_millis(100));
        assert_eq!(second, MouseEffect::None, "a plain click copies nothing");
        let reclosed = preview_string(&mut app, width);
        assert!(
            !reclosed.contains(PEER_BODY_PHRASE) && reclosed.contains(COLLAPSED_AFFORDANCE),
            "with the chain reset, the quick second press is a plain click that \
             toggles the node shut"
        );
    }

    /// A click on a node header while an overlay owns input does NEITHER: no
    /// toggle, no link open, and the overlay is left exactly as it was.
    ///
    /// The same `!app.preview_pointer_blocked()` gate the link arm always had, which the
    /// fold must not quietly widen. The overlay here is a modal (the running-session
    /// choice); an open quick reply is NOT one, and
    /// `a_peer_node_header_toggles_while_a_reply_is_open` pins that half — asked of the PRESS (`press_starts_selection`),
    /// so the whole click, release included, is inert, and no selection starts
    /// either.
    #[test]
    fn a_click_on_a_peer_node_header_during_an_overlay_neither_toggles_nor_opens() {
        let (mut app, buffer) = peer_app();
        let width = view::preview_transcript_rect(&app).width;
        let (col, row) = drawn_peer_handle_cell(&buffer, app.preview_rect);
        assert_eq!(
            fold_under_pointer(&mut app, col, row).as_deref(),
            Some(PEER_KEY),
            "the probe must be a cell that WOULD toggle, or the silence below is \
             the geometry's and not the gate's"
        );
        app.open_live_choice("s1".to_string());
        assert!(
            app.preview_pointer_blocked(),
            "the overlay must really own input"
        );

        let released = left_click(&mut app, col, row);
        let after = preview_string(&mut app, width);
        assert!(
            !after.contains(PEER_BODY_PHRASE) && after.contains(COLLAPSED_AFFORDANCE),
            "a click behind an overlay must leave the node closed"
        );
        assert!(
            matches!(released, Outcome::Continue) && !app.has_preview_selection(),
            "and must neither ask the driver for anything nor start a selection"
        );
        assert!(
            app.modal.is_some(),
            "and must not disturb the overlay either"
        );
    }

    // --- injected-context fold: the same pane click, the same premise --------

    /// The injected record's `uuid` in [`injected_link_session`].
    const INJECTED_UUID: &str = "inj-link-1";
    /// That record's fold key: its `uuid` behind the injected prefix.
    /// `store::preview` owns the prefix; it is restated here on purpose, for the
    /// same reason [`COLLAPSED_AFFORDANCE`] is.
    const INJECTED_KEY: &str = "injected:inj-link-1";
    /// A phrase carried ONLY by the injected body, so the node's state is read the
    /// way a user reads it: on screen when OPEN, absent when CLOSED.
    const INJECTED_BODY_PHRASE: &str = "follow every review step";

    /// A session holding ONE injected (`isMeta`, non-peer) record whose body OPENS
    /// with a markdown link, written as a real file under `dir`.
    ///
    /// The link sits at the body's content column 0 on purpose: that is the
    /// column the header's marker is drawn on, so if the body's links were ever
    /// rebased onto the header row, the header's own leftmost cell would carry a
    /// link and a click there would stop being the node's alone.
    fn injected_link_session(dir: &Path) -> Session {
        let file = dir.join("sess-injected-link.jsonl");
        let body = format!("[guide]({LINK_URL}) explains the task.\\n\\n{INJECTED_BODY_PHRASE}.");
        let jsonl = format!(
            concat!(
                r#"{{"type":"user","sessionId":"sess-injected-link","cwd":"/tmp","#,
                r#""timestamp":"2026-07-01T10:00:00.000Z","uuid":"{uuid}","isMeta":true,"#,
                r#""message":{{"role":"user","content":"{body}"}}}}"#,
                "\n",
            ),
            uuid = INJECTED_UUID,
            body = body,
        );
        std::fs::write(&file, jsonl).expect("write the injected-link fixture");
        let mut s = session("sess-injected-link");
        s.file = file;
        s
    }

    /// The injected node header's LEFTMOST drawn cell — its `◇` marker, on the
    /// pane's first CONTENT column. Scanned from the drawn buffer, never computed.
    fn drawn_injected_marker_cell(buffer: &ratatui::buffer::Buffer, preview: Rect) -> (u16, u16) {
        drawn_cell(buffer, preview, "\u{25c7}").expect(
            "the fixture's injected node must be drawn inside the preview pane, \
             or these tests prove nothing",
        )
    }

    /// The premise fold-before-link rests on, for the INJECTED node kind: its
    /// header row carries no link region, in EITHER fold state and at every pane
    /// width the node is readable at. The peer kind's twin is
    /// `a_peer_node_header_carries_no_link_regions_at_any_width`; the arm's doc
    /// comment names both, because each kind builds its header on its own path.
    #[test]
    fn an_injected_node_header_carries_no_link_regions_at_any_width() {
        let dir = unique_temp_dir("injected-premise");
        let mut app = App::new(
            vec![injected_link_session(&dir)],
            Scope::All,
            PathBuf::from("/tmp"),
        );
        for width in [80u16, 40, 24, 16] {
            for open in [false, true] {
                if open {
                    app.toggle_peer_fold(INJECTED_KEY, width);
                }
                let (_row_prefix, _lines, links, folds) = app
                    .preview_hit_context(width)
                    .expect("the fixture session is selected, so it has a preview");
                assert!(
                    folds.iter().any(|f| f.key == INJECTED_KEY),
                    "the injected node must render its fold region at width {width}, \
                     or this width proves nothing"
                );
                for fold in folds {
                    assert!(
                        !links.iter().any(|l| l.content_row == fold.content_row),
                        "an injected header row must carry no link at width {width} \
                         (open={open}), or fold-before-link stops being free"
                    );
                }
                if open {
                    assert!(
                        !links.is_empty(),
                        "the open node's body must contribute links at width \
                         {width}, or the disjointness above is vacuous"
                    );
                    app.toggle_peer_fold(INJECTED_KEY, width);
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A real click on an INJECTED node's header toggles THAT node — open, then
    /// closed again — and is never taken for a link, while a link inside the open
    /// body still is one.
    ///
    /// Driven END TO END through [`handle_mouse`] — press, then release — like the
    /// peer node's click tests: a resolver nothing routes to would pass every pure
    /// assertion and still leave the node inert. Each click is preceded by the
    /// pure seam's verdict for that cell — a fold key and NO url — so the click can
    /// only ever take its fold branch here and never reaches `resume::open_url`.
    #[test]
    fn a_click_on_an_injected_node_header_toggles_it_and_never_opens_a_link() {
        let dir = unique_temp_dir("injected-click");
        let mut app = App::new(
            vec![injected_link_session(&dir)],
            Scope::All,
            PathBuf::from("/tmp"),
        );
        let buffer = render_board(&mut app);
        let width = view::preview_transcript_rect(&app).width;
        let (col, row) = drawn_injected_marker_cell(&buffer, app.preview_rect);
        assert!(
            view::preview_transcript_rect(&app).contains(Position { x: col, y: row }),
            "the probe must be inside the transcript rect the press gate admits"
        );

        let collapsed = preview_string(&mut app, width);
        assert!(
            collapsed.contains(COLLAPSED_AFFORDANCE) && !collapsed.contains(INJECTED_BODY_PHRASE),
            "an injected node starts CLOSED, or the expand assertion below is vacuous"
        );
        assert_eq!(
            resolve_link_click(&mut app, col, row),
            LinkClick::NoLink,
            "a closed injected header must carry no link"
        );
        assert_eq!(
            fold_under_pointer(&mut app, col, row).as_deref(),
            Some(INJECTED_KEY),
            "the header must be the injected node's own click target"
        );

        left_click(&mut app, col, row);
        let expanded = preview_string(&mut app, width);
        assert!(
            expanded.contains(INJECTED_BODY_PHRASE) && expanded.contains(EXPANDED_AFFORDANCE),
            "the click must open the injected node's body under a header offering \
             to close it"
        );

        // Open, the header is still the node's alone — its body's link is not on it.
        let buffer = render_board(&mut app);
        assert_eq!(
            drawn_injected_marker_cell(&buffer, app.preview_rect),
            (col, row),
            "opening a node must leave its header where it was clicked"
        );
        assert_eq!(
            resolve_link_click(&mut app, col, row),
            LinkClick::NoLink,
            "an open injected header must carry no link either"
        );
        assert_eq!(
            fold_under_pointer(&mut app, col, row).as_deref(),
            Some(INJECTED_KEY),
        );
        let (link_col, link_row) = drawn_link_cell(&buffer, app.preview_rect);
        assert_ne!(link_row, row, "the body's link is drawn below the header");
        assert_eq!(
            fold_under_pointer(&mut app, link_col, link_row),
            None,
            "the node's BODY is not its click target"
        );
        assert_eq!(
            resolve_link_click(&mut app, link_col, link_row),
            LinkClick::Opening(LINK_URL.to_string()),
            "so a click on the body's link falls through to the link"
        );

        // A SEPARATE second click, as in the peer twin above: a quick one on the
        // same cell is a double-click, which selects a word and toggles nothing.
        separate_left_click(&mut app, col, row);
        let reclosed = preview_string(&mut app, width);
        assert!(
            !reclosed.contains(INJECTED_BODY_PHRASE) && reclosed.contains(COLLAPSED_AFFORDANCE),
            "the second click must CLOSE the node again"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- peer-message fold under the pinned row: one geometry, both shapes ----

    /// A click on a FOLDED node's header opens it while the pinned row is
    /// reserved above the transcript.
    ///
    /// Every selected session reserves that row (`view::preview_banner`), so the
    /// node's header is drawn one row lower than the pane's first inner row, and
    /// the pane click must resolve against that SAME lower rect
    /// ([`view::preview_transcript_rect`]). Asserted on the precondition first, so a
    /// board that stopped pinning the row could not pass this as the banner-less
    /// case in disguise.
    ///
    /// The probe is the header's MARKER cell, the pane's first CONTENT column, so
    /// the one click also pins that column as the transcript's on the rows beneath
    /// the pinned one. And the row directly ABOVE the header,
    /// the node's blank separator, is clicked first and must stay inert: a
    /// hit-test that forgot the reserved row resolves that row to the header.
    #[test]
    fn a_click_on_a_folded_peer_node_opens_it_beneath_the_pinned_row() {
        let (mut app, buffer) = peer_app();
        assert!(
            view::preview_banner(&app).is_some(),
            "every selected session pins the row, or this is the banner-less case"
        );
        let transcript = view::preview_transcript_rect(&app);
        assert_eq!(
            transcript.y,
            app.preview_rect.y + 2,
            "the transcript must start one row below the pane's first inner row"
        );
        let width = transcript.width;
        let (col, row) = drawn_peer_marker_cell(&buffer, app.preview_rect);
        assert_eq!(
            col,
            app.preview_rect.x + 1,
            "the marker must sit on the pane's first CONTENT column"
        );
        assert!(
            row > transcript.y,
            "the header must be drawn below the transcript's first row, so the row \
             above it is transcript rather than the pinned row"
        );

        left_click(&mut app, col, row - 1);
        let untouched = preview_string(&mut app, width);
        assert!(
            untouched.contains(COLLAPSED_AFFORDANCE) && !untouched.contains(PEER_BODY_PHRASE),
            "the row above the header is the node's blank separator and must not \
             toggle it"
        );

        left_click(&mut app, col, row);
        let expanded = preview_string(&mut app, width);
        assert!(
            expanded.contains(PEER_BODY_PHRASE) && expanded.contains(EXPANDED_AFFORDANCE),
            "a click on the header drawn beneath the pinned row must open the node"
        );
    }

    /// The banner-less half of the same seam: while a quick reply to the selected
    /// session is in flight, its inline echo turns take the pinned row's place
    /// (`view::preview_banner` is `None`), so the transcript owns the pane's whole
    /// inner rect and a fold click must NOT shift by a row that was never
    /// reserved.
    ///
    /// The blank BELOW the collapsed header, the one that leads the next turn, is
    /// clicked first and must stay inert: a hit-test that reserved a row anyway
    /// resolves that row to the header's last drawn row.
    #[test]
    fn a_click_on_a_folded_peer_node_opens_it_in_a_banner_less_pane() {
        let (folder, file) = PEER_FIXTURE;
        let mut app = App::new(
            vec![fixture_session("s1", folder, file)],
            Scope::All,
            PathBuf::from("/tmp"),
        );
        app.sending = vec![crate::tui::app::Sending {
            session_id: "s1".to_string(),
            message: "thanks".to_string(),
            baseline_msg_count: 0,
        }];
        let buffer = render_board(&mut app);
        assert!(
            view::preview_banner(&app).is_none(),
            "an in-flight reply reserves no pinned row"
        );
        let transcript = view::preview_transcript_rect(&app);
        assert_eq!(
            transcript.y,
            app.preview_rect.y + 1,
            "with no pinned row the transcript owns the pane's first inner row"
        );
        let width = transcript.width;
        let (col, row) = drawn_peer_marker_cell(&buffer, app.preview_rect);
        assert_eq!(
            col,
            app.preview_rect.x + 1,
            "the marker must sit on the pane's first CONTENT column"
        );

        // The header may soft-wrap, so the next turn's blank is found by where the
        // next turn's `●` marker was DRAWN: it is the row directly above it.
        let next_turn = (row + 1..app.preview_rect.bottom())
            .find(|&y| {
                buffer
                    .cell((col, y))
                    .is_some_and(|c| c.symbol() == "\u{25cf}")
            })
            .expect("the fixture draws a claude turn below the node");
        left_click(&mut app, col, next_turn - 1);
        let untouched = preview_string(&mut app, width);
        assert!(
            untouched.contains(COLLAPSED_AFFORDANCE) && !untouched.contains(PEER_BODY_PHRASE),
            "the blank below a collapsed header leads the next turn and must not \
             toggle the node"
        );

        left_click(&mut app, col, row);
        let expanded = preview_string(&mut app, width);
        assert!(
            expanded.contains(PEER_BODY_PHRASE) && expanded.contains(EXPANDED_AFFORDANCE),
            "a click on the header of a banner-less pane must open the node"
        );
    }

    /// A link label that starts its line, on the pane's FIRST content column,
    /// stays clickable beneath the pinned row: the pane click owns that column
    /// there too, and resolves the cell against the transcript rect the pinned row
    /// pushed down.
    ///
    /// Stops at the pure [`resolve_link_click`] seam, like its siblings, because
    /// the click's link branch through `handle_event` would spawn a browser.
    #[test]
    fn a_link_on_the_first_content_column_opens_beneath_the_pinned_row() {
        let dir = unique_temp_dir("link-col0-pinned");
        let mut app = App::new(
            vec![link_at_column_zero_session(&dir)],
            Scope::All,
            PathBuf::from("/tmp"),
        );
        let buffer = render_board(&mut app);
        assert!(
            view::preview_banner(&app).is_some(),
            "every selected session pins the row, or this is the banner-less case"
        );
        let transcript = view::preview_transcript_rect(&app);
        assert_eq!(
            transcript.y,
            app.preview_rect.y + 2,
            "the transcript must start one row below the pane's first inner row"
        );

        let (col, row) = drawn_link_cell(&buffer, app.preview_rect);
        assert_eq!(
            col,
            app.preview_rect.x + 1,
            "the label must sit on the pane's first CONTENT column"
        );
        assert!(
            transcript.contains(Position { x: col, y: row }),
            "the first content column is inside the transcript rect the press gate \
             admits"
        );
        assert_eq!(
            resolve_link_click(&mut app, col, row),
            LinkClick::Opening(LINK_URL.to_string()),
            "the cell the label was DRAWN on must resolve to its url"
        );
        assert_ne!(
            resolve_link_click(&mut app, col, row - 1),
            LinkClick::Opening(LINK_URL.to_string()),
            "the row above the label is the blank that leads its paragraph"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Task VERIFY-4: Enter on a LIVE session enters the choice-overlay state
    /// (not the resume path); navigating + confirming Cancel returns to the board.
    #[test]
    fn enter_on_live_session_opens_choice_overlay_then_cancel_returns_to_board() {
        let mut app = app_with("live-1", Some("background"));
        assert!(app.modal.is_none());

        let out = press(&mut app, KeyCode::Enter);
        assert!(matches!(out, Outcome::Continue));
        let modal = app
            .modal
            .clone()
            .expect("Enter on a live row opens the overlay");
        assert_eq!(modal.session_id.as_deref(), Some("live-1"));
        assert_eq!(
            modal.selected_action(),
            Some(&ModalAction::Attach),
            "defaults to the Attach choice"
        );

        // Overlay owns the keyboard: → cycles Attach -> Fork -> Cancel.
        press(&mut app, KeyCode::Right);
        assert_eq!(
            app.modal.as_ref().unwrap().selected_action(),
            Some(&ModalAction::Fork)
        );
        press(&mut app, KeyCode::Right);
        assert_eq!(
            app.modal.as_ref().unwrap().selected_action(),
            Some(&ModalAction::Cancel)
        );

        // Confirming Cancel dismisses the overlay and stays on the board.
        let out = press(&mut app, KeyCode::Enter);
        assert!(matches!(out, Outcome::Continue));
        assert!(app.modal.is_none(), "Cancel returns to the board");
    }

    /// Confirming Attach on an INTERACTIVE live session (no agent-view job id)
    /// must refuse with a board status instead of escalating to a hand-off — a
    /// broken `claude attach <uuid>` is never spawned. Proves `route_handoff`
    /// resolves the live agent's (absent) `id` and routes it through the gate.
    #[test]
    fn confirming_attach_on_an_interactive_session_refuses_without_a_handoff() {
        let mut app = app_with("live-1", Some("interactive"));
        // Enter opens the overlay defaulting to Attach; a second Enter confirms it.
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            app.modal.as_ref().unwrap().selected_action(),
            Some(&ModalAction::Attach)
        );
        let out = press(&mut app, KeyCode::Enter);

        // Stays on the board (no Resume escalation) with the no-job hint shown.
        assert!(
            matches!(out, Outcome::Continue),
            "an interactive session must not escalate to a hand-off"
        );
        assert!(app.modal.is_none(), "the overlay closes on confirm");
        assert_eq!(app.status.as_deref(), Some(resume::ATTACH_NO_JOB_ID));
    }

    /// Task VERIFY-4: Enter on a NON-live session takes the resume path, never
    /// the overlay (asserted by the overlay state staying closed).
    #[test]
    fn enter_on_non_live_session_takes_the_resume_path_not_the_overlay() {
        let mut app = app_with("plain-1", None);
        let out = press(&mut app, KeyCode::Enter);
        // The synthetic cwd/file are not resumable, so the gate refuses and sets
        // a status — the key invariant is the OVERLAY state was never entered.
        assert!(matches!(out, Outcome::Continue));
        assert!(
            app.modal.is_none(),
            "a non-live Enter must not open the overlay"
        );
    }

    /// A session written to a REAL file whose `cwd` REALLY exists, so
    /// `resume::check`'s authoritative re-read and existence gate both PASS and
    /// Enter can reach an actual `Outcome::Resume`.
    ///
    /// The synthetic `session()` above cannot: its file does not exist, so every
    /// Enter refuses and the resume path is indistinguishable from a no-op. To
    /// prove a `done` session PLAIN-RESUMES (not merely "did not open the
    /// overlay"), the gate has to be allowed to succeed.
    fn resumable_session(dir: &Path, id: &str) -> Session {
        let file = dir.join(format!("{id}.jsonl"));
        let jsonl = format!(
            concat!(
                r#"{{"type":"user","sessionId":"{id}","cwd":"{cwd}","#,
                r#""timestamp":"2026-07-14T10:00:00.000Z","#,
                r#""message":{{"role":"user","content":"hi"}}}}"#,
                "\n",
            ),
            id = id,
            cwd = dir.display(),
        );
        std::fs::write(&file, jsonl).expect("write the resumable fixture");
        Session {
            file,
            session_id: id.to_string(),
            cwd: dir.to_path_buf(),
            git_branch: Some("main".to_string()),
            timestamp: None,
            repo: "repo".to_string(),
            label: format!("label {id}"),
            root_uuid: None,
            msg_count: 0,
            content_index: String::new(),
            background: false,
            has_agent_name: false,
            has_agent_setting: false,
            failed_task: None,
        }
    }

    /// An app over one REPORTABLE, resumable session joined to a background agent
    /// carrying `state` — the shape `claude agents --json --all` reports — and,
    /// SEPARATELY, what claude's active list says via `live`.
    ///
    /// The two are independent parameters on purpose: the whole bug is that the
    /// polled `--all` badge state and claude's live answer CAN DISAGREE. A helper
    /// that derived one from the other could not express the race at all.
    fn app_with_agent_state(dir: &Path, id: &str, state: &str, live: &[&str]) -> App {
        let mut app = App::new(
            vec![resumable_session(dir, id)],
            Scope::All,
            PathBuf::from("/tmp"),
        );
        let mut reported = HashMap::new();
        reported.insert(
            id.to_string(),
            ReportedAgent {
                kind: "background".to_string(),
                // A real `--all` record carries its agent-view job id; supplying
                // one means a wrongly-opened overlay would be fully functional,
                // so this test fails ONLY on the routing decision itself.
                id: Some("job-1".to_string()),
                state: Some(state.to_string()),
                status: None,
                pid: None,
                started_at_ms: None,
            },
        );
        app.set_reported_agents(reported, None);
        seed_live(&mut app, live);
        assert_eq!(app.selected.as_deref(), Some(id));
        app
    }

    /// THE regression `--all` could cause. The agent map carries agents that
    /// reported completion, so a MEMBERSHIP test against THAT map would divert
    /// Enter into the Attach/Fork/Cancel overlay for every session that ever
    /// finished — i.e. for the large majority of rows — breaking the board's
    /// PRIMARY interaction.
    ///
    /// The intent is unchanged from when the gate classified `done`; only the
    /// premise is now stated the way the gate actually asks it — claude's active
    /// list does NOT report this session. Asserts the OBSERVABLE outcome, never a
    /// predicate's return value: a real `Outcome::Resume` carrying the PLAIN
    /// `claude -r <id>` argv. No `claude` is spawned — the probe is seeded, and
    /// `handle_event` stops at the pure refusal gate and hands the argv back for
    /// the driver to launch.
    #[test]
    fn enter_on_a_done_agent_absent_from_the_live_set_plain_resumes() {
        let dir = unique_temp_dir("done-resume");
        // Badged `done`, and claude confirms it is not holding it: no overlay.
        let mut app = app_with_agent_state(&dir, "sess-done", "done", &[]);

        let out = press(&mut app, KeyCode::Enter);

        assert!(
            app.modal.is_none(),
            "claude does not report this session as live, so Enter must not open \
             the Attach/Fork/Cancel overlay"
        );
        let Outcome::Resume(ready) = out else {
            panic!(
                "Enter on a `done` session must escalate to the resume hand-off; \
                 status: {:?}",
                app.status
            );
        };
        assert_eq!(
            ready.argv.join(" "),
            "claude -r sess-done",
            "a finished session must PLAIN-resume: no --fork-session, no attach"
        );
        assert_eq!(
            app.status, None,
            "a clean plain-resume leaves no refusal on the board"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **THE TOCTOU case — the bug this whole seam exists for.**
    ///
    /// The `--all` poll badged this session `done` (up to ~5.3s ago, or longer if
    /// the board has since been idle past `AGENTS_IDLE_AFTER`), but claude's
    /// active list reports it LIVE right now. The two disagree, which is exactly
    /// what the old gate could not represent: it inferred `state != "done"` ⇒
    /// live, so it plain-resumed, and `claude -r` refused with "Session … is
    /// currently running as a background agent (bg)". The user hit this on a
    /// `● bg done` row.
    ///
    /// Claude is the authority, so the fresh probe wins over the stale badge and
    /// Enter must offer Attach/Fork. This test is the one that pins the whole
    /// change: it fails against ANY gate that reads the polled map.
    #[test]
    fn enter_on_a_done_badged_session_that_claude_reports_live_opens_the_overlay() {
        let dir = unique_temp_dir("done-but-live");
        // The stale badge says `done`; claude says it is still running.
        let mut app = app_with_agent_state(&dir, "sess-raced", "done", &["sess-raced"]);

        let out = press(&mut app, KeyCode::Enter);

        assert!(
            matches!(out, Outcome::Continue),
            "a session claude is holding open must NOT hand off to `claude -r`, \
             which would be refused"
        );
        let modal = app.modal.clone().expect(
            "claude reports this session LIVE, so Enter must open the \
             Attach/Fork overlay even though the polled badge still says `done` \
             — trusting the stale badge here is the TOCTOU bug",
        );
        assert_eq!(modal.session_id.as_deref(), Some("sess-raced"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other half of the gate, over the IDENTICAL fixture: a session claude
    /// reports as still WORKING keeps opening the overlay.
    ///
    /// Paired with the `done`-absent test above, this proves Enter routes on
    /// MEMBERSHIP of the freshly-probed live set: the two differ only by what the
    /// probe reports.
    #[test]
    fn enter_on_a_working_agent_in_the_live_set_opens_the_overlay() {
        let dir = unique_temp_dir("working-overlay");
        let mut app = app_with_agent_state(&dir, "sess-working", "working", &["sess-working"]);

        let out = press(&mut app, KeyCode::Enter);

        assert!(matches!(out, Outcome::Continue));
        let modal = app
            .modal
            .clone()
            .expect("a working agent is live, so Enter must open the overlay");
        assert_eq!(modal.session_id.as_deref(), Some("sess-working"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The inverse disagreement, and the reason the probe's fail-soft direction
    /// is REVERSED from the deleted classifier's: a session badged `working` that
    /// claude does NOT report live must plain-resume.
    ///
    /// This is also the probe-failure path (a missing `claude`, a non-zero exit,
    /// or bad JSON all yield an EMPTY set, which is indistinguishable from "found
    /// nothing"): we degrade toward letting claude decide, and claude's own check
    /// backstops us. The old gate failed the other way and would have trapped
    /// this session behind an overlay it did not need.
    #[test]
    fn enter_on_a_working_badged_session_absent_from_the_live_set_plain_resumes() {
        let dir = unique_temp_dir("working-not-live");
        let mut app = app_with_agent_state(&dir, "sess-stale", "working", &[]);

        let out = press(&mut app, KeyCode::Enter);

        assert!(
            app.modal.is_none(),
            "claude is the authority: if its active list does not report the \
             session, Enter plain-resumes regardless of the badge"
        );
        let Outcome::Resume(ready) = out else {
            panic!("expected a plain resume; status: {:?}", app.status);
        };
        assert_eq!(ready.argv.join(" "), "claude -r sess-stale");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A probe that FAILS (missing binary / non-zero exit / bad JSON) yields an
    /// empty set, which must plain-resume rather than block the board.
    ///
    /// Fail-soft toward "let claude decide": we would rather hand the user
    /// claude's own real message than invent a refusal from a signal we could not
    /// read. Distinct from the test above in premise — nothing is badged live
    /// here at all — so the empty set is the ONLY thing driving the outcome.
    #[test]
    fn a_failed_probe_falls_back_to_a_plain_resume() {
        let dir = unique_temp_dir("probe-failed");
        let mut app = App::new(
            vec![resumable_session(&dir, "sess-nosignal")],
            Scope::All,
            PathBuf::from("/tmp"),
        );
        // Exactly what `agents::live_agents` returns when the shell-out fails in
        // any way.
        seed_live(&mut app, &[]);

        let out = press(&mut app, KeyCode::Enter);

        assert!(
            app.modal.is_none(),
            "an unavailable signal must never strand the user in an overlay"
        );
        let Outcome::Resume(ready) = out else {
            panic!("expected a plain resume; status: {:?}", app.status);
        };
        assert_eq!(ready.argv.join(" "), "claude -r sess-nosignal");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An app over a REALLY resumable session that the `--all` poll badges as a
    /// live background agent carrying `polled_job`.
    ///
    /// `polled_job` is the STALE snapshot the Attach hand-off must never read, so
    /// every test below seeds it with an id that DIFFERS from whatever claude
    /// reports at hand-off. The probe is left for the caller: what claude says at
    /// the hand-off is the variable under test.
    fn app_with_polled_job(dir: &Path, id: &str, polled_job: &str) -> App {
        let mut app = App::new(
            vec![resumable_session(dir, id)],
            Scope::All,
            PathBuf::from("/tmp"),
        );
        let mut reported = HashMap::new();
        reported.insert(
            id.to_string(),
            ReportedAgent {
                kind: "background".to_string(),
                id: Some(polled_job.to_string()),
                state: Some("working".to_string()),
                status: None,
                pid: None,
                started_at_ms: None,
            },
        );
        app.set_reported_agents(reported, None);
        assert_eq!(app.selected.as_deref(), Some(id));
        app
    }

    /// **The Attach job id comes from the PROBE, never from the polled `--all`
    /// map** — the same authoritative-read rule the resume gate follows, one layer
    /// down.
    ///
    /// The two ids DIFFER on purpose: the poll says `stale-job`, claude's active
    /// list says `fresh-job`. A fixture where they agreed would pass against
    /// EITHER source and so could not tell them apart — the "test board with
    /// exactly one bucket" mistake PATTERNS.md records. Asserts the observable
    /// argv the driver would spawn, not which function was called.
    #[test]
    fn attach_takes_its_job_id_from_the_probe_not_the_polled_map() {
        let dir = unique_temp_dir("attach-fresh-job");
        let mut app = app_with_polled_job(&dir, "sess-attach", "stale-job");
        // Asked at the hand-off, claude reports a DIFFERENT job id than the poll
        // is still carrying.
        seed_live_agents(
            &mut app,
            &[("sess-attach", "background", Some("fresh-job"))],
        );

        press(&mut app, KeyCode::Enter); // gate: live -> overlay, defaulting to Attach
        assert_eq!(
            app.modal.as_ref().unwrap().selected_action(),
            Some(&ModalAction::Attach)
        );
        let out = press(&mut app, KeyCode::Enter); // confirm Attach

        let Outcome::Resume(ready) = out else {
            panic!(
                "Attach on a live background agent must hand off; status: {:?}",
                app.status
            );
        };
        assert_eq!(
            ready.argv.join(" "),
            "claude attach fresh-job",
            "the attach target must be the job id claude reported AT THE HAND-OFF; \
             `stale-job` here means it was read back off the ~5.3s-stale `--all` map"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Live at Enter, FINISHED by the time Attach is confirmed: never spawn
    /// `claude attach` with a dead job id.
    ///
    /// The overlay can sit open indefinitely, so this gap is unbounded — the
    /// probe that OPENED the overlay is itself stale by the time the user picks.
    /// Claude is up and answering here (another agent is still live); it simply no
    /// longer reports OURS. That is what separates this from the probe-failure
    /// test below, and it pins that the refusal keys on our session's ABSENCE
    /// rather than on an empty answer: an implementation that attached to whatever
    /// job the list happened to carry would spawn `claude attach other-job` and
    /// fail here. The `--all` map meanwhile still badges ours live with
    /// `stale-job`, so reading THAT would hand off to a finished job.
    #[test]
    fn a_session_that_finishes_while_the_overlay_is_open_never_attaches_a_dead_id() {
        let dir = unique_temp_dir("attach-vanished");
        let mut app = app_with_polled_job(&dir, "sess-gone", "stale-job");
        seed_live_then(
            &mut app,
            // At the Enter gate: live, with a real job id.
            &[("sess-gone", "background", Some("fresh-job"))],
            // At the Attach hand-off: ours has finished; an unrelated agent runs on.
            &[("sess-other", "background", Some("other-job"))],
        );

        press(&mut app, KeyCode::Enter);
        assert!(
            app.modal.is_some(),
            "it was live at Enter, so the overlay opens"
        );

        let out = press(&mut app, KeyCode::Enter); // confirm Attach

        assert!(
            matches!(out, Outcome::Continue),
            "a session claude no longer holds must NOT hand off to `claude attach` \
             with a dead job id"
        );
        assert_eq!(
            app.status.as_deref(),
            Some(resume::ATTACH_NOT_LIVE),
            "the board must report what was OBSERVED — claude no longer reports it \
             — and name the routes that still work"
        );
        assert!(app.modal.is_none(), "the overlay closes on confirm");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A probe that FAILS at the Attach hand-off (missing binary / non-zero exit /
    /// bad JSON all yield an empty map) refuses fail-soft: no panic, no spawn.
    ///
    /// Distinct from the test above in PREMISE — claude answered nothing at all
    /// here, rather than answering without our session — and the fail-soft
    /// direction deliberately COLLAPSES the two: with no authoritative id there is
    /// nothing to attach to either way, so both refuse identically. We decline to
    /// distinguish "finished" from "could not ask" precisely because the probe
    /// cannot, which is why the copy states the report rather than a cause.
    #[test]
    fn a_probe_failure_at_the_attach_hand_off_refuses_instead_of_attaching() {
        let dir = unique_temp_dir("attach-probe-failed");
        let mut app = app_with_polled_job(&dir, "sess-nosignal", "stale-job");
        seed_live_then(
            &mut app,
            &[("sess-nosignal", "background", Some("fresh-job"))],
            // The shell-out fails: an empty answer, indistinguishable from "none".
            &[],
        );

        press(&mut app, KeyCode::Enter);
        assert!(app.modal.is_some());

        let out = press(&mut app, KeyCode::Enter); // confirm Attach

        assert!(
            matches!(out, Outcome::Continue),
            "an unreadable signal must never spawn a guessed-at `claude attach`"
        );
        assert_eq!(app.status.as_deref(), Some(resume::ATTACH_NOT_LIVE));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Fork does NOT probe, and must not be dragged behind Attach's re-ask: it is
    /// the route `ATTACH_NOT_LIVE` points the user at, so it has to stay valid
    /// exactly when Attach is refused.
    ///
    /// A fork of a live session is expected to work and a fork of a finished one
    /// is an ordinary fork, so liveness is not Fork's question to ask. The probe
    /// reports NOTHING by the time Fork is confirmed; the hand-off must proceed
    /// regardless.
    #[test]
    fn fork_from_the_overlay_hands_off_without_asking_the_probe() {
        let dir = unique_temp_dir("overlay-fork");
        let mut app = app_with_polled_job(&dir, "sess-fork", "stale-job");
        seed_live_then(
            &mut app,
            &[("sess-fork", "background", Some("fresh-job"))],
            &[],
        );

        press(&mut app, KeyCode::Enter); // gate: live -> overlay (Attach)
        press(&mut app, KeyCode::Right); // -> Fork
        assert_eq!(
            app.modal.as_ref().unwrap().selected_action(),
            Some(&ModalAction::Fork)
        );
        let out = press(&mut app, KeyCode::Enter); // confirm Fork

        let Outcome::Resume(ready) = out else {
            panic!(
                "Fork must hand off whatever the probe says; status: {:?}",
                app.status
            );
        };
        assert_eq!(
            ready.argv.join(" "),
            "claude -r sess-fork --fork-session",
            "Fork has no liveness question to ask, so it must never be gated on one"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Esc dismisses the overlay without acting.
    #[test]
    fn esc_dismisses_the_choice_overlay() {
        let mut app = app_with("live-1", Some("interactive"));
        press(&mut app, KeyCode::Enter);
        assert!(app.modal.is_some());
        press(&mut app, KeyCode::Esc);
        assert!(app.modal.is_none(), "Esc dismisses the overlay");
    }

    /// Ctrl-F fork stays a direct hand-off for a LIVE session (no overlay).
    #[test]
    fn ctrl_f_forks_a_live_session_directly_without_the_overlay() {
        let mut app = app_with("live-1", Some("background"));
        let out = handle_event(
            &mut app,
            AppEvent::Input(Event::Key(ctrl(KeyCode::Char('f')))),
            &mut store_at(Path::new("/tmp")),
        );
        assert!(matches!(out, Outcome::Continue));
        assert!(
            app.modal.is_none(),
            "Ctrl-F must not open the choice overlay, even for a live session"
        );
    }

    /// A `ReportedAgents` event swaps the agent set in (off-thread delivery path),
    /// together with the wall-clock instant the poller stamped it at.
    ///
    /// The stamp is the banner age's "now", so three things are pinned about it:
    /// it lands on `App` exactly as carried (this arm reads no clock of its own);
    /// a later map WITHOUT one clears it rather than keeping the older poll's
    /// instant; and it never reaches `App::status`, because an age is true over an
    /// interval and the status line carries only keypress-scoped outcomes
    /// (STATUS-LINE OWNERSHIP).
    #[test]
    fn reported_agents_event_updates_the_agent_set() {
        // The capture's real `startedAt` (1_790_152_789_592), polled 46 minutes on.
        const POLLED_AT: i64 = 1_790_155_549_592;
        let mut app = app_with("s", None);
        assert!(app.reported_agent("s").is_none());
        assert_eq!(app.reported_at_ms, None, "no poll yet, so no instant");
        let mut reported = HashMap::new();
        let mut agent = reported_agent("background");
        agent.started_at_ms = Some(POLLED_AT - 46 * 60 * 1_000);
        reported.insert("s".to_string(), agent);
        handle_event(
            &mut app,
            AppEvent::ReportedAgents {
                agents: reported.clone(),
                reported_at_ms: Some(POLLED_AT),
            },
            &mut store_at(Path::new("/tmp")),
        );
        assert_eq!(
            app.reported_agent("s").map(ReportedAgent::kind_label),
            Some("bg"),
            "a ReportedAgents event must update the agent set"
        );
        assert_eq!(
            app.reported_at_ms,
            Some(POLLED_AT),
            "the map's own stamp must land with it, exactly as carried"
        );
        assert_eq!(
            app.status, None,
            "an age is an interval fact: it renders on the banner, never on the status line"
        );

        handle_event(
            &mut app,
            AppEvent::ReportedAgents {
                agents: reported,
                reported_at_ms: None,
            },
            &mut store_at(Path::new("/tmp")),
        );
        assert_eq!(
            app.reported_at_ms, None,
            "a map with no stamp must not inherit the previous poll's instant"
        );
    }

    /// A `ModelAliases` event is what carries the off-thread `--model` probe's
    /// answer onto the board, and this is the only place that wiring is pinned.
    ///
    /// Asserted through the PICKER'S ROWS rather than through app state, because the
    /// rows are what a user would see; a test reading a field could pass over a
    /// picker that still built itself from the seed. The fixture's aliases differ
    /// from the seed in order and length on purpose, so "the rows changed" cannot be
    /// satisfied by the seed. The picker is a compose's, so a draft is open throughout.
    ///
    /// The event is also asserted to be SILENT: which aliases are on offer is true
    /// over an interval and belongs to the picker, so it must never squat on the
    /// keypress-scoped status line (STATUS-LINE OWNERSHIP).
    #[test]
    fn model_aliases_event_replaces_the_pickers_seed_without_touching_the_status_line() {
        let mut app = app_with("s", None);
        compose::open_background(&mut app, None);
        let rows = |app: &mut App| -> Vec<String> {
            app.open_model_picker();
            let labels = app
                .modal
                .as_ref()
                .expect("the model picker is open")
                .choices
                .iter()
                .map(|c| c.label.clone())
                .collect();
            app.modal = None;
            labels
        };

        let seeded = rows(&mut app);
        assert!(
            !seeded.iter().any(|l| l == "sonnet[1m]"),
            "the seed must not already carry the fixture's marker alias: {seeded:?}"
        );

        app.set_status("a refusal the user has not acknowledged".to_string());
        let out = handle_event(
            &mut app,
            AppEvent::ModelAliases(vec![
                "sonnet[1m]".to_string(),
                "opusplan".to_string(),
                "zeta-9".to_string(),
            ]),
            &mut store_at(Path::new("/tmp")),
        );
        assert!(
            matches!(out, Outcome::Continue),
            "the probe's answer never ends the board session"
        );

        let probed = rows(&mut app);
        assert_eq!(
            probed.iter().skip(1).cloned().collect::<Vec<_>>(),
            vec!["sonnet[1m]", "opusplan", "zeta-9"],
            "the delivered set must replace the seed verbatim, unknown alias and all"
        );
        assert_eq!(
            app.status.as_deref(),
            Some("a refusal the user has not acknowledged"),
            "a background delivery must not overwrite a sticky refusal"
        );

        // An EMPTY delivery is the probe's "could not read it" answer: it degrades
        // to the seed rather than emptying the overlay.
        handle_event(
            &mut app,
            AppEvent::ModelAliases(Vec::new()),
            &mut store_at(Path::new("/tmp")),
        );
        assert_eq!(
            rows(&mut app),
            seeded,
            "an empty probe result falls back to the seed, never to an empty picker"
        );
    }

    /// A board row backed by a committed PREVIEW fixture, so a render really parses
    /// a transcript — the one way a reply's session model can be on record.
    fn preview_fixture_session(id: &str, file: &str) -> Session {
        let mut row = session(id);
        row.file = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("preview")
            .join(file);
        row
    }

    /// The first row of the picker the OPEN compose would show, as (label,
    /// description) — opened and closed again, so the compose stays as it was.
    fn picker_row_zero(app: &mut App) -> (String, String) {
        app.open_model_picker();
        let row = app.modal.take().expect("the model picker is open").choices[0].clone();
        (row.label, row.description.unwrap_or_default())
    }

    /// A `SettingsModel` event is what carries the off-thread settings read onto the
    /// board, and this is the only place that wiring is pinned — BOTH halves of it:
    /// the new-session model reaches a draft's picker, and the restore override
    /// reaches a reply's.
    ///
    /// Asserted through the picker's first ROW — what a user sees — rather than the
    /// stored fields, and asserted SILENT: both facts are true over an interval, so
    /// they must never overwrite the keypress-scoped status line (STATUS-LINE
    /// OWNERSHIP). A later empty answer (a return from a session whose saved
    /// `/model` pick cleared the setting) takes each away again.
    #[test]
    fn settings_model_event_reaches_both_pickers_without_touching_the_status_line() {
        let mut app = App::new(
            vec![preview_fixture_session(
                "sbe-switch",
                "sess-model-switch-1.jsonl",
            )],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        let store = &mut store_at(Path::new("/tmp"));

        // The DRAFT half: the settings model.
        compose::open_background(&mut app, None);
        assert_eq!(
            picker_row_zero(&mut app).0,
            "default",
            "nothing read yet names no new-session model"
        );
        app.set_status("a refusal the user has not acknowledged".to_string());
        let out = handle_event(
            &mut app,
            AppEvent::SettingsModel(crate::claude_settings::ModelDefaults {
                new_session: Some("opus[1m]".to_string()),
                restore_overridden: false,
            }),
            store,
        );
        assert!(
            matches!(out, Outcome::Continue),
            "the settings read never ends the board session"
        );
        assert_eq!(
            picker_row_zero(&mut app).0,
            "default (opus[1m]) (settings)",
            "the delivered model reaches the draft picker's first row"
        );
        assert_eq!(
            app.status.as_deref(),
            Some("a refusal the user has not acknowledged"),
            "a background delivery must not overwrite a sticky refusal"
        );
        app.close_compose();

        // The REPLY half: the restore override. The preview is rendered first, as a
        // frame would, so the session's own model is on record.
        let _ = app.preview_text(80);
        compose::open(&mut app, "sbe-switch".to_string(), None);
        assert_eq!(
            picker_row_zero(&mut app).0,
            "session's model (Sonnet 5)",
            "with no override, a reply's default is the model its session last answered with"
        );
        handle_event(
            &mut app,
            AppEvent::SettingsModel(crate::claude_settings::ModelDefaults {
                new_session: Some("opus[1m]".to_string()),
                restore_overridden: true,
            }),
            store,
        );
        let (label, why) = picker_row_zero(&mut app);
        assert_eq!(
            label, "default",
            "an override means claude will not restore the session's model"
        );
        assert!(
            why.contains("ANTHROPIC_MODEL"),
            "and the row says why: {why}"
        );

        // A later empty answer takes both away again.
        handle_event(
            &mut app,
            AppEvent::SettingsModel(crate::claude_settings::ModelDefaults::default()),
            store,
        );
        assert_eq!(
            picker_row_zero(&mut app).0,
            "session's model (Sonnet 5)",
            "the override is gone, so the reply names its session's model again"
        );
        app.close_compose();
        compose::open_background(&mut app, None);
        assert_eq!(
            picker_row_zero(&mut app).0,
            "default",
            "and the draft names no settings model again"
        );
    }

    /// The tick is the board's clock: `view::blink_visible` phases the live-badge
    /// pulse off it, so a `Tick` that does not ADVANCE it leaves the dot frozen —
    /// exactly the "dot never pulses" bug this counter exists to fix. The view
    /// tests set `App::tick` by hand, so this is the only place the wiring from
    /// the real event to that field is pinned.
    #[test]
    fn tick_event_advances_the_board_clock() {
        let mut app = app_with("s", None);
        assert_eq!(app.tick, 0, "a fresh board starts at tick 0");

        for expected in 1..=3 {
            let out = handle_event(&mut app, AppEvent::Tick, &mut store_at(Path::new("/tmp")));
            assert!(
                matches!(out, Outcome::Continue),
                "a tick never ends the board"
            );
            assert_eq!(
                app.tick, expected,
                "each Tick must advance the clock by one"
            );
        }
    }

    /// The tick counter WRAPS rather than overflowing: a board left running long
    /// enough to saturate a `u64` must keep drawing, not panic in a debug build.
    #[test]
    fn tick_event_wraps_instead_of_overflowing() {
        let mut app = app_with("s", None);
        app.tick = u64::MAX;
        handle_event(&mut app, AppEvent::Tick, &mut store_at(Path::new("/tmp")));
        assert_eq!(app.tick, 0, "the clock must wrap from u64::MAX back to 0");
    }

    /// Task 3.10: the `AppEvent::Tick` wiring drives `tick_status`, so a transient
    /// status expires after `STATUS_DWELL_TICKS` ticks while a sticky one stays.
    #[test]
    fn tick_event_expires_transient_status() {
        let mut app = app_with("s", None);
        app.set_status_transient("sent");
        assert_eq!(app.status.as_deref(), Some("sent"));

        for i in 0..STATUS_DWELL_TICKS {
            assert_eq!(
                app.status.as_deref(),
                Some("sent"),
                "transient status must survive tick {i}"
            );
            handle_event(&mut app, AppEvent::Tick, &mut store_at(Path::new("/tmp")));
        }
        assert!(
            app.status.is_none(),
            "transient status must clear after STATUS_DWELL_TICKS Tick events"
        );
        assert!(app.status_ttl.is_none());
    }

    #[test]
    fn tick_event_keeps_sticky_status() {
        let mut app = app_with("s", None);
        app.set_status("send failed: boom");

        for i in 0..=STATUS_DWELL_TICKS {
            assert_eq!(
                app.status.as_deref(),
                Some("send failed: boom"),
                "sticky status must survive tick {i}"
            );
            handle_event(&mut app, AppEvent::Tick, &mut store_at(Path::new("/tmp")));
        }
        assert_eq!(app.status.as_deref(), Some("send failed: boom"));
    }

    /// Task 3.12: a failure from the honesty seam (`status_for_output` on a
    /// non-zero exit) is classified sticky, so it MUST survive the dwell window.
    #[test]
    fn failure_status_survives_the_dwell() {
        let mut app = app_with("s", None);
        let (status, success) = send::status_for_output(false, "", "boom");
        assert!(!success, "the fixture must be a sticky failure");
        app.set_status(status);

        for i in 0..=STATUS_DWELL_TICKS {
            assert!(
                app.status.is_some(),
                "failure status must survive tick {i}: {:?}",
                app.status
            );
            handle_event(&mut app, AppEvent::Tick, &mut store_at(Path::new("/tmp")));
        }
        assert!(
            app.status.is_some(),
            "failure status must still be present after the dwell"
        );
    }

    #[test]
    fn arrows_always_move() {
        assert_eq!(key_to_action(key(KeyCode::Up), true, false), Action::MoveUp);
        assert_eq!(
            key_to_action(key(KeyCode::Down), true, false),
            Action::MoveDown
        );
        // Arrows navigate even mid-query.
        assert_eq!(
            key_to_action(key(KeyCode::Up), false, false),
            Action::MoveUp
        );
        assert_eq!(
            key_to_action(key(KeyCode::Down), false, false),
            Action::MoveDown
        );
        // And an UNSHIFTED arrow keeps moving even when the preview HAS marks —
        // the new binding takes the shifted form alone.
        assert_eq!(key_to_action(key(KeyCode::Up), false, true), Action::MoveUp);
        assert_eq!(
            key_to_action(key(KeyCode::Down), false, true),
            Action::MoveDown
        );
    }

    /// With nothing marked to move between, the SHIFTED arrows are bit-for-bit the
    /// plain arrows they have always been.
    ///
    /// This is the whole safety argument for putting a binding on a modifier the
    /// board never used: a user who never searches loses nothing, and a terminal or
    /// multiplexer that drops the modifier degrades to a working key rather than a
    /// dead one. Every state where the guard is unsatisfied is walked, so the
    /// fall-through cannot be half-implemented.
    #[test]
    fn shifted_arrows_fall_through_to_plain_move_with_nothing_marked() {
        for (query_empty, marked) in [(true, false), (false, false), (true, true)] {
            assert_eq!(
                key_to_action(shift(KeyCode::Up), query_empty, marked),
                Action::MoveUp,
                "Shift-Up must still move (query_empty={query_empty}, marked={marked})"
            );
            assert_eq!(
                key_to_action(shift(KeyCode::Down), query_empty, marked),
                Action::MoveDown,
                "Shift-Down must still move (query_empty={query_empty}, marked={marked})"
            );
        }
    }

    /// Only with a query AND something marked in the previewed transcript do the
    /// shifted arrows become match navigation.
    #[test]
    fn shifted_arrows_step_the_preview_matches_when_something_is_marked() {
        assert_eq!(
            key_to_action(shift(KeyCode::Up), false, true),
            Action::PreviewMatchPrev
        );
        assert_eq!(
            key_to_action(shift(KeyCode::Down), false, true),
            Action::PreviewMatchNext
        );
    }

    #[test]
    fn jk_always_type_into_the_query() {
        // `j`/`k` are search characters like every other letter — the selection
        // moves on the arrows, which are not printable and cannot be typed by
        // accident. The `query_empty = true` half is the one with teeth; it is
        // what fails if a bare-letter navigation binding is ever reintroduced.
        for empty in [true, false] {
            assert_eq!(
                key_to_action(key(KeyCode::Char('j')), empty, false),
                Action::Insert('j')
            );
            assert_eq!(
                key_to_action(key(KeyCode::Char('k')), empty, false),
                Action::Insert('k')
            );
        }
    }

    /// Plain `←` / `→` move the search caret, ALWAYS: with or without a query and
    /// with or without marks. The `query_empty = true` rows are the ones with teeth
    /// for the rejected "fold while the query is empty" split — a gate on the query
    /// would hand the empty-query arrows back to the fold, and this fails there.
    #[test]
    fn left_right_move_the_search_caret_regardless_of_query() {
        for (query_empty, marked) in [(true, false), (false, false), (true, true), (false, true)] {
            assert_eq!(
                key_to_action(key(KeyCode::Left), query_empty, marked),
                Action::CaretBack,
                "Left (query_empty={query_empty}, marked={marked})"
            );
            assert_eq!(
                key_to_action(key(KeyCode::Right), query_empty, marked),
                Action::CaretForward,
                "Right (query_empty={query_empty}, marked={marked})"
            );
        }
    }

    /// `Alt-b` / `Alt-f` — readline's word hops, the `ESC b` / `ESC f` some
    /// terminals send for `⌥←` / `⌥→` — hop the caret one WORD, with or without a
    /// query and with or without marks. The uppercase rows are what crossterm
    /// decodes `ESC B` / `ESC F` into: the capital letter plus `SHIFT`.
    ///
    /// This is the bound half of the `Alt` catch-all rule for these two letters:
    /// hoisting `Char(_) if alt => Ignore` above the hop arms decodes them as
    /// [`Action::Ignore`] and fails HERE.
    #[test]
    fn alt_b_and_alt_f_hop_the_caret_by_word() {
        for (query_empty, marked) in [(true, false), (false, false), (true, true), (false, true)] {
            for (event, want) in [
                (alt(KeyCode::Char('b')), Action::CaretWordBack),
                (alt(KeyCode::Char('f')), Action::CaretWordForward),
                (alt_shift(KeyCode::Char('B')), Action::CaretWordBack),
                (alt_shift(KeyCode::Char('F')), Action::CaretWordForward),
            ] {
                assert_eq!(
                    key_to_action(event, query_empty, marked),
                    want,
                    "{event:?} (query_empty={query_empty}, marked={marked})"
                );
            }
        }
    }

    /// `Alt-←` / `Alt-→` (`CSI 1;3D` / `C`) and `Ctrl-←` / `Ctrl-→` (`CSI 1;5D` /
    /// `C`) hop the caret one WORD, always — the arrow halves of the word-hop set.
    ///
    /// Each pair has an arm placement this pins. The `Alt` arms must sit ABOVE the
    /// unguarded plain arms, which match an `Alt` arrow too: the other order decodes
    /// `Alt-←` as [`Action::CaretBack`], a one-character step that looks like a
    /// sluggish hop. The `Ctrl` arms must sit INSIDE the `ctrl` early-return block:
    /// written in the lower match they are never reached, and `Ctrl-←` decodes as
    /// the block's [`Action::Ignore`].
    #[test]
    fn alt_and_ctrl_arrows_hop_the_caret_by_word() {
        for (query_empty, marked) in [(true, false), (false, false), (true, true), (false, true)] {
            for (event, want) in [
                (alt(KeyCode::Left), Action::CaretWordBack),
                (alt(KeyCode::Right), Action::CaretWordForward),
                (ctrl(KeyCode::Left), Action::CaretWordBack),
                (ctrl(KeyCode::Right), Action::CaretWordForward),
            ] {
                assert_eq!(
                    key_to_action(event, query_empty, marked),
                    want,
                    "{event:?} (query_empty={query_empty}, marked={marked})"
                );
            }
        }
    }

    /// The `Alt` arrow arms sit BETWEEN the layout arms and the plain caret arms,
    /// and a held `Shift` still wins: `Shift-Alt-←` (`CSI 1;4D`) steps the layout,
    /// so hoisting the `Alt` arms above the `Shift` ones fails here — the one
    /// ordering the word-hop decode test above cannot see.
    #[test]
    fn a_held_shift_wins_over_the_alt_word_hop_arms() {
        assert_eq!(
            key_to_action(alt_shift(KeyCode::Left), false, false),
            Action::LayoutTowardPreview
        );
        assert_eq!(
            key_to_action(alt_shift(KeyCode::Right), false, false),
            Action::LayoutTowardList
        );
    }

    #[test]
    fn q_always_types_into_the_query() {
        // `q` types, it never quits — an unconfirmed exit on the first letter of
        // "query" is a board thrown away by typo. `Esc`/`Ctrl-C` are the quit
        // keys. The `query_empty = true` half is the one with teeth; it is what
        // fails if the bare-`q` quit binding is ever reintroduced.
        for empty in [true, false] {
            assert_eq!(
                key_to_action(key(KeyCode::Char('q')), empty, false),
                Action::Insert('q')
            );
        }
    }

    #[test]
    fn copy_id_with_a_selection_decides_to_yank_the_full_id() {
        let id = "550e8400-e29b-41d4-a716-446655440000";
        let mut app = app_with(id, None);
        // The decision performs no I/O at all: it hands the driver the FULL id as
        // the copy request, and claims nothing on the status line — the status
        // comes from the copy's RESULT (`finish_copy`), never from the keypress.
        let Outcome::Copy(requested) = copy_selected_id(&mut app) else {
            panic!("a selection must request the copy");
        };
        assert_eq!(
            requested,
            CopyPayload::SessionId(id.to_string()),
            "the request carries the FULL session id"
        );
        assert_eq!(app.status, None, "nothing is claimed before the copy ran");
    }

    #[test]
    fn copy_id_with_no_selection_decides_to_write_nothing() {
        // Empty store => nothing is selected. The decision must request NO copy —
        // `Continue`, not `Copy`, is the real, assertable proof that nothing is
        // copied or written (not a proxy through the status string) — plus the
        // sticky no-selection status. The `Ctrl-X y` dispatch of this branch is
        // pinned by `ctrl_x_y_completes_the_chord_without_leaking_into_the_query`.
        let mut app = App::new(vec![], Scope::All, PathBuf::from("/tmp"));
        assert!(app.selected_session().is_none());
        assert!(matches!(copy_selected_id(&mut app), Outcome::Continue));
        assert_eq!(app.status.as_deref(), Some(NO_SELECTION_STATUS));
    }

    /// The OSC 52 path's status puts the FULL id first and the caveat after it, so
    /// an 80-column help row truncates the caveat and never the id — the id is what
    /// the user selects by hand when the terminal ignores the escape. It names OSC
    /// 52 and never says "copied": nothing confirmed that the terminal took it.
    #[test]
    fn the_osc52_sent_status_puts_the_full_id_before_the_caveat_and_never_says_copied() {
        let id = "550e8400-e29b-41d4-a716-446655440000";
        let status = osc52_sent_status(id);
        let id_at = status.find(id).expect("the status carries the full id");
        let caveat_at = status
            .find(OSC52_SENT_STATUS_CAVEAT)
            .expect("the status carries the caveat");
        assert!(
            id_at + id.len() <= caveat_at,
            "the id must come BEFORE the caveat: {status:?}"
        );
        assert!(
            id_at + id.len() <= 80,
            "the whole id must fit an 80-column help row: {status:?}"
        );
        assert!(status.contains("OSC 52"), "say how it was sent: {status:?}");
        assert!(
            !status.to_lowercase().contains("copied"),
            "the OSC 52 path must never claim a copy: {status:?}"
        );
    }

    /// Both copy status lines name the thing the way the `Ctrl-X y` verb is labelled
    /// on the key-doc surfaces with room for it (`copy session ID`; the which-key
    /// hint's column budget shortens it to `y copy ID`): a tool-confirmed copy reads
    /// EXACTLY `Copied session ID <uuid>`, and the OSC 52 line says `session ID
    /// <uuid>` too, so a relabel cannot leave one route naming it differently.
    #[test]
    fn both_copy_statuses_say_session_id_the_way_the_key_docs_label_the_verb() {
        let id = "550e8400-e29b-41d4-a716-446655440000";
        assert_eq!(copy_status(id), format!("Copied session ID {id}"));
        let sent = osc52_sent_status(id);
        assert!(
            sent.contains(&format!("session ID {id}")),
            "the OSC 52 line names the session ID the same way: {sent:?}"
        );
    }

    #[test]
    fn a_bare_y_types_into_the_query() {
        // The copy lives on `Ctrl-X y`, never a bare letter: a bare `y` TYPES, with
        // or without a query. The `query_empty = true` half is the one with teeth —
        // it is what fails if the withdrawn bare-`y` copy binding ever returns.
        for empty in [true, false] {
            assert_eq!(
                key_to_action(key(KeyCode::Char('y')), empty, false),
                Action::Insert('y')
            );
        }
    }

    #[test]
    fn esc_clears_a_typed_query_and_quits_on_an_empty_one() {
        assert_eq!(key_to_action(key(KeyCode::Esc), true, false), Action::Quit);
        assert_eq!(
            key_to_action(key(KeyCode::Esc), false, false),
            Action::ClearQuery
        );
    }

    #[test]
    fn esc_clears_the_query_first_and_a_second_esc_quits() {
        let mut app = App::new(
            vec![session("alpha"), session("beta")],
            Scope::All,
            PathBuf::from("/tmp"),
        );
        let all = app.filtered.len();
        press(&mut app, KeyCode::Char('z'));
        press(&mut app, KeyCode::Char('z'));
        assert_eq!(app.query(), "zz");
        assert!(app.filtered.len() < all, "the query narrowed the list");

        let out = press(&mut app, KeyCode::Esc);
        assert!(matches!(out, Outcome::Continue), "first Esc keeps running");
        assert_eq!(app.query(), "", "first Esc clears the query");
        assert_eq!(app.filtered.len(), all, "the filter re-applied");
        assert!(app.selected_session().is_some(), "selection survives");

        let out = press(&mut app, KeyCode::Esc);
        assert!(matches!(out, Outcome::Quit), "second Esc quits");
    }

    #[test]
    fn ctrl_c_always_quits() {
        assert_eq!(
            key_to_action(ctrl(KeyCode::Char('c')), false, false),
            Action::Quit
        );
    }

    #[test]
    fn enter_resumes_and_ctrl_f_forks() {
        assert_eq!(
            key_to_action(key(KeyCode::Enter), true, false),
            Action::Resume { fork: false }
        );
        assert_eq!(
            key_to_action(ctrl(KeyCode::Char('f')), false, false),
            Action::Resume { fork: true }
        );
    }

    #[test]
    fn ctrl_n_starts_a_new_session_regardless_of_query() {
        // Ctrl-N is an always-available action key (like Ctrl-F / Ctrl-A): it
        // never becomes query input, so it maps to NewSession whether or not the
        // user is mid-query. Both `n` and `N` (Shift) decode the same.
        for empty in [true, false] {
            assert_eq!(
                key_to_action(ctrl(KeyCode::Char('n')), empty, false),
                Action::NewSession
            );
            assert_eq!(
                key_to_action(ctrl(KeyCode::Char('N')), empty, false),
                Action::NewSession
            );
        }
    }

    #[test]
    fn toggles_are_reachable_regardless_of_query() {
        // Tab toggles search mode; Ctrl-A scope.
        assert_eq!(
            key_to_action(key(KeyCode::Tab), false, false),
            Action::ToggleSearchMode
        );
        assert_eq!(
            key_to_action(ctrl(KeyCode::Char('a')), false, false),
            Action::ToggleScope
        );
    }

    /// The retired preview toggle, `Ctrl-/`, is now an ordinary unbound `Ctrl`
    /// key: it lands in the ctrl block's "ignore other Ctrl keys" fallback, in BOTH
    /// encodings a terminal sends it as (`/`, and the 0x1f control code surfaced as
    /// `_`), with or without a query. `Ignore` rather than `Insert` is the point —
    /// a stray press of the old key must not type a `/` into the search.
    #[test]
    fn the_retired_ctrl_slash_is_ignored_and_never_types() {
        for empty in [true, false] {
            assert_eq!(
                key_to_action(ctrl(KeyCode::Char('/')), empty, false),
                Action::Ignore
            );
            assert_eq!(
                key_to_action(ctrl(KeyCode::Char('_')), empty, false),
                Action::Ignore
            );
        }

        // Through the whole handler: the board, the query and the layout are all
        // exactly what they were.
        let mut app = app_with("s", None);
        press_ctrl(&mut app, KeyCode::Char('/'));
        press_ctrl(&mut app, KeyCode::Char('_'));
        assert_eq!(app.query(), "", "Ctrl-/ must not type into the query");
        assert_eq!(app.pane_layout(), PaneLayout::Even, "nor move the layout");
    }

    /// `Shift-←` / `Shift-→` step the layout UNCONDITIONALLY: with and without a
    /// query, and with search hits present — every combination of the two
    /// conditions the SHIFTED VERTICAL arrows depend on. The `marked = true` rows are
    /// the ones with teeth: they are the state in which the neighbouring
    /// `Shift-↑`/`↓` arms change meaning, and they must not drag this pair along.
    #[test]
    fn shifted_horizontal_arrows_step_the_layout_regardless_of_query_or_marks() {
        for (query_empty, marked) in [(true, false), (false, false), (true, true), (false, true)] {
            assert_eq!(
                key_to_action(shift(KeyCode::Left), query_empty, marked),
                Action::LayoutTowardPreview,
                "Shift-Left (query_empty={query_empty}, marked={marked})"
            );
            assert_eq!(
                key_to_action(shift(KeyCode::Right), query_empty, marked),
                Action::LayoutTowardList,
                "Shift-Right (query_empty={query_empty}, marked={marked})"
            );
        }
    }

    /// The layout arms sit ABOVE the plain `Left`/`Right` ones, and this is the
    /// test that makes the order observable: the unguarded caret arm matches a
    /// shifted arrow too, so swapping the two would leave every decode above
    /// compiling while `Shift-←` moved the search caret. Both directions are
    /// asserted — the shifted keys step, the unshifted ones still move the caret.
    #[test]
    fn the_layout_arms_win_over_the_plain_caret_arms() {
        assert_eq!(
            key_to_action(shift(KeyCode::Left), false, false),
            Action::LayoutTowardPreview
        );
        assert_eq!(
            key_to_action(key(KeyCode::Left), false, false),
            Action::CaretBack
        );
        assert_eq!(
            key_to_action(shift(KeyCode::Right), false, false),
            Action::LayoutTowardList
        );
        assert_eq!(
            key_to_action(key(KeyCode::Right), false, false),
            Action::CaretForward
        );
    }

    #[test]
    fn preview_scroll_keys_act_regardless_of_query() {
        // Page + jump keys are not printable, so they scroll the preview whether
        // or not the user is mid-query.
        for empty in [true, false] {
            assert_eq!(
                key_to_action(key(KeyCode::PageUp), empty, false),
                Action::PreviewPageUp
            );
            assert_eq!(
                key_to_action(key(KeyCode::PageDown), empty, false),
                Action::PreviewPageDown
            );
            assert_eq!(
                key_to_action(key(KeyCode::Home), empty, false),
                Action::PreviewTop
            );
            assert_eq!(
                key_to_action(key(KeyCode::End), empty, false),
                Action::PreviewBottom
            );
            // Ctrl-U / Ctrl-D quarter-page, also independent of query state.
            assert_eq!(
                key_to_action(ctrl(KeyCode::Char('u')), empty, false),
                Action::PreviewHalfUp
            );
            assert_eq!(
                key_to_action(ctrl(KeyCode::Char('d')), empty, false),
                Action::PreviewHalfDown
            );
            // Ctrl-T / Ctrl-E reach the same top/bottom jump as Home/End, also
            // independent of query state.
            assert_eq!(
                key_to_action(ctrl(KeyCode::Char('t')), empty, false),
                Action::PreviewTop
            );
            assert_eq!(
                key_to_action(ctrl(KeyCode::Char('e')), empty, false),
                Action::PreviewBottom
            );
        }
    }

    #[test]
    fn printable_characters_type_into_the_query() {
        assert_eq!(
            key_to_action(key(KeyCode::Char('a')), true, false),
            Action::Insert('a')
        );
        assert_eq!(
            key_to_action(key(KeyCode::Char('z')), false, false),
            Action::Insert('z')
        );
        assert_eq!(
            key_to_action(key(KeyCode::Backspace), false, false),
            Action::Backspace
        );
    }

    /// All three word-delete keys decode to [`Action::BackspaceWord`], and plain
    /// `Backspace` still decodes to [`Action::Backspace`].
    ///
    /// The contrast in the last block is the whole test. `Alt-Backspace` is
    /// matched by the UNGUARDED `KeyCode::Backspace` arm too, so the two arms
    /// differ only in ORDER: with the guarded one below, every assertion here
    /// still compiles and `Alt-Backspace` quietly deletes ONE CHARACTER — the
    /// exact failure this feature exists to fix, and one that looks like it
    /// worked. Asserting both directions is what makes the order observable.
    ///
    /// `Ctrl-W` is asserted because it lives inside the `ctrl` early-return
    /// block; the same arm written in the lower match would never be reached.
    /// Its shifted forms are both covered — a terminal may report `Ctrl-Shift-W`
    /// as the bare uppercase char or with SHIFT alongside CONTROL.
    ///
    /// Every case runs with `query_empty` both ways: a word delete is a deletion
    /// whether or not anything is there to delete, so none of these keys may
    /// ever grow a query gate.
    #[test]
    fn every_word_delete_key_maps_to_backspace_word() {
        for empty in [true, false] {
            // The macOS gesture, ESC 0x7F.
            assert_eq!(
                key_to_action(alt(KeyCode::Backspace), empty, false),
                Action::BackspaceWord
            );
            // readline's Ctrl-W (0x17), needing no Alt at all.
            assert_eq!(
                key_to_action(ctrl(KeyCode::Char('w')), empty, false),
                Action::BackspaceWord
            );
            assert_eq!(
                key_to_action(ctrl(KeyCode::Char('W')), empty, false),
                Action::BackspaceWord
            );
            assert_eq!(
                key_to_action(ctrl_shift(KeyCode::Char('W')), empty, false),
                Action::BackspaceWord
            );
            // The third key `TextArea::input` maps to a word delete, so the
            // board answers the same set the reply box does.
            assert_eq!(
                key_to_action(alt(KeyCode::Char('h')), empty, false),
                Action::BackspaceWord
            );
            assert_eq!(
                key_to_action(alt(KeyCode::Char('H')), empty, false),
                Action::BackspaceWord
            );

            // ...and the unmodified key is untouched: one character, as always.
            assert_eq!(
                key_to_action(key(KeyCode::Backspace), empty, false),
                Action::Backspace
            );
        }
    }

    /// An `Alt`-modified printable with no binding is SWALLOWED, never typed.
    ///
    /// The bound `Alt-H` above and this are the two halves of one rule: the
    /// catch-all arm must be reachable for the unbound half of the namespace
    /// (`Alt-J` must not type `j` into the query) and must NOT be reached for
    /// the bound half. The sibling test owns the second half — hoisting the
    /// catch-all above `Char('h' | 'H') if alt` fails THERE, on `Alt-H`. This
    /// test owns the first: delete the catch-all outright and nothing else in
    /// the suite notices, because `Alt-J` then falls through to `Char(c)` and
    /// types a bare `j` into the query.
    #[test]
    fn an_unbound_alt_printable_is_ignored_rather_than_typed() {
        for empty in [true, false] {
            assert_eq!(
                key_to_action(alt(KeyCode::Char('j')), empty, false),
                Action::Ignore
            );
            assert_eq!(
                key_to_action(alt(KeyCode::Char('z')), empty, false),
                Action::Ignore
            );
        }
    }

    /// A word-delete key pressed END TO END removes an ATOM, not a character.
    ///
    /// The decode tests above stop at [`Action::BackspaceWord`] and the `App`
    /// tests start at `pop_query_word`, which leaves the WIRE between them — the
    /// `apply_action` arm — pinned by nothing: swap its body for
    /// `app.pop_query_char()` and both halves still pass while the feature ships
    /// deleting one character per press, the exact defect it exists to fix. The
    /// contrast assertion names that impostor result (`alpha bet`) so the failure
    /// says which way the wire went wrong.
    ///
    /// Driven through `handle_event` rather than `apply_action` directly because
    /// `apply_action` is private to this module's routing and the keypress is the
    /// thing a user actually performs; all three keys go through the one arm, so
    /// `Alt-Backspace` and `Ctrl-W` here cover it from both match blocks.
    #[test]
    fn a_word_delete_keypress_removes_a_whole_atom_from_the_query() {
        for press_word_delete in [
            (|app: &mut App| press_alt(app, KeyCode::Backspace)) as fn(&mut App) -> Outcome,
            |app: &mut App| press_ctrl(app, KeyCode::Char('w')),
        ] {
            let mut app = app_with("idle", None);
            app.push_query_str("alpha beta");

            press_word_delete(&mut app);

            assert_eq!(
                app.query(),
                "alpha ",
                "the keypress must reach `pop_query_word`, not `pop_query_char` \
                 (which would leave `alpha bet`)"
            );
        }
    }

    /// `←` / `→` pressed END TO END move the caret one character — and do NOTHING
    /// else. A caret move is not a query change, so the press must not go through
    /// the query funnel: in name+content mode that funnel re-arms the preview's
    /// jump onto a match, which would yank the reader's pane on a key that edited
    /// nothing, so the drained jump staying drained is the assertion with teeth.
    /// Both ends are walked past, so a move off either end is pinned as a no-op.
    #[test]
    fn left_and_right_keypresses_move_the_caret_and_leave_the_board_alone() {
        let mut app = app_with("s", None);
        press(&mut app, KeyCode::Tab);
        assert_eq!(
            app.search_mode,
            SearchMode::NameAndContent,
            "premise: the mode in which a query change arms the preview's jump"
        );
        type_into_board(&mut app, "lab");
        assert_eq!(
            app.query_caret(),
            3,
            "premise: typing leaves the caret last"
        );
        assert!(
            app.take_preview_match_jump(),
            "premise: a real query change DOES arm the jump (drained here)"
        );
        app.preview_follow_bottom = false;
        app.preview_scroll = 7;
        let filtered = app.filtered.clone();

        press(&mut app, KeyCode::Left);
        assert_eq!(app.query_caret(), 2, "← steps the caret one character back");
        for _ in 0..3 {
            press(&mut app, KeyCode::Left);
        }
        assert_eq!(app.query_caret(), 0, "and stops at the head of the line");
        press(&mut app, KeyCode::Right);
        assert_eq!(app.query_caret(), 1, "→ steps it one character forward");
        for _ in 0..3 {
            press(&mut app, KeyCode::Right);
        }
        assert_eq!(app.query_caret(), 3, "and stops at the tail");

        assert_eq!(app.query(), "lab", "a caret move edits nothing");
        assert!(
            !app.take_preview_match_jump(),
            "a caret move is not a query change: it must not re-arm the preview's jump"
        );
        assert_eq!(app.filtered, filtered, "the list is untouched");
        assert_eq!(app.selected.as_deref(), Some("s"), "so is the selection");
        assert_eq!(
            (app.preview_scroll, app.preview_follow_bottom),
            (7, false),
            "and the reader's place in the preview"
        );
    }

    /// Every word-hop pair pressed END TO END — both byte forms of `⌥←` / `⌥→`
    /// (`CSI 1;3D` and `ESC b`, plus the `ESC B` capital), and `Ctrl-←` / `Ctrl-→`
    /// — moves the caret one WORD and does NOTHING else.
    ///
    /// The query `lab el` makes a hop distinguishable from every near miss: from
    /// the tail a one-character step lands at 5 where the hop lands at 4, and from
    /// the head the hop forward lands on the START of `el` (4) — not on `lab`'s
    /// last character (2), and not just past it (3). The drained preview jump
    /// staying drained is what catches a hop routed through the query funnel, as in
    /// the one-character sibling above.
    #[test]
    fn word_hop_keypresses_move_the_caret_by_word_and_leave_the_board_alone() {
        for (back, forward) in [
            (alt(KeyCode::Left), alt(KeyCode::Right)),
            (alt(KeyCode::Char('b')), alt(KeyCode::Char('f'))),
            (alt_shift(KeyCode::Char('B')), alt_shift(KeyCode::Char('F'))),
            (ctrl(KeyCode::Left), ctrl(KeyCode::Right)),
        ] {
            let mut app = app_with("s", None);
            let mut store = store_at(Path::new("/tmp"));
            press(&mut app, KeyCode::Tab);
            type_into_board(&mut app, "lab el");
            assert_eq!(
                app.query_caret(),
                6,
                "premise: typing leaves the caret last"
            );
            assert!(
                app.take_preview_match_jump(),
                "premise: a real query change DOES arm the jump (drained here)"
            );
            app.preview_follow_bottom = false;
            app.preview_scroll = 7;
            let filtered = app.filtered.clone();
            assert!(!filtered.is_empty(), "premise: the query keeps the row");

            let mut hop = |event, want: usize, why: &str| {
                feed(&mut app, event, &mut store);
                assert_eq!(app.query_caret(), want, "{event:?}: {why}");
            };
            hop(back, 4, "back to the start of `el`, not one character (5)");
            hop(back, 0, "then to the start of `lab`");
            hop(back, 0, "and no further");
            hop(
                forward,
                4,
                "forward onto the START of `el`, not onto `lab`'s last character (2) or \
                 just past it (3)",
            );
            hop(forward, 6, "then to the tail");
            hop(forward, 6, "and no further");

            assert_eq!(app.query(), "lab el", "{back:?}: a word hop edits nothing");
            assert!(
                !app.take_preview_match_jump(),
                "{back:?}: a word hop is not a query change: it must not re-arm the jump"
            );
            assert_eq!(app.filtered, filtered, "{back:?}: the list is untouched");
            assert_eq!(
                app.selected.as_deref(),
                Some("s"),
                "{back:?}: so is the selection"
            );
            assert_eq!(
                (app.preview_scroll, app.preview_follow_bottom),
                (7, false),
                "{back:?}: and the reader's place in the preview"
            );
        }
    }

    /// Once `←` has moved the caret, every editing key acts AT it: a typed
    /// character goes in there, `Backspace` takes the character before it, and a
    /// word delete cuts the atom before it — never the query's last atom, which is
    /// what a word delete still pinned to the end of the line would take (it would
    /// leave `alpha beta ` here). The text after the caret survives each edit and
    /// the caret stays where the edit left it.
    #[test]
    fn editing_keys_act_at_the_caret_after_it_moves() {
        for press_word_delete in [
            (|app: &mut App| press_alt(app, KeyCode::Backspace)) as fn(&mut App) -> Outcome,
            |app: &mut App| press_ctrl(app, KeyCode::Char('w')),
        ] {
            let mut app = app_with("idle", None);
            app.push_query_str("alpha beta gamma");
            for _ in 0.." gamma".len() {
                press(&mut app, KeyCode::Left);
            }
            assert_eq!(
                app.query_caret(),
                10,
                "premise: the caret sits after `beta`"
            );

            press_word_delete(&mut app);
            assert_eq!(
                app.query(),
                "alpha  gamma",
                "the atom BEFORE the caret goes and the tail after it stays"
            );
            assert_eq!(app.query_caret(), 6, "the caret stays at the cut");

            type_into_board(&mut app, "xy");
            assert_eq!(app.query(), "alpha xy gamma", "typing resumes at the cut");
            press(&mut app, KeyCode::Backspace);
            assert_eq!(
                app.query(),
                "alpha x gamma",
                "Backspace takes the character before the caret, not the last one"
            );
            assert_eq!(app.query_caret(), 7);
        }
    }

    #[test]
    fn release_events_are_not_actionable() {
        let released = KeyEvent::new_with_kind(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert!(!is_actionable(released), "release events must be ignored");
        assert!(is_actionable(key(KeyCode::Char('q'))), "press events act");
    }

    // --- new-session agent picker -----------------------------------------

    use crate::defined_agents::DefinedAgent;

    fn def_agent(name: &str) -> DefinedAgent {
        DefinedAgent {
            name: name.to_string(),
            description: None,
        }
    }

    #[test]
    fn agent_pick_key_maps_navigation_confirm_and_cancel() {
        // A List (vertical picker): Up/Down (and k/j, plus Tab forward) navigate;
        // Enter confirms; Esc / Ctrl-C cancel.
        let list = ModalLayout::List;
        assert!(matches!(
            modal_key(key(KeyCode::Down), list),
            ModalNav::Next
        ));
        assert!(matches!(
            modal_key(key(KeyCode::Char('j')), list),
            ModalNav::Next
        ));
        assert!(matches!(modal_key(key(KeyCode::Tab), list), ModalNav::Next));
        assert!(matches!(modal_key(key(KeyCode::Up), list), ModalNav::Prev));
        assert!(matches!(
            modal_key(key(KeyCode::Char('k')), list),
            ModalNav::Prev
        ));
        assert!(matches!(
            modal_key(key(KeyCode::Enter), list),
            ModalNav::Confirm
        ));
        assert!(matches!(
            modal_key(key(KeyCode::Esc), list),
            ModalNav::Cancel
        ));
        assert!(matches!(
            modal_key(ctrl(KeyCode::Char('c')), list),
            ModalNav::Cancel
        ));

        // A List never MOVES its highlight sideways — the Row's horizontal
        // navigation must not be unioned into a vertical picker's key map. Its
        // arrows ADJUST the highlighted row instead (only a model row acts on that;
        // see the behavioural tests below), and `h`/`l` stay unbound.
        assert_eq!(
            modal_key(key(KeyCode::Left), list),
            ModalNav::Adjust { forward: false }
        );
        assert_eq!(
            modal_key(key(KeyCode::Right), list),
            ModalNav::Adjust { forward: true }
        );
        assert_eq!(modal_key(key(KeyCode::Char('h')), list), ModalNav::Ignore);
        assert_eq!(modal_key(key(KeyCode::Char('l')), list), ModalNav::Ignore);

        // A Row (button strip) binds the horizontal keys to MOVE its highlight, on
        // top of the shared vertical ones — `h`/`l` included, unlike the List.
        let row = ModalLayout::Row;
        assert!(matches!(modal_key(key(KeyCode::Left), row), ModalNav::Prev));
        assert!(matches!(
            modal_key(key(KeyCode::Char('h')), row),
            ModalNav::Prev
        ));
        assert!(matches!(
            modal_key(key(KeyCode::Right), row),
            ModalNav::Next
        ));
        assert!(matches!(
            modal_key(key(KeyCode::Char('l')), row),
            ModalNav::Next
        ));
        // The shared vertical keys and cancel still work in the Row layout.
        assert!(matches!(modal_key(key(KeyCode::Up), row), ModalNav::Prev));
        assert!(matches!(
            modal_key(key(KeyCode::Enter), row),
            ModalNav::Confirm
        ));
        assert!(matches!(
            modal_key(key(KeyCode::Esc), row),
            ModalNav::Cancel
        ));
    }

    /// The AGENT picker keeps ignoring `←`/`→`, behaviourally: the arrows reach it
    /// as [`ModalNav::Adjust`] (the `List` key map binds them for the model
    /// picker's sake), and on an agent row that must change NOTHING — not the
    /// highlight, not the rows, not the open state — and launch nothing.
    ///
    /// Compared after EVERY single press, never only after a sequence: a sequence
    /// can wrap a moved highlight back to where it started (three moves over three
    /// rows) and pass while every press was acting.
    #[test]
    fn the_agent_picker_ignores_left_and_right() {
        let mut app = app_with("s", None);
        app.open_agent_picker(vec![def_agent("planner"), def_agent("reviewer")]);
        for downs in 0..3 {
            if downs > 0 {
                press(&mut app, KeyCode::Down);
            }
            let before = app.modal.clone();
            for code in [KeyCode::Left, KeyCode::Right] {
                let out = press(&mut app, code);
                assert!(
                    matches!(out, Outcome::Continue),
                    "an arrow launches nothing"
                );
                assert_eq!(
                    app.modal, before,
                    "row {downs}: {code:?} must leave the agent picker exactly as it was"
                );
                assert!(!app.is_composing(), "and must not open the draft pane");
            }
        }
    }

    #[test]
    fn picker_ctrl_o_on_the_default_row_starts_a_bare_claude() {
        // `app_with` uses `/tmp` as the launch dir (exists), so the new-session
        // gate proceeds and the default (row 0) starts a bare `claude`.
        let mut app = app_with("s", None);
        app.open_agent_picker(vec![def_agent("planner"), def_agent("reviewer")]);
        let out = press_ctrl(&mut app, KeyCode::Char('o'));
        match out {
            Outcome::Resume(ready) => assert_eq!(ready.argv.join(" "), "claude"),
            _ => panic!("the default pick must start a bare claude"),
        }
        assert!(app.modal.is_none(), "Ctrl-O closes the picker");
    }

    #[test]
    fn picker_ctrl_o_on_an_agent_row_binds_it_and_remembers_the_pick() {
        let mut app = app_with("s", None);
        app.open_agent_picker(vec![def_agent("planner"), def_agent("reviewer")]);
        // Down once from the default row -> the first agent (planner).
        press(&mut app, KeyCode::Down);
        assert_eq!(
            app.modal.as_ref().unwrap().selected_action(),
            Some(&ModalAction::New(Some("planner".to_string())))
        );
        let out = press_ctrl(&mut app, KeyCode::Char('o'));
        match out {
            Outcome::Resume(ready) => {
                assert_eq!(ready.argv.join(" "), "claude --agent planner");
                // The plan carries the new-session hint, not the resume one.
                assert_eq!(ready.nonzero_hint, resume::NEW_SESSION_NONZERO_HINT);
            }
            _ => panic!("an agent pick must start `claude --agent planner`"),
        }
        // An ACTUAL launch is remembered in-memory: the NEXT picker pre-highlights it.
        app.open_agent_picker(vec![def_agent("planner"), def_agent("reviewer")]);
        assert_eq!(
            app.modal.as_ref().unwrap().selected_action(),
            Some(&ModalAction::New(Some("planner".to_string()))),
            "the last started agent pre-highlights on the next Ctrl-N"
        );
    }

    #[test]
    fn picker_esc_dismisses_without_starting_a_session() {
        let mut app = app_with("s", None);
        app.open_agent_picker(vec![def_agent("planner")]);
        let out = press(&mut app, KeyCode::Esc);
        assert!(matches!(out, Outcome::Continue));
        assert!(app.modal.is_none(), "Esc dismisses the picker");
    }

    #[test]
    fn keys_route_to_the_picker_not_the_board_while_it_is_open() {
        // While the picker owns the keyboard, a printable char is an inert picker
        // keypress — it must not type into the query or touch the board.
        let mut app = app_with("s", None);
        app.open_agent_picker(vec![def_agent("planner")]);
        press(&mut app, KeyCode::Char('x'));
        assert!(
            app.query().is_empty(),
            "a key during the picker must not type into the query"
        );
        assert!(app.modal.is_some(), "an inert key leaves the picker open");
    }

    // --- the Ctrl-N draft pane and the picker's Ctrl-O bypass ---------------

    /// A REGRESSION PIN for the verb swap: the interactive start MOVED from `Enter`
    /// to `Ctrl-O`, and it must have moved unchanged. Both rows are asserted by
    /// their FULL argv, because "the feature still works" would also pass against a
    /// drafted positional leaking into the interactive start.
    #[test]
    fn picker_ctrl_o_starts_interactively_with_the_unchanged_argv() {
        for (downs, expected) in [(0usize, "claude"), (1, "claude --agent planner")] {
            let mut app = app_with("s", None);
            app.open_agent_picker(vec![def_agent("planner"), def_agent("reviewer")]);
            for _ in 0..downs {
                press(&mut app, KeyCode::Down);
            }
            match press_ctrl(&mut app, KeyCode::Char('o')) {
                Outcome::Resume(ready) => {
                    assert_eq!(
                        ready.argv.join(" "),
                        expected,
                        "Ctrl-O must emit the bare new-session argv — no prompt positional"
                    );
                    assert_eq!(ready.nonzero_hint, resume::NEW_SESSION_NONZERO_HINT);
                    assert_eq!(ready.race_probe_id, None);
                }
                _ => panic!("Ctrl-O on the picker must hand off a resume"),
            }
            assert!(!app.is_composing(), "Ctrl-O must NOT open the draft pane");
        }
    }

    /// `Enter` on a highlighted picker row closes the picker and opens the
    /// background draft pane for THAT row's agent — the default row carrying `None`
    /// rather than a blank name. Drafting is what a new session defaults to now.
    #[test]
    fn picker_enter_opens_the_background_draft_for_the_highlighted_agent() {
        // Row 0: the "default (no agent)" entry.
        let mut app = app_with("s", None);
        app.set_pane_layout(PaneLayout::ListOnly); // prove the draft pane brings the preview back
        app.open_agent_picker(vec![def_agent("planner"), def_agent("reviewer")]);
        let out = press(&mut app, KeyCode::Enter);
        assert!(
            matches!(out, Outcome::Continue),
            "opening a pane launches nothing"
        );
        assert!(app.modal.is_none(), "Enter closes the picker");
        assert_eq!(
            app.pane_layout(),
            PaneLayout::Even,
            "the draft pane brings a hidden preview back at 1:1"
        );
        assert_eq!(
            app.compose.as_ref().map(|c| &c.target),
            Some(&ComposeTarget::NewBackgroundAgent { agent: None })
        );

        // Row 1: the first named agent.
        let mut app = app_with("s", None);
        app.open_agent_picker(vec![def_agent("planner"), def_agent("reviewer")]);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            app.compose.as_ref().map(|c| &c.target),
            Some(&ComposeTarget::NewBackgroundAgent {
                agent: Some("planner".to_string())
            })
        );
    }

    /// `Ctrl-N` with ZERO defined agents skips the pointless one-row picker and
    /// opens the draft pane directly, bound to no agent — it must NOT resume.
    ///
    /// BOTH `HOME` and `CLAUDE_CONFIG_DIR` are redirected so
    /// `defined_agents::discover_agents` finds neither a user- nor a
    /// project-level `.claude/agents`. `HOME` alone used to be enough because
    /// `user_agents_dir` hardcoded `~/.claude/agents`, but it is now PROFILE-scoped
    /// via `config::claude_config_dir_if_known`, which prefers `$CLAUDE_CONFIG_DIR`
    /// over `$HOME` — so a developer with `CLAUDE_CONFIG_DIR` exported in their own
    /// shell would have it win over this test's redirected `HOME`, defeating the
    /// isolation and letting their REAL agents decide which branch this test
    /// exercises. Redirecting `CLAUDE_CONFIG_DIR` to the same isolated temp home's
    /// `.claude` closes that gap.
    #[test]
    fn ctrl_n_with_no_defined_agents_opens_the_draft_pane_with_no_agent() {
        let _guard = crate::config::env_lock();
        let home = unique_temp_dir("no-agents-home");
        let launch = unique_temp_dir("no-agents-launch");
        let previous_home = std::env::var_os("HOME");
        let previous_claude_config_dir = std::env::var_os("CLAUDE_CONFIG_DIR");
        std::env::set_var("HOME", &home);
        std::env::set_var("CLAUDE_CONFIG_DIR", home.join(".claude"));

        let mut app = App::new(vec![session("s")], Scope::All, launch.clone());
        app.set_pane_layout(PaneLayout::ListOnly); // prove the draft pane brings the preview back
        let out = press_ctrl(&mut app, KeyCode::Char('n'));

        match previous_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        match previous_claude_config_dir {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&launch);

        assert!(
            matches!(out, Outcome::Continue),
            "the no-agent path must draft, not hand off a resume"
        );
        assert!(
            app.modal.is_none(),
            "a one-row picker would be pure friction"
        );
        assert_eq!(
            app.pane_layout(),
            PaneLayout::Even,
            "the draft pane brings a hidden preview back at 1:1"
        );
        assert_eq!(
            app.compose.as_ref().map(|c| &c.target),
            Some(&ComposeTarget::NewBackgroundAgent { agent: None })
        );
    }

    /// `Ctrl-O` belongs to the PICKER alone: on the running-session Attach/Fork
    /// strip and the hard-delete confirm it is inert — it must not close the modal,
    /// launch anything, or (worst) act on the highlighted choice.
    #[test]
    fn ctrl_o_is_inert_on_every_modal_but_the_picker() {
        // The Row-layout key map never yields Interactive, whatever is highlighted.
        assert!(matches!(
            modal_key(ctrl(KeyCode::Char('o')), ModalLayout::Row),
            ModalNav::Ignore
        ));
        assert!(matches!(
            modal_key(ctrl(KeyCode::Char('o')), ModalLayout::List),
            ModalNav::Interactive
        ));

        // The running-session choice: still open, nothing handed off.
        let mut app = app_with("s", Some("background"));
        app.open_live_choice("s".to_string());
        let out = press_ctrl(&mut app, KeyCode::Char('o'));
        assert!(matches!(out, Outcome::Continue));
        assert!(app.modal.is_some(), "Ctrl-O must not dismiss the overlay");
        assert!(
            !app.is_composing(),
            "Ctrl-O must not compose from Attach/Fork"
        );

        // The hard-delete confirm: likewise inert, and nothing was deleted.
        let mut app = app_with("s", None);
        app.open_delete_confirm();
        let out = press_ctrl(&mut app, KeyCode::Char('o'));
        assert!(matches!(out, Outcome::Continue));
        assert!(app.modal.is_some(), "Ctrl-O must not dismiss the confirm");
        assert!(!app.is_composing());
    }

    /// `Ctrl-B` was REMOVED from the picker with no alias and no deprecation, so it
    /// must be as inert there as any unbound chord: no draft, no launch, and the
    /// picker still open. The guard that the removal is real rather than renamed.
    #[test]
    fn ctrl_b_is_now_inert_on_the_picker() {
        assert!(matches!(
            modal_key(ctrl(KeyCode::Char('b')), ModalLayout::List),
            ModalNav::Ignore
        ));

        let mut app = app_with("s", None);
        app.open_agent_picker(vec![def_agent("planner"), def_agent("reviewer")]);
        press(&mut app, KeyCode::Down);
        let out = press_ctrl(&mut app, KeyCode::Char('b'));
        assert!(
            matches!(out, Outcome::Continue),
            "an unbound chord launches nothing"
        );
        assert!(app.modal.is_some(), "Ctrl-B must leave the picker open");
        assert!(!app.is_composing(), "Ctrl-B must not open the draft pane");
    }

    /// OPENING a draft must NOT record its row as the last-picked agent: nothing has
    /// been launched yet (the draft can still be cancelled), and the memory behind
    /// `Ctrl-N`'s pre-highlight means "the agent of the last new session actually
    /// started". Pinned through the ONLY surface that reads it — the next picker's
    /// pre-highlight — with `Ctrl-O` on the same row as the control, so this cannot
    /// pass by that memory being dead.
    #[test]
    fn opening_the_draft_does_not_record_the_pick_as_the_last_new_agent() {
        let agents = || vec![def_agent("planner"), def_agent("reviewer")];

        // Draft on row 2 (reviewer), then cancel: nothing was launched.
        let mut app = app_with("s", None);
        app.open_agent_picker(agents());
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Esc);
        assert!(!app.is_composing(), "Esc cancels the draft");

        app.open_agent_picker(agents());
        assert_eq!(
            app.modal.as_ref().map(|m| m.selected),
            Some(0),
            "a drafted (never launched) pick must not pre-highlight the next Ctrl-N"
        );

        // Control: `Ctrl-O` on that same row DOES record it.
        let mut app = app_with("s", None);
        app.open_agent_picker(agents());
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        press_ctrl(&mut app, KeyCode::Char('o'));
        app.open_agent_picker(agents());
        assert_eq!(
            app.modal.as_ref().map(|m| m.selected),
            Some(2),
            "an interactive start pre-highlights the agent it used"
        );
    }

    /// `Enter` in the draft pane launches in the BACKGROUND: it escalates to
    /// `Outcome::BgLaunch` (never `Outcome::Resume` — the board must not tear down),
    /// carrying `claude --agent <name> --bg <prompt>` run in the launch dir, and
    /// marks the draft card launching in the preview pane.
    #[test]
    fn draft_enter_escalates_to_a_background_launch_not_a_resume() {
        let mut app = app_with("s", None);
        app.open_agent_picker(vec![def_agent("planner")]);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        type_into_draft(&mut app, "ship the thing");

        match press(&mut app, KeyCode::Enter) {
            Outcome::BgLaunch(req) => {
                assert_eq!(
                    req.argv.join(" "),
                    "claude --agent planner --bg ship the thing"
                );
                assert_eq!(req.cwd, app.launch_dir, "the child runs in the launch dir");
            }
            Outcome::Resume(_) => {
                panic!("a background launch must NOT route through the teardown round trip")
            }
            _ => panic!("a drafted prompt must escalate to a background launch"),
        }
        assert!(!app.is_composing(), "launching closes the draft pane");
        assert!(
            app.draft
                .as_ref()
                .is_some_and(crate::tui::app::NewSessionDraft::is_launching),
            "the draft card is marked launching in the preview pane"
        );
        assert!(
            app.sending.is_empty(),
            "a launch is not a reply: nothing to echo into a transcript"
        );
    }

    /// A BACKGROUND launch is a real start, so it records its agent as the pick the
    /// next `Ctrl-N` pre-highlights — the same memory the interactive routes write.
    /// Pinned through that pre-highlight (the only surface reading it), with an
    /// EMPTY draft on another row as the control: a nudge is not a launch, so it
    /// must leave the memory alone.
    #[test]
    fn a_background_launch_records_the_agent_as_the_last_new_agent() {
        let agents = || vec![def_agent("planner"), def_agent("reviewer")];

        // Row 2 (reviewer), drafted and launched with `--bg`.
        let mut app = app_with("s", None);
        app.open_agent_picker(agents());
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        type_into_draft(&mut app, "ship the thing");
        assert!(
            matches!(press(&mut app, KeyCode::Enter), Outcome::BgLaunch(_)),
            "the draft must actually launch for this to be a launch record"
        );

        app.open_agent_picker(agents());
        assert_eq!(
            app.modal.as_ref().map(|m| m.selected),
            Some(2),
            "a background launch pre-highlights the agent it started"
        );

        // Control: an empty draft nudges instead of launching, so it records nothing.
        let mut app = app_with("s", None);
        app.open_agent_picker(agents());
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter);
        assert!(app.is_composing(), "an empty draft stays open");
        app.close_compose();
        app.open_agent_picker(agents());
        assert_eq!(
            app.modal.as_ref().map(|m| m.selected),
            Some(0),
            "a nudged (never launched) draft must not pre-highlight the next Ctrl-N"
        );
    }

    /// `Enter` on an EMPTY draft nudges and keeps the pane open — a background agent
    /// with no first message would just sit there, so this is not a launch.
    #[test]
    fn draft_enter_on_an_empty_buffer_nudges_and_keeps_the_pane_open() {
        for blank in ["", "   \n  "] {
            let mut app = app_with("s", None);
            app.open_agent_picker(vec![def_agent("planner")]);
            press(&mut app, KeyCode::Enter);
            type_into_draft(&mut app, blank);
            let out = press(&mut app, KeyCode::Enter);
            assert!(
                matches!(out, Outcome::Continue),
                "an empty draft must launch nothing"
            );
            assert!(app.is_composing(), "the draft pane stays open to type into");
            let status = app.status.as_deref().expect("a nudge is shown");
            assert!(
                status.contains("background agent"),
                "the nudge must say why an empty draft is useless: {status}"
            );
        }
    }

    /// The draft CARD and the compose editor open and close as ONE surface.
    ///
    /// They are separate fields precisely so the view need not read the compose
    /// target — which is exactly the shape that could drift — so this pins that
    /// every route out of a draft clears both. A REPLY is the control: it previews
    /// a real session, so it must open no card at all.
    #[test]
    fn the_draft_card_and_the_editor_open_and_close_together() {
        // Confirming the picker's "default (no agent)" row drafts: editor AND card.
        // Driven through the picker rather than a bare `Ctrl-N` so the test does not
        // depend on which agents the host machine happens to define.
        let mut app = app_with("s", None);
        app.open_agent_picker(vec![def_agent("planner")]);
        press(&mut app, KeyCode::Enter);
        assert!(
            app.is_composing(),
            "confirming a pick opens the draft editor"
        );
        assert_eq!(
            app.draft,
            Some(NewSessionDraft {
                agent: None,
                launch_id: None,
            }),
            "the draft pane must own a card so the preview stops showing a transcript"
        );

        // Esc drops both — a cancelled draft may not leave a placeholder pane up.
        press(&mut app, KeyCode::Esc);
        assert!(!app.is_composing(), "Esc closes the editor");
        assert!(app.draft.is_none(), "Esc must close the card with it");

        // The picker route carries the picked agent onto the card.
        let mut app = app_with("s", None);
        app.open_agent_picker(vec![def_agent("planner")]);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            app.draft.as_ref().and_then(|d| d.agent.as_deref()),
            Some("planner"),
            "the card names the agent the picker confirmed"
        );

        // Control: a quick REPLY previews a real session, so it opens NO card.
        let mut app = app_with("idle", None);
        press_ctrl(&mut app, KeyCode::Char('r'));
        assert!(app.is_composing(), "Ctrl-R opens the reply editor");
        assert!(
            app.draft.is_none(),
            "a reply must not blank out the transcript it is addressed to"
        );
    }

    /// A DISPATCHED draft keeps its card, marked in flight, until the launch reports
    /// back — the pane still has no session to show, so snapping back to an
    /// unrelated transcript at the moment of launch would reintroduce the very
    /// confusion the card removes. The completion event already on the channel is
    /// what ends it: no tick, thread, or event source is added for this.
    #[test]
    fn a_dispatched_draft_keeps_its_card_until_the_launch_reports_back() {
        let mut app = app_with("s", None);
        app.open_agent_picker(vec![def_agent("planner")]);
        press(&mut app, KeyCode::Enter);
        type_into_draft(&mut app, "ship the thing");
        let Outcome::BgLaunch(req) = press(&mut app, KeyCode::Enter) else {
            panic!("the draft must actually dispatch for this to be an in-flight card");
        };

        assert!(!app.is_composing(), "there is nothing left to type");
        assert_eq!(
            app.draft,
            Some(NewSessionDraft {
                agent: None,
                launch_id: Some(req.launch_id),
            }),
            "the card stays, stamped with the launch it now reports"
        );

        // THAT launch's one-shot completion event closes it (spawn failures
        // included: the driver emits exactly one of these whatever the child did).
        let out = handle_event(
            &mut app,
            AppEvent::BgLaunchFinished {
                launch_id: req.launch_id,
                status: "background agent started".to_string(),
                success: true,
            },
            &mut store_at(Path::new("/tmp")),
        );
        assert!(matches!(out, Outcome::Continue));
        assert!(app.draft.is_none(), "the result ends the card");
        assert_eq!(app.status.as_deref(), Some("background agent started"));
    }

    /// `Ctrl-O` in the draft pane runs the agent INTERACTIVELY instead: the draft
    /// becomes claude's trailing positional through the ordinary
    /// `Outcome::Resume` teardown round trip, never a `--bg` launch — and, being a
    /// real launch, it records the agent the next `Ctrl-N` pre-highlights.
    #[test]
    fn draft_ctrl_o_hands_off_interactively_with_the_prompt_as_a_positional() {
        let mut app = app_with("s", None);
        app.open_agent_picker(vec![def_agent("planner")]);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        type_into_draft(&mut app, "ship the thing");

        match press_ctrl(&mut app, KeyCode::Char('o')) {
            Outcome::Resume(ready) => {
                assert_eq!(
                    ready.argv.join(" "),
                    "claude --agent planner ship the thing"
                );
                assert!(
                    !ready.argv.iter().any(|a| a == "--bg"),
                    "the interactive hand-off must not carry --bg: {:?}",
                    ready.argv
                );
                assert_eq!(ready.nonzero_hint, resume::NEW_SESSION_NONZERO_HINT);
            }
            Outcome::BgLaunch(_) => panic!("Ctrl-O must NOT launch in the background"),
            _ => panic!("Ctrl-O must hand off an interactive resume"),
        }
        assert!(!app.is_composing(), "handing off closes the draft pane");
        // The CARD too, not just the editor: this route tears the terminal down, and
        // a card left behind here is exactly the stranded placeholder that survives
        // into the next board session (the completion event it waits for cannot).
        assert!(app.draft.is_none(), "handing off closes the card with it");

        // The draft's own `Ctrl-O` is one of the three REAL launch points, so it
        // writes the same memory the picker's `Ctrl-O` and the `--bg` submit do.
        app.open_agent_picker(vec![def_agent("planner"), def_agent("reviewer")]);
        assert_eq!(
            app.modal.as_ref().map(|m| m.selected),
            Some(1),
            "the draft's interactive launch pre-highlights the agent it started"
        );
    }

    /// `Ctrl-O` on an EMPTY draft launches BARE — no positional at all — i.e.
    /// exactly what the picker's own `Ctrl-O` emits.
    #[test]
    fn draft_ctrl_o_on_an_empty_buffer_launches_bare() {
        let mut app = app_with("s", None);
        app.open_agent_picker(vec![def_agent("planner")]);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        type_into_draft(&mut app, "   ");

        match press_ctrl(&mut app, KeyCode::Char('o')) {
            Outcome::Resume(ready) => assert_eq!(
                ready.argv.join(" "),
                "claude --agent planner",
                "a whitespace draft must emit no positional"
            ),
            _ => panic!("Ctrl-O must hand off an interactive resume"),
        }
    }

    /// A REFUSED `Ctrl-O` tears the surface down too, even though the board stays up.
    ///
    /// This is the one interactive route the shared teardown seam does not cover: a
    /// `check_new` refusal returns `Outcome::Continue`, which by design does NOT end
    /// the board session, so `handle_event` never reaches `close_compose` and
    /// `open_interactive`'s own call is all that stands between the user and a
    /// surface left up over a launch that never happened. Delete that one line and
    /// every other `Ctrl-O` test still passes — the seam masks them — while the user
    /// is handed a refusal status behind an editor that is still taking keystrokes
    /// for a hand-off that was declined.
    #[test]
    fn draft_ctrl_o_tears_the_surface_down_when_the_gate_refuses() {
        // A launch dir that is GONE is what `resume::check_new` refuses on; no
        // `claude` is spawned on this path (the gate is pure existence + argv).
        let missing = PathBuf::from("/no/such/snapback/launch/dir/anywhere");
        assert!(
            !missing.exists(),
            "the launch dir must be absent for the gate to refuse"
        );
        let mut app = App::new(vec![session("s")], Scope::All, missing);
        seed_live(&mut app, &[]);

        app.open_agent_picker(vec![def_agent("planner")]);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        type_into_draft(&mut app, "ship the thing");
        assert!(
            app.is_composing() && app.draft.is_some(),
            "the draft surface must be up for its teardown to be testable"
        );

        let out = press_ctrl(&mut app, KeyCode::Char('o'));
        assert!(
            matches!(out, Outcome::Continue),
            "a refused new-session gate keeps the board, so no teardown seam fires"
        );
        assert!(
            app.status
                .as_deref()
                .is_some_and(|s| s.contains("no longer exists")),
            "the refusal must surface as a board status: {:?}",
            app.status
        );
        assert!(
            !app.is_composing(),
            "the editor must not survive a declined hand-off"
        );
        assert!(
            app.draft.is_none(),
            "nor the card: nothing was dispatched for it to report, and no \
             BgLaunchFinished will ever arrive to close it"
        );
        assert!(
            !app.preview_pointer_blocked(),
            "a surface stranded by a refusal leaves the mouse gated on the board"
        );
    }

    /// `Ctrl-O` is INERT on a reply draft — there is no new-session launch to escape
    /// to — and it must leave the reply's buffer and target untouched.
    #[test]
    fn ctrl_o_is_inert_on_a_reply_draft() {
        let mut app = app_with("idle", None);
        press_ctrl(&mut app, KeyCode::Char('r'));
        type_into_draft(&mut app, "hello");

        let out = press_ctrl(&mut app, KeyCode::Char('o'));
        assert!(
            matches!(out, Outcome::Continue),
            "a reply has no interactive launch to escape to"
        );
        assert!(app.is_composing(), "the reply compose zone stays open");
        assert_eq!(
            app.compose.as_ref().map(|c| &c.target),
            Some(&ComposeTarget::Reply {
                session_id: "idle".to_string(),
                stop_job: None,
            })
        );
        assert_eq!(
            app.compose
                .as_ref()
                .map(|c| c.textarea.lines().join("\n"))
                .as_deref(),
            Some("hello"),
            "an inert chord must not edit the buffer either"
        );
    }

    /// Which outcomes END the board session — the predicate the compose surface's
    /// teardown hangs on.
    ///
    /// Both directions are load-bearing, and the FALSE side is the sharper one: the
    /// no-teardown effects keep drawing on the same channel, so counting
    /// `BgLaunch` here would close the draft card at the moment of dispatch — which
    /// is precisely the snap-back-to-an-unrelated-transcript the card exists to
    /// prevent. `Signal` is one of them: the driver performs it inline and keeps the
    /// board up, so it must not end the session either.
    #[test]
    fn ends_board_session_is_true_for_the_teardown_outcomes_only() {
        let ready = resume::Ready {
            cwd: PathBuf::from("/tmp"),
            argv: vec!["claude".to_string()],
            nonzero_hint: resume::NEW_SESSION_NONZERO_HINT,
            race_probe_id: None,
        };
        assert!(Outcome::Quit.ends_board_session());
        assert!(Outcome::Resume(ready).ends_board_session());

        assert!(!Outcome::Continue.ends_board_session());
        assert!(!Outcome::BgLaunch(crate::send::BgLaunchRequest {
            launch_id: 0,
            argv: vec!["claude".to_string()],
            cwd: PathBuf::from("/tmp"),
        })
        .ends_board_session());
        assert!(!Outcome::Send(crate::send::SendRequest {
            session_id: "s".to_string(),
            argv: vec!["claude".to_string()],
            cwd: PathBuf::from("/tmp"),
            stop_job: None,
        })
        .ends_board_session());
        assert!(!Outcome::Interrupt(crate::send::InterruptRequest {
            argv: vec!["claude".to_string()],
            cwd: PathBuf::from("/tmp"),
            session_id: "s".to_string(),
        })
        .ends_board_session());
        // The clipboard copy's request and its completion both keep the board up,
        // whichever kind they carry: the worker reports back on the SAME channel.
        for payload in [
            CopyPayload::SessionId("s".to_string()),
            CopyPayload::Selection("row".to_string()),
        ] {
            assert!(!Outcome::Copy(payload.clone()).ends_board_session());
            assert!(!Outcome::FinishCopy {
                payload,
                copied: false,
            }
            .ends_board_session());
        }
        // A value only: nothing here signals — the syscall lives in the driver.
        assert!(!Outcome::Signal { pid: 29628 }.ends_board_session());
    }

    /// An in-flight card must not outlive the board session that dispatched it.
    ///
    /// The editor closes at dispatch, so `Ctrl-F` / `Enter` on a row stay routable
    /// while the card is up — and both hand the terminal over. The completion event
    /// that would have ended the card cannot survive that: `run_inner` builds a new
    /// `EventLoop` per board session and drops the old receiver, so the launch
    /// reports back into a channel nobody is reading and the SAME `App` re-enters
    /// the board still holding the card. That strands the preview on a placeholder
    /// for every session, with `preview_pointer_blocked` stuck true (dead link clicks, dead
    /// fold toggles, dead drag-selections), recoverable only by opening and
    /// cancelling another compose.
    /// Every hand-off therefore ends the card with the board session it belonged to.
    #[test]
    fn handing_off_while_a_launch_is_in_flight_leaves_no_stranded_card() {
        let dir = unique_temp_dir("stranded-card");
        for fork in [true, false] {
            let mut app = App::new(
                vec![resumable_session(&dir, "sess-handoff")],
                Scope::All,
                PathBuf::from("/tmp"),
            );
            seed_live(&mut app, &[]);
            app.open_agent_picker(vec![def_agent("planner")]);
            press(&mut app, KeyCode::Enter);
            type_into_draft(&mut app, "ship the thing");
            assert!(
                matches!(press(&mut app, KeyCode::Enter), Outcome::BgLaunch(_)),
                "the draft must dispatch for the card to be in flight"
            );
            assert!(app.draft.is_some(), "the dispatched card is up");

            let out = if fork {
                press_ctrl(&mut app, KeyCode::Char('f'))
            } else {
                press(&mut app, KeyCode::Enter)
            };
            assert!(
                matches!(out, Outcome::Resume(_)),
                "the row must really hand off, or this proves nothing (fork={fork})"
            );
            assert!(
                app.draft.is_none(),
                "the in-flight card must not survive the hand-off (fork={fork})"
            );
            assert!(
                !app.preview_pointer_blocked(),
                "a stranded card leaves the mouse gated forever (fork={fork})"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A finished launch may only close the card IT dispatched — NEVER a compose
    /// the user opened after dispatching.
    ///
    /// The card deliberately outlives `Enter`, so the completion event lands on
    /// whatever surface happens to be open when it arrives. Closing blindly there
    /// destroys a quick reply mid-sentence: the typed buffer is gone with no
    /// warning, and the only thing the user did was not sit still for the second
    /// or two the launch took. The guard is the shape a quick reply is cleared by
    /// (`App::clear_sending`) — the request carries an identity, the completion
    /// event carries it back, and the handler acts only on a match.
    #[test]
    fn a_finished_launch_never_closes_a_compose_opened_after_it() {
        let mut app = app_with("idle", None);
        app.open_agent_picker(vec![def_agent("planner")]);
        press(&mut app, KeyCode::Enter);
        type_into_draft(&mut app, "ship the thing");
        let Outcome::BgLaunch(req) = press(&mut app, KeyCode::Enter) else {
            panic!("the draft must dispatch for this interleaving to exist");
        };

        // The user does not wait for the child: a quick reply is opened and typed
        // into while `claude --bg` is still running.
        press_ctrl(&mut app, KeyCode::Char('r'));
        type_into_draft(&mut app, "and this");
        assert!(app.is_composing(), "Ctrl-R must open the reply editor");

        let out = handle_event(
            &mut app,
            AppEvent::BgLaunchFinished {
                launch_id: req.launch_id,
                status: "background agent started".to_string(),
                success: true,
            },
            &mut store_at(Path::new("/tmp")),
        );

        assert!(matches!(out, Outcome::Continue));
        assert!(
            app.is_composing(),
            "the launch's own completion must not tear down a reply opened after it"
        );
        assert_eq!(
            app.compose
                .as_ref()
                .map(|c| c.textarea.lines().join("\n"))
                .as_deref(),
            Some("and this"),
            "the typed reply must survive an unrelated launch finishing"
        );

        // Control: a SECOND draft opened after the dispatch is equally untouchable —
        // the stale event names the first launch, and the new card never launched.
        app.close_compose();
        app.open_agent_picker(vec![def_agent("planner")]);
        press(&mut app, KeyCode::Enter);
        handle_event(
            &mut app,
            AppEvent::BgLaunchFinished {
                launch_id: req.launch_id,
                status: "background agent started".to_string(),
                success: true,
            },
            &mut store_at(Path::new("/tmp")),
        );
        assert!(
            app.is_composing() && app.draft.is_some(),
            "a stale launch result must not close a draft opened after it"
        );
    }

    /// Two launches dispatched from ONE board session are told apart, so an
    /// OUT-OF-ORDER completion cannot close a card that was never its own.
    ///
    /// Id UNIQUENESS is what makes `launching_draft` an IDENTITY check rather than
    /// the weaker "is any card in flight". Freeze the minting counter and every
    /// dispatch stamps the same id, so the FIRST launch's result closes the SECOND
    /// launch's card — throwing away the placeholder for a child that is still
    /// running, on exactly the interleaving (two dispatches, one board, results in
    /// any order) the guard exists for. Nothing else in the suite forces the two ids
    /// apart: the sibling test's second draft is never dispatched, so it carries no
    /// id to collide with.
    #[test]
    fn a_second_dispatchs_card_survives_the_first_launchs_result() {
        let mut app = app_with("idle", None);

        // Launch A.
        app.open_agent_picker(vec![def_agent("planner")]);
        press(&mut app, KeyCode::Enter);
        type_into_draft(&mut app, "first thing");
        let Outcome::BgLaunch(first) = press(&mut app, KeyCode::Enter) else {
            panic!("the first draft must dispatch for this interleaving to exist");
        };

        // Launch B, drafted and dispatched while A is still running.
        app.open_agent_picker(vec![def_agent("planner")]);
        press(&mut app, KeyCode::Enter);
        type_into_draft(&mut app, "second thing");
        let Outcome::BgLaunch(second) = press(&mut app, KeyCode::Enter) else {
            panic!("the second draft must dispatch for this interleaving to exist");
        };
        assert_ne!(
            first.launch_id, second.launch_id,
            "each dispatch must mint its OWN id, or the guard degrades into \
             'is any card in flight' and cannot tell the two launches apart"
        );

        // A finishes SECOND-to-last in wall-clock order but names the FIRST launch:
        // the card on screen belongs to B and must be left alone.
        let out = handle_event(
            &mut app,
            AppEvent::BgLaunchFinished {
                launch_id: first.launch_id,
                status: "background agent started".to_string(),
                success: true,
            },
            &mut store_at(Path::new("/tmp")),
        );
        assert!(matches!(out, Outcome::Continue));
        assert_eq!(
            app.draft,
            Some(NewSessionDraft {
                agent: None,
                launch_id: Some(second.launch_id),
            }),
            "the first launch's result must leave the second launch's card in flight"
        );

        // Control: B's OWN result does end B's card, so the id asserted above is a
        // matchable one and this is not a card that simply never closes.
        handle_event(
            &mut app,
            AppEvent::BgLaunchFinished {
                launch_id: second.launch_id,
                status: "background agent started".to_string(),
                success: true,
            },
            &mut store_at(Path::new("/tmp")),
        );
        assert!(
            app.draft.is_none(),
            "the second launch's own result ends its card"
        );
    }

    /// A finished launch surfaces its mapped status on the board and nothing else:
    /// there is no row to re-anchor and no in-flight echo to clear, because a
    /// brand-new agent has no session id the board knows yet.
    #[test]
    fn a_finished_bg_launch_only_surfaces_its_status() {
        let mut app = app_with("s", None);
        let out = handle_event(
            &mut app,
            AppEvent::BgLaunchFinished {
                // No card is up (this board never drafted), so no id can match —
                // the status must land all the same.
                launch_id: 0,
                status: "background agent started".to_string(),
                success: true,
            },
            &mut store_at(Path::new("/tmp")),
        );
        assert!(matches!(out, Outcome::Continue));
        assert_eq!(app.status.as_deref(), Some("background agent started"));
        assert!(app.sending.is_empty());
        assert!(!app.is_composing());
    }

    // --- Ctrl-X leader chord: hide / show-hidden / hard-delete / rescan / copy session ID ---

    /// Feed one key EVENT (carrying its modifiers) through `handle_event` against
    /// `store`. The chord tests need both `Ctrl-X` (a modified key) and a real
    /// reload store, which the `press`/`ctrl` helpers cannot express together.
    /// The store is threaded (rather than built per call) so a test's successive
    /// keypresses meet the SAME warm cache the board does.
    fn feed(app: &mut App, ev: KeyEvent, store: &mut SessionStore) -> Outcome {
        handle_event(app, AppEvent::Input(Event::Key(ev)), store)
    }

    /// Write a minimal, PARSEABLE `<id>.jsonl` into a store's encoded-cwd dir so a
    /// real `SessionStore::load_from` discovers it — the hard-delete tests assert
    /// the file truly leaves the store, which a synthetic in-memory session cannot
    /// prove. `ts` orders the sessions deterministically.
    fn write_store_session(dir: &Path, id: &str, ts: &str) {
        let jsonl = format!(
            concat!(
                r#"{{"type":"user","sessionId":"{id}","cwd":"/tmp/proj","#,
                r#""timestamp":"{ts}","message":{{"role":"user","content":"hi"}}}}"#,
                "\n",
            ),
            id = id,
            ts = ts,
        );
        std::fs::write(dir.join(format!("{id}.jsonl")), jsonl).expect("write a store fixture");
    }

    /// Write a store session that belongs to a FORK LINEAGE: a null-parent ROOT
    /// record carrying `root_uuid`, plus one ordinary turn.
    ///
    /// Two files written with the SAME `root_uuid` (and cwd, and branch) are what
    /// a background fork produces — claude copies the transcript verbatim, root
    /// record included — so they derive one `lineage_key` and the board folds them
    /// into a single `(+N)` head. That folded head is the shape the lineage delete
    /// exists for.
    fn write_lineage_session(dir: &Path, id: &str, ts: &str, root_uuid: &str) {
        let jsonl = format!(
            concat!(
                r#"{{"type":"attachment","uuid":"{root}","parentUuid":null,"#,
                r#""sessionId":"{id}","cwd":"/tmp/proj","timestamp":"{ts}"}}"#,
                "\n",
                r#"{{"type":"user","sessionId":"{id}","cwd":"/tmp/proj","#,
                r#""timestamp":"{ts}","message":{{"role":"user","content":"hi"}}}}"#,
                "\n",
            ),
            root = root_uuid,
            id = id,
            ts = ts,
        );
        std::fs::write(dir.join(format!("{id}.jsonl")), jsonl).expect("write a lineage fixture");
    }

    /// Seed claude's ACTIVE list with FULL records — `(session_id, kind, state)` —
    /// and hand back a counter of how many times the board PROBED it.
    ///
    /// `seed_live` cannot express the delete tests: it reports every id as an
    /// INTERACTIVE session, which the writer guard refuses outright, so a
    /// background agent's activity bucket would never be reached. `kind` and
    /// `state` are exactly the two fields that guard reads.
    ///
    /// The counter is the PROBE BUDGET seam: a lineage delete must stay ONE
    /// shell-out no matter how many members it has, and a count is the only way to
    /// see a per-member regression (N spawns on the UI thread) that no other
    /// assertion would notice.
    fn seed_live_records(
        app: &mut App,
        agents: &[(&str, &str, Option<&str>)],
    ) -> std::rc::Rc<std::cell::Cell<u32>> {
        let live: HashMap<String, ReportedAgent> = agents
            .iter()
            .map(|(id, kind, state)| {
                (
                    (*id).to_string(),
                    ReportedAgent {
                        kind: (*kind).to_string(),
                        id: None,
                        state: state.map(str::to_owned),
                        status: None,
                        pid: None,
                        started_at_ms: None,
                    },
                )
            })
            .collect();
        let calls = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let seen = std::rc::Rc::clone(&calls);
        app.set_live_probe(move || {
            seen.set(seen.get() + 1);
            live.clone()
        });
        calls
    }

    /// Task 4.1 (pure): the chord machine maps each completion key and cancels on
    /// everything else — Esc, an unbound key, and a Ctrl combo alike.
    #[test]
    fn chord_key_maps_completions_and_cancels_on_anything_else() {
        assert_eq!(chord_key(key(KeyCode::Char('x'))), ChordOutcome::Hide);
        assert_eq!(chord_key(key(KeyCode::Char('d'))), ChordOutcome::Delete);
        assert_eq!(chord_key(key(KeyCode::Char('h'))), ChordOutcome::ShowHidden);
        assert_eq!(chord_key(key(KeyCode::Char('r'))), ChordOutcome::Rescan);
        assert_eq!(chord_key(key(KeyCode::Char('y'))), ChordOutcome::Copy);
        assert_eq!(chord_key(key(KeyCode::Char('f'))), ChordOutcome::Fold);
        assert_eq!(
            chord_key(key(KeyCode::Char('F'))),
            ChordOutcome::Fold,
            "a held Shift on the follow-up still folds"
        );
        assert_eq!(
            chord_key(key(KeyCode::Char('m'))),
            ChordOutcome::Cancel,
            "`m` is not a chord verb: a model is picked per compose (Ctrl-L), never \
             for the whole board"
        );
        assert_eq!(
            chord_key(key(KeyCode::Char('Y'))),
            ChordOutcome::Copy,
            "a held Shift on the follow-up still copies"
        );
        assert_eq!(
            chord_key(ctrl(KeyCode::Char('y'))),
            ChordOutcome::Cancel,
            "Ctrl-X Ctrl-Y cancels: no completion is bound to a Ctrl combo"
        );
        assert_eq!(
            chord_key(key(KeyCode::Esc)),
            ChordOutcome::Cancel,
            "Esc abandons the chord"
        );
        assert_eq!(
            chord_key(key(KeyCode::Char('z'))),
            ChordOutcome::Cancel,
            "an unbound key abandons the chord"
        );
        assert_eq!(
            chord_key(ctrl(KeyCode::Char('c'))),
            ChordOutcome::Cancel,
            "Ctrl-C abandons the chord rather than being swallowed"
        );
    }

    /// The store cache seen from the board: an ordinary `SessionsChanged` reload
    /// reads NOTHING when no transcript moved, and `Ctrl-X r` — the escape hatch
    /// — drops the cache and reads the whole store again.
    ///
    /// Both halves are in one test because each is the other's control: without
    /// the steady state first, a rescan that forgot to `invalidate` would read
    /// every file anyway (they would all still be inside
    /// `MTIME_SETTLE_WINDOW`) and the second assertion could not fail. Waiting
    /// out that window is the only reason this test is not instant.
    ///
    /// Reaching the steady state takes TWO reloads after the sleep, and that is
    /// the settle window working rather than a wasted round trip: the launch
    /// load ran while the fixtures were still inside the window, so its parses
    /// were deliberately not cached — they read bytes that could still have been
    /// replaced without moving either half of the stamp. The first reload past
    /// the window is the one that takes a parse worth keeping; the second is the
    /// one that costs nothing.
    #[test]
    fn a_steady_reload_reads_nothing_while_ctrl_x_r_re_reads_the_whole_store() {
        let _guard = crate::config::env_lock();
        let root = unique_temp_dir("rescan-store");
        let state = unique_temp_dir("rescan-state");
        std::env::set_var("CLAUDE_PROJECTS_DIR", &root);
        std::env::set_var("SNAPBACK_CONFIG_DIR", &state);

        let proj = root.join("-tmp-proj");
        std::fs::create_dir_all(&proj).expect("create the encoded-cwd dir");
        write_store_session(&proj, "sbres-a", "2026-07-14T10:00:00.000Z");
        write_store_session(&proj, "sbres-b", "2026-07-10T10:00:00.000Z");

        let mut store = store_at(&root);
        let mut app = App::new(
            store.reload().sessions,
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        assert_eq!(store.last_parsed(), 2, "the launch load reads both files");

        // Carry the fixtures past the settle window, after which their stamps
        // may be trusted at all.
        std::thread::sleep(crate::store::MTIME_SETTLE_WINDOW + Duration::from_millis(200));

        // The first reload past the window re-reads both — nothing from inside
        // it was kept — and that is what fills the cache.
        handle_event(&mut app, AppEvent::SessionsChanged, &mut store);
        assert_eq!(
            store.last_parsed(),
            2,
            "a parse taken inside the settle window is never carried over"
        );

        handle_event(&mut app, AppEvent::SessionsChanged, &mut store);
        assert_eq!(
            store.last_parsed(),
            0,
            "a watcher reload over an unchanged store must parse nothing"
        );
        assert_eq!(store.last_discovered(), 2, "discovery still runs in full");
        assert_eq!(app.sessions.len(), 2, "and the board still holds both rows");

        feed(&mut app, ctrl(KeyCode::Char('x')), &mut store);
        let out = feed(&mut app, key(KeyCode::Char('r')), &mut store);

        assert!(matches!(out, Outcome::Continue));
        assert_eq!(
            store.last_parsed(),
            2,
            "Ctrl-X r must DROP the cache, not merely reload it"
        );
        assert_eq!(app.sessions.len(), 2, "the board is rebuilt, not emptied");
        assert_eq!(
            app.status.as_deref(),
            Some(rescan_status(2).as_str()),
            "the rescan reports what it landed on, so the key is never a silent no-op"
        );
        assert!(
            !app.pending_chord,
            "the chord resolves after exactly one key"
        );

        std::env::remove_var("CLAUDE_PROJECTS_DIR");
        std::env::remove_var("SNAPBACK_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&state);
    }

    /// Task 4.1 (leak guard): `Ctrl-X` then a printable follow-up completes the
    /// chord — it must NOT append to the search query, even while a query is active.
    #[test]
    fn ctrl_x_then_a_printable_key_completes_the_chord_without_leaking_into_the_query() {
        let _guard = crate::config::env_lock();
        let state = unique_temp_dir("leak-state");
        std::env::set_var("SNAPBACK_CONFIG_DIR", &state);

        let mut app = App::new(
            vec![session("leak-a")],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        // An ACTIVE query is the exact condition a printable follow-up could corrupt.
        // TYPED through the app's own seam rather than written into the widget, so
        // the caret ends up where a real query leaves it — at the end of the line,
        // which is exactly where a leaked keystroke would land.
        app.push_query_str("foo");
        let mut store = store_at(Path::new("/tmp"));

        feed(&mut app, ctrl(KeyCode::Char('x')), &mut store);
        assert!(app.pending_chord, "Ctrl-X arms the leader chord");

        // `h` (show-hidden) needs no selection or persistence to prove the guard,
        // and must be CONSUMED by the chord rather than typed into the query.
        feed(&mut app, key(KeyCode::Char('h')), &mut store);
        assert_eq!(
            app.query(),
            "foo",
            "the chord follow-up must not leak into the query"
        );
        assert!(
            app.show_hidden,
            "the printable follow-up completed the chord (show-hidden on)"
        );
        assert!(
            !app.pending_chord,
            "the chord resolves after exactly one key"
        );

        std::env::remove_var("SNAPBACK_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&state);
    }

    /// Leak guard for the copy: `Ctrl-X y` completes the chord through the real
    /// routing — the `y` must NOT append to an active query, and the copy's status
    /// is what lands on the line.
    ///
    /// The list is EMPTY on purpose, so the copy takes its no-selection branch: no
    /// copy is requested and the sticky no-selection status is set. The selected
    /// case is driven end to end by the two `ctrl_x_y_on_a_selection_*` tests below.
    #[test]
    fn ctrl_x_y_completes_the_chord_without_leaking_into_the_query() {
        let mut app = App::new(vec![], Scope::All, PathBuf::from("/tmp/launch"));
        assert!(
            app.selected_session().is_none(),
            "an empty list selects nothing, so no copy can be requested"
        );
        app.push_query_str("foo");
        let mut store = store_at(Path::new("/tmp"));

        feed(&mut app, ctrl(KeyCode::Char('x')), &mut store);
        assert!(app.pending_chord, "Ctrl-X arms the leader chord");

        let out = feed(&mut app, key(KeyCode::Char('y')), &mut store);
        assert!(matches!(out, Outcome::Continue));
        assert_eq!(
            app.query(),
            "foo",
            "the chord's `y` must not leak into the query"
        );
        assert!(
            !app.pending_chord,
            "the chord resolves after exactly one key"
        );
        assert_eq!(
            app.status.as_deref(),
            Some(NO_SELECTION_STATUS),
            "`y` completed the chord as a copy (the no-selection status)"
        );
        assert_eq!(
            app.status_ttl, None,
            "the no-selection status is STICKY (no dwell timer), like a refusal"
        );
    }

    /// Drive `Ctrl-X y` on a REAL selected session through `handle_event`, assert
    /// the keypress only REQUESTS the copy (the full id, and nothing claimed on the
    /// line), then land the worker's `CopyFinished` through `handle_event` and
    /// complete it the way the driver does — `finish_copy` — but into `term`, an
    /// injected `Vec<u8>`, so no escape can reach the test run's terminal and no
    /// clipboard tool is ever spawned. Returns the app for the caller's assertions.
    fn copy_selected_through_the_driver_seams(id: &str, copied: bool, term: &mut Vec<u8>) -> App {
        let mut app = app_with(id, None);
        let mut store = store_at(Path::new("/tmp"));

        feed(&mut app, ctrl(KeyCode::Char('x')), &mut store);
        let Outcome::Copy(requested) = feed(&mut app, key(KeyCode::Char('y')), &mut store) else {
            panic!("Ctrl-X y on a selection must hand the driver a copy request");
        };
        assert_eq!(
            requested,
            CopyPayload::SessionId(id.to_string()),
            "the request carries the FULL session id"
        );
        assert_eq!(
            app.status, None,
            "the keypress claims nothing: the status comes from the copy's RESULT"
        );
        assert!(
            !app.pending_chord,
            "the chord resolves after exactly one key"
        );

        let finished = AppEvent::CopyFinished {
            payload: requested,
            copied,
        };
        let Outcome::FinishCopy { payload, copied } = handle_event(&mut app, finished, &mut store)
        else {
            panic!("a CopyFinished must go back to the driver to be completed");
        };
        assert_eq!(payload, CopyPayload::SessionId(id.to_string()));
        finish_copy(&mut app, term, &payload, copied);
        app
    }

    /// Tick the board PAST the transient dwell, so a status that survives is
    /// provably sticky rather than merely young.
    fn tick_past_the_dwell(app: &mut App) {
        let mut store = store_at(Path::new("/tmp"));
        for _ in 0..=STATUS_DWELL_TICKS {
            handle_event(app, AppEvent::Tick, &mut store);
        }
    }

    /// (i) A clipboard TOOL copied the id: the line says so with the full id, NO
    /// OSC 52 escape is written (the id is already on the clipboard), and the line
    /// outlives the transient dwell.
    #[test]
    fn ctrl_x_y_on_a_selection_with_a_tool_copy_says_copied_writes_nothing_and_sticks() {
        let id = "550e8400-e29b-41d4-a716-446655440000";
        let mut term: Vec<u8> = Vec::new();
        let mut app = copy_selected_through_the_driver_seams(id, true, &mut term);

        assert!(
            term.is_empty(),
            "a tool copied the id, so no OSC 52 escape may be written: {:?}",
            String::from_utf8_lossy(&term)
        );
        let copied = copy_status(id);
        assert_eq!(app.status.as_deref(), Some(copied.as_str()));
        assert_eq!(app.status_ttl, None, "set sticky, not transient");
        tick_past_the_dwell(&mut app);
        assert_eq!(
            app.status.as_deref(),
            Some(copied.as_str()),
            "the Copied line must survive more than STATUS_DWELL_TICKS ticks"
        );
    }

    /// (ii) No tool copied it (none to try, or every one failed): the id goes out
    /// as EXACTLY the OSC 52 escape, the line says it was SENT — the full id, and
    /// never "Copied" — and it outlives the transient dwell too.
    #[test]
    fn ctrl_x_y_on_a_selection_without_a_tool_copy_sends_osc52_says_sent_and_sticks() {
        let id = "550e8400-e29b-41d4-a716-446655440000";
        let mut term: Vec<u8> = Vec::new();
        let mut app = copy_selected_through_the_driver_seams(id, false, &mut term);

        assert_eq!(
            term,
            clipboard::osc52_clipboard_sequence(id),
            "the fallback writes exactly the OSC 52 escape for the full id"
        );
        let sent = osc52_sent_status(id);
        assert_eq!(app.status.as_deref(), Some(sent.as_str()));
        assert!(sent.contains(id), "the line carries the full id: {sent:?}");
        assert!(
            !sent.to_lowercase().contains("copied"),
            "the OSC 52 path must never claim a copy: {sent:?}"
        );
        assert_eq!(app.status_ttl, None, "set sticky, not transient");
        tick_past_the_dwell(&mut app);
        assert_eq!(
            app.status.as_deref(),
            Some(sent.as_str()),
            "the Sent line must survive more than STATUS_DWELL_TICKS ticks"
        );
    }

    // --- the preview drag-selection copy: the SAME path, its own wording ------

    /// A selection's status names a SELECTION and its size, never a session id,
    /// and says "Copied" only when a tool exited 0 — the OSC 52 route says "Sent"
    /// and carries the same caveat the id's line does. The session-id rows of the
    /// same function are the `Ctrl-X y` wording, untouched.
    #[test]
    fn a_selection_copy_status_names_a_selection_and_says_copied_only_for_a_tool() {
        let three = CopyPayload::Selection("first\n\nthird".to_string());
        let one = CopyPayload::Selection("only row".to_string());

        assert_eq!(
            copy_result_status(&three, true),
            "Copied selection (3 lines)"
        );
        assert_eq!(copy_result_status(&one, true), "Copied selection (1 line)");

        let sent = copy_result_status(&three, false);
        assert_eq!(
            sent,
            format!("Sent selection (3 lines){OSC52_SENT_STATUS_CAVEAT}")
        );
        assert!(
            !sent.to_lowercase().contains("copied"),
            "the OSC 52 route must never claim a copy: {sent:?}"
        );
        for status in [copy_result_status(&three, true), sent] {
            assert!(
                !status.contains("session ID"),
                "a selection is not a session id: {status:?}"
            );
        }

        let id = "550e8400-e29b-41d4-a716-446655440000";
        let session = CopyPayload::SessionId(id.to_string());
        assert_eq!(copy_result_status(&session, true), copy_status(id));
        assert_eq!(copy_result_status(&session, false), osc52_sent_status(id));
    }

    /// The §11 decision, pinned: a `Ctrl-X y` id's line sticks (the user may have
    /// to select the id off it by hand), a selection's does not.
    #[test]
    fn a_copy_status_sticks_for_a_session_id_and_expires_for_a_selection() {
        assert!(copy_status_is_sticky(&CopyPayload::SessionId(
            "id".to_string()
        )));
        assert!(!copy_status_is_sticky(&CopyPayload::Selection(
            "row".to_string()
        )));
    }

    /// `finish_copy` completes a SELECTION on both routes: a tool copy writes no
    /// escape; the fallback writes EXACTLY the OSC 52 escape for the selected text
    /// (not an id). Either way the line is transient and gone after the dwell.
    #[test]
    fn finish_copy_of_a_selection_reports_the_route_and_expires() {
        let text = "first row\nsecond row";
        let payload = CopyPayload::Selection(text.to_string());
        for copied in [true, false] {
            let mut app = app_with("s", None);
            let mut term: Vec<u8> = Vec::new();

            finish_copy(&mut app, &mut term, &payload, copied);

            if copied {
                assert!(term.is_empty(), "a tool copied it: no escape");
            } else {
                assert_eq!(term, clipboard::osc52_clipboard_sequence(text));
            }
            let expected = copy_result_status(&payload, copied);
            assert_eq!(app.status.as_deref(), Some(expected.as_str()));
            assert!(app.status_ttl.is_some(), "set transient, not sticky");
            tick_past_the_dwell(&mut app);
            assert_eq!(
                app.status, None,
                "a selection's line expires after the dwell (copied: {copied})"
            );
        }
    }

    /// The first transcript row drawn with `needle` in it, and the column `needle`
    /// starts at — read off the BUFFER, never computed, so a drag in a test lands
    /// where the text really is.
    fn drawn_text_cell(buffer: &ratatui::buffer::Buffer, pane: Rect, needle: &str) -> (u16, u16) {
        for y in pane.y..pane.bottom() {
            let row: String = (pane.x..pane.right())
                .filter_map(|x| buffer.cell((x, y)).map(|c| c.symbol().to_string()))
                .collect();
            if let Some(byte) = row.find(needle) {
                let col = u16::try_from(row[..byte].chars().count()).expect("fits a row");
                return (pane.x + col, y);
            }
        }
        panic!("{needle:?} must be drawn inside the pane, or this test proves nothing");
    }

    /// A press, a drag, and the release, each followed by a frame the way the real
    /// loop draws between events. Returns the RELEASE's outcome.
    fn drag_and_release(app: &mut App, from: (u16, u16), to: (u16, u16)) -> Outcome {
        wheel(app, MouseEventKind::Down(MouseButton::Left), from.0, from.1);
        render_board(app);
        wheel(app, MouseEventKind::Drag(MouseButton::Left), to.0, to.1);
        render_board(app);
        wheel(app, MouseEventKind::Up(MouseButton::Left), to.0, to.1)
    }

    /// End to end: a drag over drawn transcript text, released, hands the driver
    /// `Outcome::Copy` with THAT text as a `Selection` — the same request `Ctrl-X y`
    /// makes, so the copy goes through the clipboard tool / OSC 52 path rather
    /// than a writer of its own. The release claims nothing on the status line:
    /// the status comes from the copy's result.
    #[test]
    fn a_finished_drag_hands_the_drawn_text_to_the_one_copy_path() {
        let dir = unique_temp_dir("drag-copy");
        let mut app = link_app(&dir, None);
        let buffer = render_board(&mut app);
        let needle = "filler line";
        let (col, row) = drawn_text_cell(&buffer, app.preview_rect, needle);
        let last = col + u16::try_from(needle.len()).expect("short") - 1;

        let released = drag_and_release(&mut app, (col, row), (last, row));

        let Outcome::Copy(payload) = released else {
            panic!("a finished drag must request a copy");
        };
        assert_eq!(payload, CopyPayload::Selection(needle.to_string()));
        assert_eq!(app.status, None, "nothing is claimed before the copy ran");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every transcript cell the frame drew reverse-videoed, as screen `(col, row)`
    /// — read off the BUFFER, so a test asserts the highlight the user sees. Scoped
    /// to the transcript rect: the list's own selection highlight is `REVERSED` too.
    fn highlighted_cells(buffer: &ratatui::buffer::Buffer, transcript: Rect) -> Vec<(u16, u16)> {
        (transcript.y..transcript.bottom())
            .flat_map(|y| (transcript.x..transcript.right()).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                buffer
                    .cell((x, y))
                    .is_some_and(|c| c.modifier.contains(Modifier::REVERSED))
            })
            .collect()
    }

    /// The column one past `needle`'s last drawn cell on the row it was drawn on.
    fn drawn_text_end(buffer: &ratatui::buffer::Buffer, pane: Rect, needle: &str) -> (u16, u16) {
        let (col, row) = drawn_text_cell(buffer, pane, needle);
        let width = u16::try_from(needle.chars().count()).expect("short needle");
        (col + width, row)
    }

    /// A drag over nothing but BLANK cells — the empty space right of a drawn line
    /// — selects nothing: no cell is reverse-videoed and the release requests no
    /// copy, so the clipboard is never overwritten with an empty string.
    #[test]
    fn a_drag_over_blank_cells_alone_highlights_and_copies_nothing() {
        let dir = unique_temp_dir("drag-blank");
        let mut app = link_app(&dir, None);
        let buffer = render_board(&mut app);
        let (text_end, row) = drawn_text_end(&buffer, app.preview_rect, "filler line 18");
        let from = (text_end + 2, row);
        let to = (text_end + 6, row);
        let transcript = view::preview_transcript_rect(&app);
        assert!(
            transcript.contains(Position { x: to.0, y: to.1 }),
            "the drag must stay inside the transcript, or it proves nothing"
        );
        assert!(
            (from.0..=to.0).all(|x| buffer
                .cell((x, row))
                .is_some_and(|c| c.symbol().trim().is_empty())),
            "the dragged-over cells must be blank, or this is not the case under test"
        );

        let released = drag_and_release(&mut app, from, to);
        let after = render_board(&mut app);

        if let Outcome::Copy(payload) = released {
            panic!("a blank-only drag must request no copy, got {payload:?}");
        }
        assert_eq!(
            highlighted_cells(&after, view::preview_transcript_rect(&app)),
            Vec::<(u16, u16)>::new(),
            "a blank-only drag highlights nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A drag across several rows highlights each row only up to its last DRAWN
    /// cell — the space right of a line is not text, so no row may end in a
    /// reverse-videoed blank. Checked on every row the drag touched, and the rows
    /// are counted so the check cannot pass over an empty highlight.
    #[test]
    fn a_multi_row_drag_never_ends_a_row_in_a_highlighted_blank_cell() {
        let dir = unique_temp_dir("drag-rows");
        let mut app = link_app(&dir, None);
        let buffer = render_board(&mut app);
        let from = drawn_text_cell(&buffer, app.preview_rect, "filler line 18");
        let (end_col, end_row) = drawn_text_end(&buffer, app.preview_rect, "filler line 21");

        drag_and_release(&mut app, from, (end_col - 1, end_row));
        let after = render_board(&mut app);

        let transcript = view::preview_transcript_rect(&app);
        let highlighted = highlighted_cells(&after, transcript);
        let mut rows = 0;
        for y in transcript.y..transcript.bottom() {
            let Some(&(last, _)) = highlighted.iter().rfind(|&&(_, row)| row == y) else {
                continue;
            };
            rows += 1;
            let symbol = after.cell((last, y)).expect("in the buffer").symbol();
            assert!(
                !symbol.trim().is_empty(),
                "row {y} ends in a highlighted BLANK cell at column {last}"
            );
        }
        assert_eq!(rows, 4, "the drag spans the four drawn rows 18..=21");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A drag that STARTS in the blank space right of a line copies from the next
    /// drawn text on: the blank first row contributes nothing, so the copy does not
    /// open with a stray newline.
    #[test]
    fn a_drag_starting_right_of_the_text_copies_no_leading_newline() {
        let dir = unique_temp_dir("drag-leading");
        let mut app = link_app(&dir, None);
        let buffer = render_board(&mut app);
        let (text_end, row) = drawn_text_end(&buffer, app.preview_rect, "filler line 20");
        let (end_col, end_row) = drawn_text_end(&buffer, app.preview_rect, "filler line 22");

        let released = drag_and_release(&mut app, (text_end + 2, row), (end_col - 1, end_row));

        let Outcome::Copy(payload) = released else {
            panic!("a drag over drawn text must request a copy");
        };
        assert_eq!(
            payload,
            CopyPayload::Selection("filler line 21\nfiller line 22".to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- autoscroll: a drag held past the transcript's top or bottom edge -------

    /// Numbered one-row lines in [`scroll_session`]: several viewports of
    /// [`BOARD`]'s preview pane, so a held drag has rows to scroll through in both
    /// directions without reaching either end in a handful of ticks.
    const SCROLL_FILLER_LINES: usize = 60;

    /// A session of [`SCROLL_FILLER_LINES`] numbered lines (`filler line 1` ..),
    /// written to a real file under `dir`. A real render, not a synthetic cache:
    /// the release copies from the same width-scoped cache the pane draws from, so
    /// only a real one proves the two agree.
    fn scroll_session(dir: &Path) -> Session {
        let file = dir.join("sess-scroll.jsonl");
        let body = (1..=SCROLL_FILLER_LINES)
            .map(|i| format!("filler line {i}"))
            .collect::<Vec<_>>()
            .join("\\n");
        let jsonl = format!(
            concat!(
                r#"{{"type":"user","sessionId":"sess-scroll","cwd":"/tmp","#,
                r#""timestamp":"2026-07-01T10:00:00.000Z","#,
                r#""message":{{"role":"user","content":"{body}"}}}}"#,
                "\n",
            ),
            body = body,
        );
        std::fs::write(&file, jsonl).expect("write the scroll fixture");
        Session {
            file,
            session_id: "sess-scroll".to_string(),
            cwd: PathBuf::from("/tmp"),
            git_branch: Some("main".to_string()),
            timestamp: None,
            repo: "repo".to_string(),
            label: "scroll session".to_string(),
            root_uuid: None,
            msg_count: 0,
            content_index: String::new(),
            background: false,
            has_agent_name: false,
            has_agent_setting: false,
            failed_task: None,
        }
    }

    /// An app over [`scroll_session`], in `App`'s default (bottom-anchored) scroll.
    fn scroll_app(dir: &Path) -> App {
        let app = App::new(vec![scroll_session(dir)], Scope::All, PathBuf::from("/tmp"));
        assert_eq!(app.selected.as_deref(), Some("sess-scroll"));
        app
    }

    /// The `N` of every `filler line N` drawn inside `transcript`, top to bottom —
    /// read off the BUFFER, so a test knows which rows the user could see.
    fn drawn_fillers(buffer: &ratatui::buffer::Buffer, transcript: Rect) -> Vec<usize> {
        (transcript.y..transcript.bottom())
            .filter_map(|y| {
                let row = drawn_row(buffer, transcript, y);
                row.trim().strip_prefix("filler line ")?.parse().ok()
            })
            .collect()
    }

    /// The `N` of every line of a copied selection, which must read
    /// `filler line N` and nothing else.
    fn copied_fillers(text: &str) -> Vec<usize> {
        text.lines()
            .map(|line| {
                line.strip_prefix("filler line ")
                    .and_then(|n| n.parse().ok())
                    .unwrap_or_else(|| panic!("copied an unexpected line: {line:?}"))
            })
            .collect()
    }

    /// Press at `from`, then drag the held button to `to` — anywhere on the board,
    /// past the transcript's edge included — with a frame after each, as the loop
    /// draws between events. The button stays DOWN.
    fn hold_drag(app: &mut App, from: (u16, u16), to: (u16, u16)) {
        wheel(app, MouseEventKind::Down(MouseButton::Left), from.0, from.1);
        render_board(app);
        wheel(app, MouseEventKind::Drag(MouseButton::Left), to.0, to.1);
        render_board(app);
    }

    /// The autoscroll step the run loop takes at instant `at` — it takes one after
    /// every event and at every frame deadline — then the frame it draws next;
    /// returns the scroll that frame resolved.
    fn step_and_draw(app: &mut App, at: Instant) -> u32 {
        app.autoscroll_preview_selection(view::preview_transcript_rect(app), at);
        render_board(app);
        app.preview_scroll
    }

    /// The step the run loop takes right after the drag event that put the pointer
    /// past the edge: it only starts the autoscroll's clock. Returns that instant.
    fn start_hold(app: &mut App) -> Instant {
        let started = Instant::now();
        step_and_draw(app, started);
        started
    }

    /// Frames in one checked stretch of a hold: about a quarter second of
    /// [`AUTOSCROLL_FRAME`]s, enough that a pointer one row past [`BOARD`]'s
    /// transcript edge owes a few rows by the end of it, while a single frame owes
    /// less than one.
    const HOLD_FRAMES: u32 = 8;

    /// Keep the pointer still for [`HOLD_FRAMES`] frames after `*clock`, stepping
    /// and drawing at each deadline as the run loop does, and advance `*clock` to
    /// the last one; returns the scroll the last frame resolved.
    fn hold_still(app: &mut App, clock: &mut Instant) -> u32 {
        for _ in 0..HOLD_FRAMES {
            *clock += AUTOSCROLL_FRAME;
            step_and_draw(app, *clock);
        }
        app.preview_scroll
    }

    /// The copied text of a release that must have requested a selection copy.
    fn selection_of(released: Outcome) -> String {
        let Outcome::Copy(CopyPayload::Selection(text)) = released else {
            panic!("the release must request a selection copy");
        };
        text
    }

    /// Held one row BELOW the transcript, the pane keeps scrolling down, stretch
    /// after stretch — the pointer only moved once — and the release copies the
    /// whole run from the press down to the row the pointer's clamped cursor
    /// reached, INCLUDING rows that were below the pane when the button went down.
    #[test]
    fn a_drag_held_below_the_edge_keeps_scrolling_down_and_copies_rows_that_started_off_screen() {
        let dir = unique_temp_dir("autoscroll-down");
        let mut app = scroll_app(&dir);
        press(&mut app, KeyCode::Home);
        let buffer = render_board(&mut app);
        assert_eq!(app.preview_scroll, 0, "premise: the pane starts at the top");
        let transcript = view::preview_transcript_rect(&app);
        let last_at_press = *drawn_fillers(&buffer, transcript)
            .iter()
            .max()
            .expect("premise: filler lines are drawn");
        let from = drawn_text_cell(&buffer, app.preview_rect, "filler line 2");
        let below = (transcript.right() - 1, transcript.bottom());

        hold_drag(&mut app, from, below);
        let mut clock = start_hold(&mut app);
        let mut scroll = app.preview_scroll;
        for stretch in 1..=3 {
            let next = hold_still(&mut app, &mut clock);
            assert!(
                next > scroll,
                "stretch {stretch}: a drag held below the edge scrolls down ({scroll} -> {next})"
            );
            scroll = next;
        }
        let after = render_board(&mut app);
        let last_on_screen = *drawn_fillers(&after, transcript)
            .iter()
            .max()
            .expect("filler lines are drawn");
        let released = wheel(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            below.0,
            below.1,
        );

        let copied = copied_fillers(&selection_of(released));
        assert_eq!(
            copied,
            (2..=last_on_screen).collect::<Vec<_>>(),
            "the copy runs from the pressed line to the last row the cursor reached"
        );
        assert!(
            last_on_screen > last_at_press,
            "rows that were below the pane at the press ({last_at_press}) are copied too"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Held one row ABOVE the transcript, the pane keeps scrolling up, stretch after
    /// stretch, and the release copies from the top row it reached down to the
    /// pressed line.
    #[test]
    fn a_drag_held_above_the_edge_keeps_scrolling_up() {
        let dir = unique_temp_dir("autoscroll-up");
        let mut app = scroll_app(&dir);
        let buffer = render_board(&mut app);
        assert!(
            app.preview_scroll > 0,
            "premise: bottom-anchored, not at the top"
        );
        let transcript = view::preview_transcript_rect(&app);
        let first_at_press = *drawn_fillers(&buffer, transcript)
            .iter()
            .min()
            .expect("premise: filler lines are drawn");
        let pressed = SCROLL_FILLER_LINES - 2;
        let (end_col, row) =
            drawn_text_end(&buffer, app.preview_rect, &format!("filler line {pressed}"));
        let above = (transcript.x, transcript.y - 1);

        hold_drag(&mut app, (end_col - 1, row), above);
        let mut clock = start_hold(&mut app);
        let mut scroll = app.preview_scroll;
        for stretch in 1..=3 {
            let next = hold_still(&mut app, &mut clock);
            assert!(
                next < scroll,
                "stretch {stretch}: a drag held above the edge scrolls up ({scroll} -> {next})"
            );
            scroll = next;
        }
        let after = render_board(&mut app);
        let first_on_screen = *drawn_fillers(&after, transcript)
            .iter()
            .min()
            .expect("filler lines are drawn");
        let released = wheel(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            above.0,
            above.1,
        );

        let copied = copied_fillers(&selection_of(released));
        assert_eq!(copied, (first_on_screen..=pressed).collect::<Vec<_>>());
        assert!(
            first_on_screen < first_at_press,
            "rows that were above the pane at the press ({first_at_press}) are copied too"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Letting go ends the autoscroll: later steps leave the pane where the release
    /// found it — and the loop stops waiting on a deadline for them — with the
    /// finished selection still held.
    #[test]
    fn releasing_a_held_drag_stops_the_autoscroll() {
        let dir = unique_temp_dir("autoscroll-release");
        let mut app = scroll_app(&dir);
        press(&mut app, KeyCode::Home);
        let buffer = render_board(&mut app);
        let transcript = view::preview_transcript_rect(&app);
        let from = drawn_text_cell(&buffer, app.preview_rect, "filler line 2");
        let below = (transcript.right() - 1, transcript.bottom());

        hold_drag(&mut app, from, below);
        let mut clock = start_hold(&mut app);
        assert!(
            hold_still(&mut app, &mut clock) > 0,
            "premise: the held drag autoscrolls"
        );
        selection_of(wheel(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            below.0,
            below.1,
        ));
        assert_eq!(
            app.autoscroll_due_in(transcript, clock),
            None,
            "a released drag sets no deadline"
        );
        let parked = app.preview_scroll;
        for _ in 0..3 {
            assert_eq!(
                hold_still(&mut app, &mut clock),
                parked,
                "a released drag never scrolls again"
            );
        }
        assert!(
            app.has_preview_selection(),
            "the finished selection stays highlighted"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A terminal that loses the button's release reports the next pointer move as
    /// a plain `Moved`, which cannot happen while the button is really held. That
    /// move resolves the press exactly as the release would have — it copies — and
    /// the autoscroll stops, instead of scrolling for as long as the board runs.
    #[test]
    fn a_lost_release_stops_the_autoscroll_at_the_next_pointer_move() {
        let dir = unique_temp_dir("autoscroll-lost");
        let mut app = scroll_app(&dir);
        press(&mut app, KeyCode::Home);
        let buffer = render_board(&mut app);
        let transcript = view::preview_transcript_rect(&app);
        let from = drawn_text_cell(&buffer, app.preview_rect, "filler line 2");
        let below = (transcript.right() - 1, transcript.bottom());

        hold_drag(&mut app, from, below);
        let mut clock = start_hold(&mut app);
        assert!(
            hold_still(&mut app, &mut clock) > 0,
            "premise: the held drag autoscrolls"
        );
        let moved = wheel(&mut app, MouseEventKind::Moved, below.0, below.1 + 1);
        let copied = copied_fillers(&selection_of(moved));
        assert_eq!(copied.first(), Some(&2), "the move copied the selection");
        render_board(&mut app);
        let parked = app.preview_scroll;
        for _ in 0..3 {
            assert_eq!(
                hold_still(&mut app, &mut clock),
                parked,
                "no step scrolls after the lost release"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The board TICK never scrolls a held drag: the run loop's autoscroll step is
    /// the one driver. Ticks arriving a second after the drag's clock started — a
    /// second the step would pay out as most of a page — leave the pane where it
    /// was, and the step then pays it.
    #[test]
    fn a_tick_alone_never_scrolls_a_held_drag() {
        let dir = unique_temp_dir("autoscroll-tick");
        let mut app = scroll_app(&dir);
        press(&mut app, KeyCode::Home);
        let buffer = render_board(&mut app);
        let transcript = view::preview_transcript_rect(&app);
        let from = drawn_text_cell(&buffer, app.preview_rect, "filler line 2");
        let below = (transcript.right() - 1, transcript.bottom());

        hold_drag(&mut app, from, below);
        let a_second_ago = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("the monotonic clock has run for a second");
        let parked = step_and_draw(&mut app, a_second_ago);
        for _ in 0..3 {
            tick(&mut app);
            render_board(&mut app);
            assert_eq!(app.preview_scroll, parked, "a tick scrolled the held drag");
        }
        assert!(
            step_and_draw(&mut app, Instant::now()) > parked,
            "premise: the step owes the second that passed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A resize clears the selection — a finished one and a held one alike. The
    /// selection is anchored to wrapped transcript rows, and a new width re-wraps
    /// the transcript, so the same row number would name other text.
    #[test]
    fn a_resize_clears_the_preview_selection() {
        let dir = unique_temp_dir("select-resize");
        let mut app = link_app(&dir, None);
        let buffer = render_board(&mut app);
        let from = drawn_text_cell(&buffer, app.preview_rect, "filler line 18");
        let (end_col, end_row) = drawn_text_end(&buffer, app.preview_rect, "filler line 19");
        drag_and_release(&mut app, from, (end_col - 1, end_row));
        assert!(app.has_preview_selection(), "premise: a selection is held");

        let resized = handle_event(
            &mut app,
            AppEvent::Input(Event::Resize(BOARD.0 - 10, BOARD.1)),
            &mut store_at(Path::new("/tmp")),
        );
        assert!(matches!(resized, Outcome::Continue));
        assert!(
            !app.has_preview_selection(),
            "a resize clears a finished selection"
        );
        let after = render_board(&mut app);
        assert_eq!(
            highlighted_cells(&after, view::preview_transcript_rect(&app)),
            Vec::<(u16, u16)>::new(),
            "and the next frame highlights nothing"
        );

        // A HELD drag goes too: its release afterwards copies nothing.
        hold_drag(&mut app, from, (end_col - 1, end_row));
        assert!(app.has_preview_selection(), "premise: a drag is held");
        handle_event(
            &mut app,
            AppEvent::Input(Event::Resize(BOARD.0, BOARD.1)),
            &mut store_at(Path::new("/tmp")),
        );
        assert!(!app.has_preview_selection());
        let released = wheel(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            end_col - 1,
            end_row,
        );
        assert!(
            !matches!(released, Outcome::Copy(_)),
            "the release of a press a resize ended copies nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The highlight is drawn only inside the transcript rect of the frame being
    /// drawn — never on the pinned row, a border, or any other row — even when the
    /// transcript moved under a finished selection (the pinned row took the pane's
    /// first row back) or the selection is taller than the pane (autoscroll).
    #[test]
    fn the_highlight_never_covers_a_row_outside_the_current_transcript_area() {
        // The pinned row returns under a finished selection that began on the
        // transcript's first row: a reply in flight to the selected session stands
        // the pinned row down (`view::preview_banner`), and its end brings it back.
        // Read from the top (`Home`), so the reply's tail leaving does not move the
        // scroll and the selection's first row sits right under the pinned row.
        let dir = unique_temp_dir("select-inside");
        let mut app = link_app(&dir, None);
        let mut reported = HashMap::new();
        reported.insert(
            "sess-link".to_string(),
            ReportedAgent {
                kind: "background".to_string(),
                id: None,
                state: Some("blocked".to_string()),
                status: None,
                pid: None,
                started_at_ms: None,
            },
        );
        app.set_reported_agents(reported, None);
        app.sending = vec![replying_to("sess-link")];
        press(&mut app, KeyCode::Home);
        render_board(&mut app);
        let before = view::preview_transcript_rect(&app);
        drag_and_release(&mut app, (before.x, before.y), (before.x + 5, before.y + 2));
        assert!(app.has_preview_selection(), "premise: a selection is held");
        app.sending.clear();
        let after = render_board(&mut app);
        let now = view::preview_transcript_rect(&app);
        assert_eq!(
            now.y,
            before.y + 1,
            "premise: the pinned row took the first row"
        );
        let pinned: String = (now.x..now.right())
            .filter_map(|x| after.cell((x, before.y)))
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            !pinned.trim().is_empty(),
            "premise: the pinned row has drawn text a stray highlight would cover"
        );
        let lit = highlighted_cells(&after, app.preview_rect);
        assert!(!lit.is_empty(), "premise: the selection is still drawn");
        for (x, y) in lit {
            assert!(
                now.contains(Position { x, y }),
                "({x}, {y}) is highlighted outside the transcript rect {now:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);

        // A selection taller than the pane: every row of the pane is inside it,
        // and nothing outside the pane's transcript rect is.
        let dir = unique_temp_dir("select-tall");
        let mut app = scroll_app(&dir);
        press(&mut app, KeyCode::Home);
        let buffer = render_board(&mut app);
        let transcript = view::preview_transcript_rect(&app);
        let from = drawn_text_cell(&buffer, app.preview_rect, "filler line 2");
        hold_drag(
            &mut app,
            from,
            (transcript.right() - 1, transcript.bottom()),
        );
        let mut clock = start_hold(&mut app);
        for _ in 0..4 {
            hold_still(&mut app, &mut clock);
        }
        let after = render_board(&mut app);
        let lit = highlighted_cells(&after, app.preview_rect);
        for &(x, y) in &lit {
            assert!(
                transcript.contains(Position { x, y }),
                "({x}, {y}) is highlighted outside the transcript rect {transcript:?}"
            );
        }
        let rows: std::collections::BTreeSet<u16> = lit.iter().map(|&(_, y)| y).collect();
        assert_eq!(
            rows.len(),
            usize::from(transcript.height),
            "the selection spans the whole pane, so every transcript row is lit"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The in-flight reply's optimistic tail is drawn below the transcript but is
    /// not transcript: a drag through it neither highlights it nor copies it — the
    /// one place the drawn frame and the transcript cache would otherwise disagree.
    #[test]
    fn a_selection_never_highlights_or_copies_the_in_flight_reply_tail() {
        let dir = unique_temp_dir("select-tail");
        let mut app = link_app(&dir, None);
        app.sending = vec![replying_to("sess-link")];
        let buffer = render_board(&mut app);
        let transcript = view::preview_transcript_rect(&app);
        let transcript_rows = app.preview_wrapped_rows(transcript.width);
        let scroll = usize::try_from(app.preview_scroll).expect("fits");
        let tail_top = transcript.y
            + u16::try_from(transcript_rows - scroll).expect("the tail starts inside the pane");
        let (_, echo_row) = drawn_text_cell(&buffer, app.preview_rect, "still landing");
        assert!(
            echo_row >= tail_top && echo_row < transcript.bottom(),
            "premise: the reply tail is drawn inside the transcript rect"
        );
        let from = drawn_text_cell(&buffer, app.preview_rect, "filler line 23");

        let released = drag_and_release(
            &mut app,
            from,
            (transcript.right() - 1, transcript.bottom() - 1),
        );
        let text = selection_of(released);
        assert!(
            !text.contains("still landing") && !text.contains("cooking"),
            "the tail is not copied: {text:?}"
        );
        assert!(text.starts_with("filler line 23\n"), "{text:?}");
        assert!(
            text.lines().last().is_some_and(|l| l.contains("docs")),
            "the copy ends on the transcript's own last line: {text:?}"
        );
        let after = render_board(&mut app);
        let lit = highlighted_cells(&after, transcript);
        assert!(!lit.is_empty(), "premise: the transcript part is lit");
        assert!(
            lit.iter().all(|&(_, y)| y < tail_top),
            "no tail row is highlighted"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- double-click word selection: timed presses, one word, the same copy ----

    /// One left-button event at `cell` at instant `at`, then a frame — the loop
    /// draws between events.
    fn button(app: &mut App, kind: MouseEventKind, cell: (u16, u16), at: Instant) -> MouseEffect {
        let effect = mouse_effect(app, mouse_ev(kind, cell.0, cell.1), at);
        render_board(app);
        effect
    }

    /// A press and release at `cell` at `at`; returns the release's effect.
    fn click_at(app: &mut App, cell: (u16, u16), at: Instant) -> MouseEffect {
        button(app, MouseEventKind::Down(MouseButton::Left), cell, at);
        button(app, MouseEventKind::Up(MouseButton::Left), cell, at)
    }

    /// The cell one column into `needle`'s first drawn occurrence, and its row.
    fn inside_word(app: &mut App, needle: &str) -> (u16, u16) {
        let buffer = render_board(app);
        let (col, row) = drawn_text_cell(&buffer, app.preview_rect, needle);
        (col + 1, row)
    }

    /// End to end: two presses on the same cell within the interval copy exactly
    /// the word under it — and the cells reverse-videoed are those same cells.
    #[test]
    fn a_double_click_copies_exactly_the_word_and_highlights_its_cells() {
        let dir = unique_temp_dir("dbl-word");
        let mut app = link_app(&dir, None);
        let cell = inside_word(&mut app, "filler line 18");
        let t0 = Instant::now();

        click_at(&mut app, cell, t0);
        button(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            cell,
            t0 + Duration::from_millis(100),
        );
        let after = render_board(&mut app);
        let released = button(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            cell,
            t0 + Duration::from_millis(150),
        );

        assert_eq!(released, MouseEffect::Copy("filler".to_string()));
        let lit = highlighted_cells(&after, view::preview_transcript_rect(&app));
        let shown: String = lit
            .iter()
            .map(|&(x, y)| after.cell((x, y)).expect("in buffer").symbol())
            .collect();
        assert_eq!(shown, "filler", "the highlight is the copied text");
        assert!(lit.iter().all(|&(_, y)| y == cell.1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A double-click on a blank cell selects nothing: no highlight, and the
    /// release copies nothing — the same blank rule a drag has.
    #[test]
    fn a_double_click_on_a_blank_cell_copies_nothing() {
        let dir = unique_temp_dir("dbl-blank");
        let mut app = link_app(&dir, None);
        let buffer = render_board(&mut app);
        let (end, row) = drawn_text_end(&buffer, app.preview_rect, "filler line 18");
        let cell = (end + 3, row);
        let t0 = Instant::now();

        click_at(&mut app, cell, t0);
        button(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            cell,
            t0 + Duration::from_millis(100),
        );
        let after = render_board(&mut app);
        let released = button(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            cell,
            t0 + Duration::from_millis(150),
        );

        assert_eq!(released, MouseEffect::None);
        assert_eq!(
            highlighted_cells(&after, view::preview_transcript_rect(&app)),
            Vec::<(u16, u16)>::new()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A second press after the interval, or on another cell, is a fresh first
    /// click — two clicks, never a word.
    #[test]
    fn a_slow_or_moved_second_click_stays_two_clicks() {
        let dir = unique_temp_dir("dbl-slow");
        let mut app = link_app(&dir, None);
        let cell = inside_word(&mut app, "filler line 18");
        let next = (cell.0 + 1, cell.1);
        let t0 = Instant::now();

        click_at(&mut app, cell, t0);
        button(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            cell,
            t0 + DOUBLE_CLICK_INTERVAL + Duration::from_millis(1),
        );
        assert!(!app.has_preview_selection(), "too slow for a double-click");
        button(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            cell,
            t0 + DOUBLE_CLICK_INTERVAL + Duration::from_millis(1),
        );

        let t1 = t0 + Duration::from_secs(5);
        click_at(&mut app, cell, t1);
        button(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            next,
            t1 + Duration::from_millis(100),
        );
        assert!(!app.has_preview_selection(), "another cell is a new click");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The second press's release with a zero-length drag in between (the pointer
    /// jitters off and back onto the cell) must not erase the word.
    #[test]
    fn a_zero_length_drag_keeps_the_double_clicked_word() {
        let dir = unique_temp_dir("dbl-zero");
        let mut app = link_app(&dir, None);
        let cell = inside_word(&mut app, "filler line 18");
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(100);

        click_at(&mut app, cell, t0);
        button(&mut app, MouseEventKind::Down(MouseButton::Left), cell, t1);
        button(&mut app, MouseEventKind::Drag(MouseButton::Left), cell, t1);
        let released = button(&mut app, MouseEventKind::Up(MouseButton::Left), cell, t1);

        assert_eq!(released, MouseEffect::Copy("filler".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A real drag after the second press is a character selection from the press
    /// cell, as any drag is.
    #[test]
    fn a_real_drag_after_the_second_press_is_a_character_selection() {
        let dir = unique_temp_dir("dbl-drag");
        let mut app = link_app(&dir, None);
        let buffer = render_board(&mut app);
        let (col, row) = drawn_text_cell(&buffer, app.preview_rect, "filler line 18");
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(100);

        click_at(&mut app, (col, row), t0);
        button(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            (col, row),
            t1,
        );
        button(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            (col + 10, row),
            t1,
        );
        let released = button(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            (col + 10, row),
            t1,
        );

        assert_eq!(released, MouseEffect::Copy("filler line".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A press that turned into a drag is not a first click: pressing the same
    /// cell again inside the interval is an ordinary press.
    #[test]
    fn a_press_that_became_a_drag_does_not_count_as_a_first_click() {
        let dir = unique_temp_dir("dbl-dragfirst");
        let mut app = link_app(&dir, None);
        let buffer = render_board(&mut app);
        let (col, row) = drawn_text_cell(&buffer, app.preview_rect, "filler line 18");
        let t0 = Instant::now();

        button(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            (col, row),
            t0,
        );
        button(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            (col + 5, row),
            t0,
        );
        button(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            (col + 5, row),
            t0,
        );
        button(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            (col, row),
            t0 + Duration::from_millis(100),
        );

        assert!(!app.has_preview_selection(), "an ordinary press deselects");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A quick third click keeps the word selected (and copies it again) rather
    /// than deselecting it; a keypress resets the chain.
    #[test]
    fn a_third_quick_click_keeps_the_word_and_a_key_resets_the_chain() {
        let dir = unique_temp_dir("dbl-third");
        let mut app = link_app(&dir, None);
        let cell = inside_word(&mut app, "filler line 18");
        let t0 = Instant::now();

        click_at(&mut app, cell, t0);
        click_at(&mut app, cell, t0 + Duration::from_millis(100));
        button(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            cell,
            t0 + Duration::from_millis(200),
        );
        assert!(
            app.has_preview_selection(),
            "the third click keeps the word"
        );
        let released = button(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            cell,
            t0 + Duration::from_millis(250),
        );
        assert_eq!(released, MouseEffect::Copy("filler".to_string()));

        app.clear_preview_selection();
        button(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            cell,
            t0 + Duration::from_millis(300),
        );
        assert!(!app.has_preview_selection(), "the chain was reset");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The pure decision: same cell, within the interval (boundary inclusive),
    /// a prior click — and nothing else.
    #[test]
    fn is_double_click_is_a_pure_function_of_cell_and_time() {
        let t0 = Instant::now();
        let here = Position { x: 4, y: 2 };
        let prev = Some(ClickRecord { pos: here, at: t0 });
        assert!(is_double_click(prev, here, t0 + DOUBLE_CLICK_INTERVAL));
        assert!(!is_double_click(
            prev,
            here,
            t0 + DOUBLE_CLICK_INTERVAL + Duration::from_millis(1)
        ));
        assert!(!is_double_click(prev, Position { x: 5, y: 2 }, t0));
        assert!(!is_double_click(None, here, t0));
    }

    /// Task 4.4: `Ctrl-X x` on a non-hidden selected session hides it, PERSISTS the
    /// hide, shrinks the visible list, and clamps the selection to the nearest row.
    #[test]
    fn ctrl_x_x_hides_the_selected_session_persists_and_clamps_selection() {
        let _guard = crate::config::env_lock();
        let state = unique_temp_dir("hide-state");
        std::env::set_var("SNAPBACK_CONFIG_DIR", &state);

        let mut app = App::new(
            vec![session("sbx-a"), session("sbx-b"), session("sbx-c")],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        // Stand on the LAST row so hiding it must clamp, not stay put.
        app.move_selection(2);
        assert_eq!(app.selected.as_deref(), Some("sbx-c"));
        assert_eq!(app.filtered.len(), 3, "all three rows are visible to start");
        let mut store = store_at(Path::new("/tmp"));

        feed(&mut app, ctrl(KeyCode::Char('x')), &mut store);
        feed(&mut app, key(KeyCode::Char('x')), &mut store);

        assert_eq!(
            app.filtered.len(),
            2,
            "hiding a row shrinks the visible list"
        );
        assert_eq!(
            app.selected.as_deref(),
            Some("sbx-b"),
            "the selection clamps to the nearest surviving row, not the hidden one"
        );
        assert!(
            app.hidden_ids.contains("sbx-c"),
            "the row is in the hidden set"
        );
        assert!(
            crate::hidden::load_hidden(&crate::config::state_dir()).contains("sbx-c"),
            "the hide is persisted to the state dir"
        );

        std::env::remove_var("SNAPBACK_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&state);
    }

    /// Task 4.4: `Ctrl-X x` on a HIDDEN row (with show-hidden on) un-hides it and
    /// persists the removal from the hidden set.
    #[test]
    fn ctrl_x_x_on_a_hidden_row_unhides_it_when_show_hidden_is_on() {
        let _guard = crate::config::env_lock();
        let state = unique_temp_dir("unhide-state");
        std::env::set_var("SNAPBACK_CONFIG_DIR", &state);

        let mut app = App::new(
            vec![session("sbx-a"), session("sbx-b")],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        // Precondition: sbx-b is hidden AND the show-hidden view is on, so the
        // hidden row is on screen for the un-hide. `toggle_show_hidden` flips the
        // view on (false -> true) and re-filters with the new hidden id in place.
        app.hidden_ids.insert("sbx-b".to_string());
        app.toggle_show_hidden();
        assert!(app.show_hidden);
        app.move_selection(1);
        assert_eq!(
            app.selected.as_deref(),
            Some("sbx-b"),
            "standing on the hidden row"
        );
        let mut store = store_at(Path::new("/tmp"));

        feed(&mut app, ctrl(KeyCode::Char('x')), &mut store);
        feed(&mut app, key(KeyCode::Char('x')), &mut store);

        assert!(
            !app.hidden_ids.contains("sbx-b"),
            "Ctrl-X x un-hides the hidden row"
        );
        assert!(
            !crate::hidden::load_hidden(&crate::config::state_dir()).contains("sbx-b"),
            "the un-hide is persisted"
        );

        std::env::remove_var("SNAPBACK_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&state);
    }

    /// Task 4.4: `Ctrl-X h` toggles the show-hidden view on and back off.
    #[test]
    fn ctrl_x_h_toggles_the_show_hidden_view() {
        let _guard = crate::config::env_lock();
        let state = unique_temp_dir("showhidden-state");
        std::env::set_var("SNAPBACK_CONFIG_DIR", &state);

        let mut app = App::new(
            vec![session("sbx-a")],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        assert!(!app.show_hidden, "hidden rows are off the board by default");
        let mut store = store_at(Path::new("/tmp"));

        feed(&mut app, ctrl(KeyCode::Char('x')), &mut store);
        feed(&mut app, key(KeyCode::Char('h')), &mut store);
        assert!(app.show_hidden, "Ctrl-X h reveals hidden rows");

        feed(&mut app, ctrl(KeyCode::Char('x')), &mut store);
        feed(&mut app, key(KeyCode::Char('h')), &mut store);
        assert!(!app.show_hidden, "Ctrl-X h again hides them");

        std::env::remove_var("SNAPBACK_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&state);
    }

    /// `Ctrl-X f` toggles the selected row's fork lineage BOTH ways, pressed
    /// through `handle_event`: a folded `(+N)` head opens, an open lineage folds —
    /// from the head or from a child, which the fold retargets to its head — and the
    /// `f` is consumed by the chord every time rather than typed into the query.
    /// The query is live on purpose: a `(+N)` head found BY searching is the row a
    /// user most wants to open, so the toggle must not depend on the query being
    /// empty.
    #[test]
    fn ctrl_x_f_folds_and_expands_the_selected_rows_lineage() {
        let mut head = session("sbf-head");
        head.root_uuid = Some("root-shared".to_string());
        let mut fork = session("sbf-fork");
        fork.root_uuid = Some("root-shared".to_string());
        let mut app = App::new(vec![head, fork], Scope::All, PathBuf::from("/tmp/launch"));
        let mut store = store_at(Path::new("/tmp"));
        type_into_board(&mut app, "label");
        assert_eq!(app.filtered.len(), 1, "premise: the lineage starts folded");
        let head_id = app.selected.clone().expect("the folded head is selected");
        let mut ctrl_x_f = |app: &mut App| {
            feed(app, ctrl(KeyCode::Char('x')), &mut store);
            feed(app, key(KeyCode::Char('f')), &mut store);
            assert!(!app.pending_chord, "the chord resolves on its one key");
            assert_eq!(app.query(), "label", "the `f` must not leak into the query");
        };

        ctrl_x_f(&mut app);
        assert_eq!(app.filtered.len(), 2, "a folded head opens");
        assert_eq!(app.selected.as_deref(), Some(head_id.as_str()));

        ctrl_x_f(&mut app);
        assert_eq!(app.filtered.len(), 1, "an open lineage folds from its head");

        ctrl_x_f(&mut app);
        press(&mut app, KeyCode::Down);
        assert_ne!(
            app.selected.as_deref(),
            Some(head_id.as_str()),
            "premise: the selection stands on the child"
        );
        ctrl_x_f(&mut app);
        assert_eq!(app.filtered.len(), 1, "and from a child");
        assert_eq!(
            app.selected.as_deref(),
            Some(head_id.as_str()),
            "which hands the selection to the head the fold keeps"
        );
    }

    /// `Ctrl-X f` on a row with no lineage to toggle — a session that is its own
    /// lineage — changes nothing: the list, the selection and the query all stay.
    #[test]
    fn ctrl_x_f_on_a_row_with_nothing_to_fold_changes_nothing() {
        let mut app = app_with("sbf-lone", None);
        let mut store = store_at(Path::new("/tmp"));
        let filtered = app.filtered.clone();

        feed(&mut app, ctrl(KeyCode::Char('x')), &mut store);
        feed(&mut app, key(KeyCode::Char('f')), &mut store);

        assert!(!app.pending_chord, "the chord resolves on its one key");
        assert_eq!(app.filtered, filtered, "nothing to fold or open");
        assert_eq!(app.selected.as_deref(), Some("sbf-lone"));
        assert!(app.query().is_empty(), "`f` must not leak into the query");
    }

    /// `Ctrl-X m` is NOT a chord verb any more: there is no board-wide model to
    /// pick. `m` after the leader is an unbound follow-up, so it abandons the chord
    /// like any other — opening nothing, and (the leak guard) never reaching the
    /// search query.
    #[test]
    fn ctrl_x_m_opens_nothing_and_leaks_nothing() {
        let mut app = App::new(
            vec![session("sbx-a")],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        let mut store = store_at(Path::new("/tmp"));

        feed(&mut app, ctrl(KeyCode::Char('x')), &mut store);
        assert!(app.pending_chord, "Ctrl-X arms the leader chord");
        feed(&mut app, key(KeyCode::Char('m')), &mut store);

        assert!(!app.pending_chord, "the chord resolves on its one key");
        assert!(app.modal.is_none(), "no model picker opens from the board");
        assert!(app.query().is_empty(), "`m` must not leak into the query");
    }

    /// The whole per-compose pick, end to end through `handle_event`: `Ctrl-L` in a
    /// REPLY opens the picker over it, `Enter` on a model row writes that model into
    /// THIS compose — text untouched — and the next compose starts on its default
    /// again, even though a pick was confirmed in the previous one. The pick is
    /// compose state, never board state.
    #[test]
    fn a_pick_is_scoped_to_one_compose_and_the_next_starts_at_default() {
        let mut app = app_with("sbc-scope", None);

        // Ctrl-R on a session claude is not holding opens the reply compose.
        press_ctrl(&mut app, KeyCode::Char('r'));
        assert!(app.is_composing(), "Ctrl-R opens the reply compose");
        type_into_draft(&mut app, "hello");
        assert_eq!(
            app.compose.as_ref().and_then(|c| c.model.clone()),
            None,
            "a fresh compose starts at its default"
        );

        press_ctrl(&mut app, KeyCode::Char('l'));
        assert!(app.modal.is_some(), "Ctrl-L opens the model picker");
        assert!(app.is_composing(), "over the compose, which stays open");
        // Move off the default row onto the first alias (the seed's `fable`), step
        // its effort once, and confirm.
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Enter);

        assert!(app.modal.is_none(), "Enter closes the picker");
        let compose = app.compose.as_ref().expect("and returns to the compose");
        assert_eq!(
            compose.model,
            Some(ModelPick {
                model: "fable".to_string(),
                effort: Some("low"),
            }),
            "Enter sets the row's model and effort into THIS compose"
        );
        assert_eq!(compose.textarea.lines(), ["hello"], "the text is untouched");

        // Leave the compose and open another: it starts at its default again.
        press(&mut app, KeyCode::Esc);
        assert!(!app.is_composing(), "Esc closes the compose");
        press_ctrl(&mut app, KeyCode::Char('r'));
        assert!(app.is_composing(), "a second reply compose opens");
        assert_eq!(
            app.compose.as_ref().and_then(|c| c.model.clone()),
            None,
            "a pick confirmed in the previous compose must not carry over"
        );
        compose::open_background(&mut app, None);
        assert_eq!(
            app.compose.as_ref().and_then(|c| c.model.clone()),
            None,
            "nor into a new-session draft"
        );
    }

    /// `Esc` from the picker returns to the compose with BOTH the text and the
    /// PREVIOUS pick intact — the picker's rows are its own copies, so nothing a
    /// highlight move or an effort step did while it was open reaches the compose.
    #[test]
    fn esc_from_the_model_picker_keeps_the_text_and_the_previous_pick() {
        let mut app = App::new(
            vec![session("sbx-a")],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        compose::open_background(&mut app, None);
        type_into_draft(&mut app, "draft text");
        let fable = ModelPick::new("fable");
        app.set_compose_model(Some(fable.clone()));

        press_ctrl(&mut app, KeyCode::Char('l'));
        assert_eq!(
            app.modal.as_ref().and_then(|m| m.selected_action()),
            Some(&ModalAction::SetModel(Some(fable.clone()))),
            "the picker opens ON the compose's current pick"
        );
        // Move to another row and give it an effort, then walk away.
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Esc);

        assert!(app.modal.is_none(), "Esc closes the picker");
        let compose = app.compose.as_ref().expect("the compose is still open");
        assert_eq!(
            compose.textarea.lines(),
            ["draft text"],
            "the text is intact"
        );
        assert_eq!(
            compose.model,
            Some(fable),
            "the previous pick is intact — nothing the picker did reached it"
        );
    }

    /// `Ctrl-O` on the MODEL picker must launch nothing. It is a `List`-layout
    /// modal, so [`modal_key`] binds the key — the second gate, in
    /// [`launch_pick_interactively`], is what keeps a non-`New` choice inert.
    #[test]
    fn ctrl_o_on_the_model_picker_launches_nothing() {
        let mut app = App::new(
            vec![session("sbx-a")],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        let mut store = store_at(Path::new("/tmp"));
        compose::open_background(&mut app, None);

        feed(&mut app, ctrl(KeyCode::Char('l')), &mut store);
        let outcome = handle_event(
            &mut app,
            AppEvent::Input(Event::Key(ctrl(KeyCode::Char('o')))),
            &mut store,
        );

        assert!(
            matches!(outcome, Outcome::Continue),
            "Ctrl-O on a model row must not hand off"
        );
        assert!(
            app.modal.is_some(),
            "and must not even close the picker: it is simply unbound here"
        );
        assert_eq!(app.compose.as_ref().and_then(|c| c.model.clone()), None);
    }

    /// In the MODEL picker `→`/`←` step the highlighted model row's effort, and ONE
    /// `Enter` sets the model and the effort together into the compose. On the
    /// default row the arrows do nothing — there is no model for an effort to
    /// belong to — and the picker re-opens on the compose's pick at its effort.
    #[test]
    fn the_model_pickers_arrows_set_an_effort_that_enter_confirms_with_the_model() {
        let mut app = App::new(
            vec![session("sbx-a")],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        let mut store = store_at(Path::new("/tmp"));
        compose::open_background(&mut app, None);
        let pick = |app: &App| app.compose.as_ref().and_then(|c| c.model.clone());

        feed(&mut app, ctrl(KeyCode::Char('l')), &mut store);
        // The default row: each arrow is a no-op that leaves the picker as it was
        // (checked per press, so a Right and a Left cannot cancel each other out).
        let before = app.modal.clone();
        for code in [KeyCode::Right, KeyCode::Left] {
            feed(&mut app, key(code), &mut store);
            assert_eq!(
                app.modal, before,
                "{code:?} does nothing on the default row"
            );
        }

        // First alias (fable): → three times walks unset -> low -> medium -> high.
        feed(&mut app, key(KeyCode::Down), &mut store);
        for _ in 0..3 {
            feed(&mut app, key(KeyCode::Right), &mut store);
        }
        assert_eq!(pick(&app), None, "adjusting sets nothing until Enter");
        feed(&mut app, key(KeyCode::Enter), &mut store);
        let fable_high = ModelPick {
            model: "fable".to_string(),
            effort: Some("high"),
        };
        assert_eq!(
            pick(&app),
            Some(fable_high.clone()),
            "one Enter confirms the model and its effort together"
        );

        // Re-open: the picked row is highlighted at the picked effort, and Enter
        // re-confirms it unchanged.
        feed(&mut app, ctrl(KeyCode::Char('l')), &mut store);
        assert_eq!(
            app.modal.as_ref().and_then(|m| m.selected_action()),
            Some(&ModalAction::SetModel(Some(fable_high.clone())))
        );
        feed(&mut app, key(KeyCode::Enter), &mut store);
        assert_eq!(pick(&app), Some(fable_high.clone()));

        // ← from unset wraps to the top level on another row.
        feed(&mut app, ctrl(KeyCode::Char('l')), &mut store);
        feed(&mut app, key(KeyCode::Down), &mut store); // haiku, unset
        feed(&mut app, key(KeyCode::Left), &mut store);
        assert_eq!(
            app.modal.as_ref().and_then(|m| m.selected_action()),
            Some(&ModalAction::SetModel(Some(ModelPick {
                model: "haiku".to_string(),
                effort: Some("max"),
            })))
        );
        // And the default row takes the pick away again.
        while app.modal.as_ref().expect("the picker is open").selected != 0 {
            feed(&mut app, key(KeyCode::Up), &mut store);
        }
        feed(&mut app, key(KeyCode::Enter), &mut store);
        assert_eq!(
            pick(&app),
            None,
            "the default row returns the compose to no pick"
        );
    }

    /// `←`/`→` inside the open model picker must NEVER reach the board, where the
    /// same keys move the search caret. The board's caret starts MID-query, so a
    /// leaked arrow in EITHER direction would move it, and the CONTROL at the end
    /// proves `→` does move it once the picker AND its compose are gone — so this
    /// cannot pass on a caret with nowhere to go.
    #[test]
    fn arrows_in_the_model_picker_never_move_the_boards_search_caret() {
        let mut app = App::new(
            vec![session("sbl-row")],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        let mut store = store_at(Path::new("/tmp"));
        type_into_board(&mut app, "lab");
        feed(&mut app, key(KeyCode::Left), &mut store);
        assert_eq!(app.query_caret(), 2, "premise: the caret sits mid-query");

        compose::open_background(&mut app, None);
        feed(&mut app, ctrl(KeyCode::Char('l')), &mut store);
        feed(&mut app, key(KeyCode::Down), &mut store);
        for code in [KeyCode::Right, KeyCode::Right, KeyCode::Left] {
            feed(&mut app, key(code), &mut store);
            assert_eq!(
                app.query_caret(),
                2,
                "{code:?} in the picker must not move the board's caret underneath it"
            );
        }
        assert!(app.modal.is_some(), "the picker still owns the keyboard");

        // Control: with the picker AND the compose closed the very same key DOES
        // move it.
        feed(&mut app, key(KeyCode::Esc), &mut store);
        feed(&mut app, key(KeyCode::Esc), &mut store);
        assert!(!app.is_composing(), "both overlays are gone");
        feed(&mut app, key(KeyCode::Right), &mut store);
        assert_eq!(
            app.query_caret(),
            3,
            "the fixture can move: → on the board steps the caret"
        );
        assert_eq!(
            app.query(),
            "lab",
            "and nothing in the round trip edited it"
        );
    }

    /// `Enter`, `Ctrl-F` and Attach NEVER carry `--model` or `--effort`, through the
    /// real key routing — even straight after a pick was confirmed in a compose. A
    /// resume and a fork keep the session's own model (claude normally restores it),
    /// and an attach joins a running process: the argvs are byte-identical to what they
    /// were before model picks existed.
    #[test]
    fn enter_fork_and_attach_never_carry_a_model_even_after_a_compose_pick() {
        let dir = unique_temp_dir("no-model-handoffs");
        let mut app = App::new(
            vec![resumable_session(&dir, "sbm-plain")],
            Scope::All,
            PathBuf::from("/tmp"),
        );
        seed_live(&mut app, &[]);
        let carries_a_model =
            |argv: &[String]| argv.iter().any(|arg| arg == "--model" || arg == "--effort");

        // A reply compose with a CONFIRMED pick, then abandoned.
        press_ctrl(&mut app, KeyCode::Char('r'));
        press_ctrl(&mut app, KeyCode::Char('l'));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Enter);
        assert!(
            app.compose.as_ref().is_some_and(|c| c.model.is_some()),
            "fixture: the compose holds a pick"
        );
        press(&mut app, KeyCode::Esc);
        assert!(!app.is_composing());

        let Outcome::Resume(resumed) = press(&mut app, KeyCode::Enter) else {
            panic!(
                "Enter on a resumable session must hand off: {:?}",
                app.status
            );
        };
        assert_eq!(resumed.argv.join(" "), "claude -r sbm-plain");
        assert!(!carries_a_model(&resumed.argv));
        assert_eq!(resumed.nonzero_hint, crate::resume::RESUME_NONZERO_HINT);

        let Outcome::Resume(forked) = press_ctrl(&mut app, KeyCode::Char('f')) else {
            panic!(
                "Ctrl-F on a resumable session must hand off: {:?}",
                app.status
            );
        };
        assert_eq!(forked.argv.join(" "), "claude -r sbm-plain --fork-session");
        assert!(!carries_a_model(&forked.argv));

        // Attach: claude reports the session running as a background job.
        seed_live_agents(&mut app, &[("sbm-plain", "background", Some("job-1"))]);
        app.open_live_choice("sbm-plain".to_string());
        let Outcome::Resume(attached) = press(&mut app, KeyCode::Enter) else {
            panic!(
                "Attach on a live background job must hand off: {:?}",
                app.status
            );
        };
        assert_eq!(attached.argv.join(" "), "claude attach job-1");
        assert!(!carries_a_model(&attached.argv));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Task 4.3 / 4.4: `Ctrl-X d` opens a confirm defaulting to Cancel; confirming
    /// Delete on a NON-live session unlinks the transcript, reloads the board from
    /// the real store, and clamps the selection to the surviving row.
    #[test]
    fn ctrl_x_d_then_confirm_deletes_a_non_live_session_and_reloads() {
        let _guard = crate::config::env_lock();
        let root = unique_temp_dir("delete-store");
        let state = unique_temp_dir("delete-state");
        std::env::set_var("CLAUDE_PROJECTS_DIR", &root);
        std::env::set_var("SNAPBACK_CONFIG_DIR", &state);

        // A real 2-session store; sbdel-del is NEWER so it is selected first.
        let proj = root.join("-tmp-proj");
        std::fs::create_dir_all(&proj).expect("create the encoded-cwd dir");
        write_store_session(&proj, "sbdel-del", "2026-07-14T10:00:00.000Z");
        write_store_session(&proj, "sbdel-keep", "2026-07-10T10:00:00.000Z");

        let mut store = store_at(&root);

        let mut app = App::new(
            store.reload().sessions,
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        seed_live(&mut app, &[]); // nothing is live
        assert_eq!(
            app.selected.as_deref(),
            Some("sbdel-del"),
            "the newer session is selected first"
        );
        let del_file = proj.join("sbdel-del.jsonl");
        assert!(del_file.is_file(), "the fixture exists before the delete");

        // Ctrl-X d opens the confirm (default Cancel); move to Delete, then Enter.
        feed(&mut app, ctrl(KeyCode::Char('x')), &mut store);
        feed(&mut app, key(KeyCode::Char('d')), &mut store);
        assert_eq!(
            app.modal.as_ref().unwrap().selected_action(),
            Some(&ModalAction::Cancel),
            "the delete confirm defaults to Cancel for safety"
        );
        feed(&mut app, key(KeyCode::Left), &mut store); // Row layout: Cancel -> Delete
        assert_eq!(
            app.modal.as_ref().unwrap().selected_action(),
            Some(&ModalAction::Delete)
        );
        let out = feed(&mut app, key(KeyCode::Enter), &mut store);
        assert!(matches!(out, Outcome::Continue));

        assert!(!del_file.exists(), "the transcript file is unlinked");
        assert!(
            proj.join("sbdel-keep.jsonl").is_file(),
            "the other session's file is untouched"
        );
        assert!(
            app.session_by_id("sbdel-del").is_none(),
            "the deleted session left the reloaded board"
        );
        assert_eq!(
            app.selected.as_deref(),
            Some("sbdel-keep"),
            "the selection clamps to the surviving row after the reload"
        );
        assert!(app.modal.is_none(), "the confirm closed");

        std::env::remove_var("CLAUDE_PROJECTS_DIR");
        std::env::remove_var("SNAPBACK_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&state);
    }

    /// A quick reply STILL IN FLIGHT blocks the hard delete, even though claude
    /// reports the session as nothing at all.
    ///
    /// The end-to-end shape of the two features' race: `send::run_send` `claude
    /// stop`s the held job before running `claude -p -r <id>`, so for the whole
    /// span of the send the target is ABSENT from claude's active list — the probe
    /// this confirm spends is empty and `can_delete` alone would say "nothing is
    /// holding the file open" — while snapback's own child appends to that exact
    /// transcript. Nothing blocks keys during a send, so this window is reachable.
    ///
    /// Seeding an EMPTY live map is therefore the whole point: it proves the
    /// refusal comes from snapback's own state and not from claude's list.
    #[test]
    fn ctrl_x_d_confirm_while_a_reply_is_in_flight_is_refused_and_removes_nothing() {
        let _guard = crate::config::env_lock();
        let root = unique_temp_dir("delete-sending-store");
        let state = unique_temp_dir("delete-sending-state");
        std::env::set_var("CLAUDE_PROJECTS_DIR", &root);
        std::env::set_var("SNAPBACK_CONFIG_DIR", &state);

        let proj = root.join("-tmp-proj");
        std::fs::create_dir_all(&proj).expect("create the encoded-cwd dir");
        write_store_session(&proj, "sbsend-1", "2026-07-14T10:00:00.000Z");

        let mut store = store_at(&root);

        let mut app = App::new(
            store.reload().sessions,
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        // Claude reports NOTHING: the send already deregistered the job.
        seed_live(&mut app, &[]);
        // ...but snapback still has the reply in flight to this very id.
        app.sending = vec![crate::tui::app::Sending {
            session_id: "sbsend-1".to_string(),
            message: "still landing".to_string(),
            baseline_msg_count: 0,
        }];
        assert_eq!(app.selected.as_deref(), Some("sbsend-1"));
        let file = proj.join("sbsend-1.jsonl");

        feed(&mut app, ctrl(KeyCode::Char('x')), &mut store);
        feed(&mut app, key(KeyCode::Char('d')), &mut store);
        feed(&mut app, key(KeyCode::Left), &mut store); // -> Delete this
        let out = feed(&mut app, key(KeyCode::Enter), &mut store);
        assert!(matches!(out, Outcome::Continue));

        assert!(
            file.is_file(),
            "a transcript with a reply still landing is NOT unlinked"
        );
        assert!(
            app.session_by_id("sbsend-1").is_some(),
            "the session stays on the board"
        );
        assert_eq!(
            app.status.as_deref(),
            Some(crate::delete::DELETE_SENDING_REFUSAL),
            "the send refusal names snapback's own writer, not a claude window"
        );

        std::env::remove_var("CLAUDE_PROJECTS_DIR");
        std::env::remove_var("SNAPBACK_CONFIG_DIR");
    }

    /// Task 4.3 / 4.4: confirming Delete on a session claude holds open
    /// INTERACTIVELY is REFUSED — the writer guard sets a board status and nothing
    /// is unlinked or reloaded.
    ///
    /// `seed_live` reports its ids as interactive sessions, which is the arm that
    /// must stay refused: a claude window someone is typing in appends to this
    /// very file on the next keystroke.
    #[test]
    fn ctrl_x_d_confirm_on_an_open_interactive_session_is_refused_and_removes_nothing() {
        let _guard = crate::config::env_lock();
        let root = unique_temp_dir("delete-live-store");
        let state = unique_temp_dir("delete-live-state");
        std::env::set_var("CLAUDE_PROJECTS_DIR", &root);
        std::env::set_var("SNAPBACK_CONFIG_DIR", &state);

        let proj = root.join("-tmp-proj");
        std::fs::create_dir_all(&proj).expect("create the encoded-cwd dir");
        write_store_session(&proj, "sblive-1", "2026-07-14T10:00:00.000Z");

        let mut store = store_at(&root);

        let mut app = App::new(
            store.reload().sessions,
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        seed_live(&mut app, &["sblive-1"]); // claude holds it open interactively
        assert_eq!(app.selected.as_deref(), Some("sblive-1"));
        let file = proj.join("sblive-1.jsonl");

        feed(&mut app, ctrl(KeyCode::Char('x')), &mut store);
        feed(&mut app, key(KeyCode::Char('d')), &mut store);
        feed(&mut app, key(KeyCode::Left), &mut store); // -> Delete this
        let out = feed(&mut app, key(KeyCode::Enter), &mut store);
        assert!(matches!(out, Outcome::Continue));

        assert!(
            file.is_file(),
            "an open interactive session's transcript is NOT unlinked"
        );
        assert!(
            app.session_by_id("sblive-1").is_some(),
            "the session stays on the board"
        );
        assert_eq!(
            app.status.as_deref(),
            Some(crate::delete::DELETE_INTERACTIVE_REFUSAL),
            "the interactive refusal is shown verbatim for a single target"
        );
        assert!(
            app.modal.is_none(),
            "the confirm closes even when the delete is refused"
        );

        std::env::remove_var("CLAUDE_PROJECTS_DIR");
        std::env::remove_var("SNAPBACK_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&state);
    }

    /// The behavior change users actually feel: a PARKED background agent — one
    /// claude reports as active but has stopped, waiting on the user — is now
    /// deletable straight through the `Ctrl-X d` key path.
    ///
    /// This is the majority shape of claude's active list, and the old membership
    /// guard refused every one of it. Nothing is writing such a transcript (claude
    /// re-opens the path to append), so the delete goes through.
    #[test]
    fn ctrl_x_d_confirm_deletes_a_parked_background_agent() {
        let _guard = crate::config::env_lock();
        let root = unique_temp_dir("delete-parked-store");
        let state = unique_temp_dir("delete-parked-state");
        std::env::set_var("CLAUDE_PROJECTS_DIR", &root);
        std::env::set_var("SNAPBACK_CONFIG_DIR", &state);

        let proj = root.join("-tmp-proj");
        std::fs::create_dir_all(&proj).expect("create the encoded-cwd dir");
        write_store_session(&proj, "sbparked-1", "2026-07-14T10:00:00.000Z");

        let mut store = store_at(&root);

        let mut app = App::new(
            store.reload().sessions,
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        // claude REPORTS it as an active agent — it is simply parked on `blocked`.
        seed_live_records(&mut app, &[("sbparked-1", "background", Some("blocked"))]);
        assert_eq!(app.selected.as_deref(), Some("sbparked-1"));
        let file = proj.join("sbparked-1.jsonl");
        assert!(file.is_file(), "the fixture exists before the delete");

        feed(&mut app, ctrl(KeyCode::Char('x')), &mut store);
        feed(&mut app, key(KeyCode::Char('d')), &mut store);
        feed(&mut app, key(KeyCode::Left), &mut store); // -> Delete this
        let out = feed(&mut app, key(KeyCode::Enter), &mut store);
        assert!(matches!(out, Outcome::Continue));

        assert!(
            !file.exists(),
            "a parked background agent's transcript IS deletable — claude reporting \
             it says nothing about a writer"
        );
        assert!(
            app.session_by_id("sbparked-1").is_none(),
            "the deleted session left the reloaded board"
        );
        assert_eq!(
            app.status, None,
            "a clean single delete says nothing; the row leaving the board is the message"
        );

        std::env::remove_var("CLAUDE_PROJECTS_DIR");
        std::env::remove_var("SNAPBACK_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&state);
    }

    /// The lineage choice takes the WHOLE fork family: every member's transcript
    /// AND its sibling `<id>/` dir goes, and an unrelated session is untouched.
    ///
    /// This is the asymmetry the choice closes. Hide already flips a lineage as
    /// one unit, so deleting only the folded HEAD left the members behind and the
    /// fold just re-headed to a surviving fork — the row never left the board.
    #[test]
    fn ctrl_x_d_delete_lineage_removes_every_member_and_its_sibling_dir() {
        let _guard = crate::config::env_lock();
        let root = unique_temp_dir("delete-lineage-store");
        let state = unique_temp_dir("delete-lineage-state");
        std::env::set_var("CLAUDE_PROJECTS_DIR", &root);
        std::env::set_var("SNAPBACK_CONFIG_DIR", &state);

        // Two members of ONE lineage (same root uuid, cwd and branch) plus an
        // unrelated session that must survive. The newer member is the head.
        let proj = root.join("-tmp-proj");
        std::fs::create_dir_all(&proj).expect("create the encoded-cwd dir");
        write_lineage_session(
            &proj,
            "sblin-head",
            "2026-07-14T10:00:00.000Z",
            "root-uuid-1",
        );
        write_lineage_session(
            &proj,
            "sblin-old",
            "2026-07-12T10:00:00.000Z",
            "root-uuid-1",
        );
        write_store_session(&proj, "sblin-other", "2026-07-10T10:00:00.000Z");
        // The older member carries subagent transcripts, so the sibling dir has
        // something to prove it went with the file.
        let old_subagents = proj.join("sblin-old").join("subagents");
        std::fs::create_dir_all(&old_subagents).expect("create the subagents dir");
        std::fs::write(old_subagents.join("agent-1.jsonl"), "{}\n").expect("write a subagent");

        let mut store = store_at(&root);

        let mut app = App::new(
            store.reload().sessions,
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        seed_live_records(&mut app, &[]); // nothing is live
        assert_eq!(
            app.selected.as_deref(),
            Some("sblin-head"),
            "the newest lineage member heads the folded row"
        );

        feed(&mut app, ctrl(KeyCode::Char('x')), &mut store);
        feed(&mut app, key(KeyCode::Char('d')), &mut store);
        let choices = &app.modal.as_ref().expect("the confirm is open").choices;
        assert_eq!(
            choices.len(),
            3,
            "a real lineage offers [Delete this] [Delete lineage (N)] [Cancel]"
        );
        assert_eq!(
            choices[1].label, "Delete lineage (2)",
            "the button states the REAL member count"
        );
        assert_eq!(
            app.modal.as_ref().unwrap().selected_action(),
            Some(&ModalAction::Cancel),
            "the confirm still defaults to Cancel with the extra button in the strip"
        );

        feed(&mut app, key(KeyCode::Left), &mut store); // Cancel -> Delete lineage
        assert!(
            matches!(
                app.modal.as_ref().unwrap().selected_action(),
                Some(&ModalAction::DeleteLineage(_))
            ),
            "the middle button is the lineage delete"
        );
        let out = feed(&mut app, key(KeyCode::Enter), &mut store);
        assert!(matches!(out, Outcome::Continue));

        assert!(
            !proj.join("sblin-head.jsonl").exists(),
            "the head's transcript is gone"
        );
        assert!(
            !proj.join("sblin-old.jsonl").exists(),
            "the FOLDED member's transcript is gone too — that is the whole point"
        );
        assert!(
            !proj.join("sblin-old").exists(),
            "each member's sibling <id>/ dir goes with it"
        );
        assert!(
            proj.join("sblin-other.jsonl").is_file(),
            "an unrelated session is untouched"
        );
        assert!(
            app.session_by_id("sblin-head").is_none() && app.session_by_id("sblin-old").is_none(),
            "the whole lineage left the reloaded board"
        );
        assert_eq!(
            app.status.as_deref(),
            Some("2 deleted"),
            "a lineage reports what it did"
        );

        std::env::remove_var("CLAUDE_PROJECTS_DIR");
        std::env::remove_var("SNAPBACK_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&state);
    }

    /// A MIXED lineage is PARTIAL, not all-or-nothing: the members that pass the
    /// writer guard are deleted, the running one is skipped, and the status reports
    /// the split.
    ///
    /// All-or-nothing would let one busy fork block its whole family — the exact
    /// dead end the lineage choice exists to remove.
    #[test]
    fn ctrl_x_d_delete_lineage_skips_a_running_member_and_reports_the_split() {
        let _guard = crate::config::env_lock();
        let root = unique_temp_dir("delete-mixed-store");
        let state = unique_temp_dir("delete-mixed-state");
        std::env::set_var("CLAUDE_PROJECTS_DIR", &root);
        std::env::set_var("SNAPBACK_CONFIG_DIR", &state);

        let proj = root.join("-tmp-proj");
        std::fs::create_dir_all(&proj).expect("create the encoded-cwd dir");
        write_lineage_session(
            &proj,
            "sbmix-head",
            "2026-07-14T10:00:00.000Z",
            "root-uuid-2",
        );
        write_lineage_session(
            &proj,
            "sbmix-busy",
            "2026-07-13T10:00:00.000Z",
            "root-uuid-2",
        );
        write_lineage_session(
            &proj,
            "sbmix-old",
            "2026-07-12T10:00:00.000Z",
            "root-uuid-2",
        );

        let mut store = store_at(&root);

        let mut app = App::new(
            store.reload().sessions,
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        // ONE member is genuinely working a turn; the others are not reported.
        seed_live_records(&mut app, &[("sbmix-busy", "background", Some("working"))]);
        assert_eq!(app.selected.as_deref(), Some("sbmix-head"));

        feed(&mut app, ctrl(KeyCode::Char('x')), &mut store);
        feed(&mut app, key(KeyCode::Char('d')), &mut store);
        feed(&mut app, key(KeyCode::Left), &mut store); // -> Delete lineage (3)
        let out = feed(&mut app, key(KeyCode::Enter), &mut store);
        assert!(matches!(out, Outcome::Continue));

        assert!(
            proj.join("sbmix-busy.jsonl").is_file(),
            "the working member is skipped, not unlinked"
        );
        assert!(
            !proj.join("sbmix-head.jsonl").exists() && !proj.join("sbmix-old.jsonl").exists(),
            "one busy fork must not block the rest of the lineage"
        );
        assert_eq!(
            app.status.as_deref(),
            Some("2 deleted, 1 skipped (running)"),
            "the split is reported honestly, with the skip counted as a refusal"
        );
        assert!(
            app.session_by_id("sbmix-busy").is_some(),
            "the surviving member is still on the reloaded board"
        );

        std::env::remove_var("CLAUDE_PROJECTS_DIR");
        std::env::remove_var("SNAPBACK_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&state);
    }

    /// A lineage member that LEFT THE BOARD while the confirm sat open is
    /// COUNTED, not silently dropped.
    ///
    /// The member ids ride the `DeleteLineage` choice from the moment the modal
    /// OPENED, so a `SessionsChanged` reload can drop one out from under them —
    /// simulated here exactly as it happens, by the transcript disappearing from
    /// the store and the board reloading. That target is neither unlinked nor
    /// refused, so before the reconciliation the board reported `2 deleted` for a
    /// family of THREE and the third id was mentioned nowhere at all.
    #[test]
    fn ctrl_x_d_delete_lineage_counts_a_member_that_left_the_board() {
        let _guard = crate::config::env_lock();
        let root = unique_temp_dir("delete-gone-store");
        let state = unique_temp_dir("delete-gone-state");
        std::env::set_var("CLAUDE_PROJECTS_DIR", &root);
        std::env::set_var("SNAPBACK_CONFIG_DIR", &state);

        // THREE members of one lineage: two survive to the confirm, one does not,
        // so "2 deleted" and "3 targets" are distinguishable rather than equal.
        let proj = root.join("-tmp-proj");
        std::fs::create_dir_all(&proj).expect("create the encoded-cwd dir");
        for (id, ts) in [
            ("sbgone-head", "2026-07-14T10:00:00.000Z"),
            ("sbgone-mid", "2026-07-13T10:00:00.000Z"),
            ("sbgone-away", "2026-07-12T10:00:00.000Z"),
        ] {
            write_lineage_session(&proj, id, ts, "root-uuid-5");
        }

        let mut store = store_at(&root);

        let mut app = App::new(
            store.reload().sessions,
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        seed_live_records(&mut app, &[]); // nothing is live

        // Opening the confirm CAPTURES all three member ids.
        feed(&mut app, ctrl(KeyCode::Char('x')), &mut store);
        feed(&mut app, key(KeyCode::Char('d')), &mut store);
        assert_eq!(
            app.modal.as_ref().expect("the confirm is open").choices[1].label,
            "Delete lineage (3)",
            "all three members are targeted when the modal opens"
        );

        // ...and now one of them leaves the board while the modal sits open.
        std::fs::remove_file(proj.join("sbgone-away.jsonl")).expect("drop a member from the store");
        handle_event(&mut app, AppEvent::SessionsChanged, &mut store);
        assert!(
            app.session_by_id("sbgone-away").is_none(),
            "the reload dropped that member from the board"
        );
        assert!(
            app.modal.is_some(),
            "the reload leaves the confirm standing, still holding the stale ids"
        );

        feed(&mut app, key(KeyCode::Left), &mut store); // -> Delete lineage (3)
        let out = feed(&mut app, key(KeyCode::Enter), &mut store);
        assert!(matches!(out, Outcome::Continue));

        assert!(
            !proj.join("sbgone-head.jsonl").exists() && !proj.join("sbgone-mid.jsonl").exists(),
            "the two members still on the board are deleted"
        );
        assert_eq!(
            app.status.as_deref(),
            Some("2 deleted, 1 already gone"),
            "all THREE targets are accounted for — the vanished one is reported, \
             not swallowed by a tally that only counts what the pass touched"
        );

        std::env::remove_var("CLAUDE_PROJECTS_DIR");
        std::env::remove_var("SNAPBACK_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&state);
    }

    /// PROBE BUDGET: a lineage delete shells out to claude EXACTLY ONCE, however
    /// many members it has.
    ///
    /// Nothing else can see this. Judging each member through the per-session
    /// accessor would still delete the right files and still report the right
    /// split, while spawning `claude` once per member — N blocking shell-outs on
    /// the render loop (AGENTS.md OFF-UI-THREAD). Counting the probe is the only
    /// assertion that goes red for it.
    #[test]
    fn a_lineage_delete_probes_claude_exactly_once() {
        let _guard = crate::config::env_lock();
        let root = unique_temp_dir("delete-probe-store");
        let state = unique_temp_dir("delete-probe-state");
        std::env::set_var("CLAUDE_PROJECTS_DIR", &root);
        std::env::set_var("SNAPBACK_CONFIG_DIR", &state);

        // THREE members, so a per-member probe counts 3 and a single probe counts
        // 1 — the two are distinguishable rather than coincidentally equal.
        let proj = root.join("-tmp-proj");
        std::fs::create_dir_all(&proj).expect("create the encoded-cwd dir");
        write_lineage_session(
            &proj,
            "sbprobe-a",
            "2026-07-14T10:00:00.000Z",
            "root-uuid-3",
        );
        write_lineage_session(
            &proj,
            "sbprobe-b",
            "2026-07-13T10:00:00.000Z",
            "root-uuid-3",
        );
        write_lineage_session(
            &proj,
            "sbprobe-c",
            "2026-07-12T10:00:00.000Z",
            "root-uuid-3",
        );

        let mut store = store_at(&root);

        let mut app = App::new(
            store.reload().sessions,
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        let probes = seed_live_records(&mut app, &[]);

        feed(&mut app, ctrl(KeyCode::Char('x')), &mut store);
        feed(&mut app, key(KeyCode::Char('d')), &mut store);
        assert_eq!(
            probes.get(),
            0,
            "OPENING the confirm asks claude nothing — the probe belongs to the confirm"
        );

        feed(&mut app, key(KeyCode::Left), &mut store); // -> Delete lineage (3)
        feed(&mut app, key(KeyCode::Enter), &mut store);

        assert_eq!(
            probes.get(),
            1,
            "three members, ONE shell-out: every member is judged against the same \
             freshly-probed map"
        );
        assert!(
            !proj.join("sbprobe-a.jsonl").exists()
                && !proj.join("sbprobe-b.jsonl").exists()
                && !proj.join("sbprobe-c.jsonl").exists(),
            "the count above must be over a delete that actually happened"
        );

        std::env::remove_var("CLAUDE_PROJECTS_DIR");
        std::env::remove_var("SNAPBACK_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&state);
    }

    /// A LONE session offers no lineage button: the strip stays
    /// `[Delete this] [Cancel]`, so nothing suggests a family that does not exist.
    #[test]
    fn ctrl_x_d_offers_no_lineage_choice_for_a_lone_session() {
        let _guard = crate::config::env_lock();
        let root = unique_temp_dir("delete-lone-store");
        let state = unique_temp_dir("delete-lone-state");
        std::env::set_var("CLAUDE_PROJECTS_DIR", &root);
        std::env::set_var("SNAPBACK_CONFIG_DIR", &state);

        let proj = root.join("-tmp-proj");
        std::fs::create_dir_all(&proj).expect("create the encoded-cwd dir");
        // A rootless session (no lineage at all) AND a session that HAS a root but
        // no twin: both are families of one, and neither may offer the button.
        write_store_session(&proj, "sblone-rootless", "2026-07-14T10:00:00.000Z");
        write_lineage_session(
            &proj,
            "sblone-solo",
            "2026-07-12T10:00:00.000Z",
            "root-uuid-4",
        );

        let mut store = store_at(&root);

        let mut app = App::new(
            store.reload().sessions,
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        seed_live_records(&mut app, &[]);

        // Row 0 is the newer rootless session; one step down is the solo lineage.
        for (step, id) in [(0isize, "sblone-rootless"), (1, "sblone-solo")] {
            app.move_selection(step);
            assert_eq!(app.selected.as_deref(), Some(id), "standing on {id}");
            feed(&mut app, ctrl(KeyCode::Char('x')), &mut store);
            feed(&mut app, key(KeyCode::Char('d')), &mut store);
            let modal = app.modal.as_ref().expect("the confirm is open");
            assert_eq!(
                modal
                    .choices
                    .iter()
                    .map(|c| c.action.clone())
                    .collect::<Vec<_>>(),
                vec![ModalAction::Delete, ModalAction::Cancel],
                "{id}: a family of one offers no lineage button"
            );
            assert_eq!(
                modal.selected_action(),
                Some(&ModalAction::Cancel),
                "{id}: still defaulted to Cancel"
            );
            feed(&mut app, key(KeyCode::Esc), &mut store);
        }

        std::env::remove_var("CLAUDE_PROJECTS_DIR");
        std::env::remove_var("SNAPBACK_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&state);
    }

    /// Task 4.4: cancelling the chord (`Ctrl-X` then `Esc`) opens no modal and
    /// leaves BOTH the store and the persisted hidden set untouched.
    #[test]
    fn ctrl_x_then_esc_cancels_the_chord_leaving_the_store_and_hidden_set_untouched() {
        let _guard = crate::config::env_lock();
        let root = unique_temp_dir("cancel-store");
        let state = unique_temp_dir("cancel-state");
        std::env::set_var("CLAUDE_PROJECTS_DIR", &root);
        std::env::set_var("SNAPBACK_CONFIG_DIR", &state);

        let proj = root.join("-tmp-proj");
        std::fs::create_dir_all(&proj).expect("create the encoded-cwd dir");
        write_store_session(&proj, "sbcancel-1", "2026-07-14T10:00:00.000Z");

        let mut store = store_at(&root);

        let mut app = App::new(
            store.reload().sessions,
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        assert_eq!(app.selected.as_deref(), Some("sbcancel-1"));

        feed(&mut app, ctrl(KeyCode::Char('x')), &mut store);
        assert!(app.pending_chord, "Ctrl-X arms the chord");
        let out = feed(&mut app, key(KeyCode::Esc), &mut store);
        assert!(matches!(out, Outcome::Continue));

        assert!(!app.pending_chord, "Esc abandons the chord");
        assert!(app.modal.is_none(), "cancel opens no modal");
        assert!(app.hidden_ids.is_empty(), "cancel hides nothing");
        assert!(
            proj.join("sbcancel-1.jsonl").is_file(),
            "cancel removes nothing from the store"
        );
        assert!(
            crate::hidden::load_hidden(&crate::config::state_dir()).is_empty(),
            "cancel persists no hide"
        );

        std::env::remove_var("CLAUDE_PROJECTS_DIR");
        std::env::remove_var("SNAPBACK_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&state);
    }
}
