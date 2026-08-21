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
//! Because such a result can arrive after the board it belongs to is gone,
//! [`handle_event`] closes the compose surface on any [`Outcome`] that
//! [ends the board session](Outcome::ends_board_session) as well.
//!
//! This module is the *decision* half of the loop: [`key_to_action`] maps a key
//! to an [`Action`], and [`handle_event`] applies an [`AppEvent`] to the [`App`]
//! and returns an [`Outcome`] telling the driver (in [`crate::tui`]) whether to
//! continue, quit, hand off a resume, or fire one of the three no-teardown
//! children (`Send` / `Interrupt` / `BgLaunch`). All of it is terminal-free and
//! unit tested; the terminal-driving loop that calls it lives in
//! [`crate::tui::run`].
//!
//! ## Keybindings
//!
//! | Key | Action |
//! | --- | ------ |
//! | `Up` / `Down` | move selection (always) |
//! | `j` / `k` | move selection (only while the query is empty; otherwise typed) |
//! | `Left` / `Right` | fold / expand the selected row's fork lineage (always) |
//! | `Enter` | resume the selected session |
//! | `Ctrl-F` | fork-resume the selected session |
//! | `Ctrl-N` | start a new session in the launch directory. When agents are defined a picker opens first and `Enter` on a pick opens a draft pane for the session's first message; with none defined that draft opens straight away. In the draft, `Enter` starts a BACKGROUND agent without leaving the board, `Ctrl-O` runs it interactively instead, `Esc` cancels |
//! | `Ctrl-O` (in the agent picker) | start the highlighted agent INTERACTIVELY at once, skipping the draft — the same verb `Ctrl-O` names inside the draft, so BOTH routes out of the picker cost exactly one key. Bound on the picker alone — inert on every other modal |
//! | `Ctrl-R` | quick-reply: send a one-shot message to the selected session without leaving the board. An agent whose run is OVER (`done` / `stopped` / `failed`) is stopped first so the reply lands in place; `needs input` confirms first; `working` / `idle` / `interrupted` / an unrecognized qualifier is refused (see [`send::reply_gate`]) |
//! | `Ctrl-K` | stop / interrupt the selected session's live background agent (`claude stop`); an agent whose run is OVER (`done` / `stopped` / `failed`) stops at once, every other live agent confirms first, and a session claude is not holding — or one running interactively, which carries no job id — is refused (see [`send::interrupt_gate`]) |
//! | `Tab` | toggle name-only vs. name+content search. Widening to content also opens the preview on the most recent match, exactly as typing does: it goes through the same query funnel, and the mode is the gate that key just opened |
//! | `Ctrl-A` | flip the scope: current folder <-> project (the launch repo and all of its git worktrees). ONE key for both, because the second is a refinement of the same question the first answers, not a separate mode. Launched with `--all`/`-a` it becomes a three-stop cycle through all folders as well — the whole store is on this key only when the launch flag put it there |
//! | `Ctrl-X` then `x`/`d`/`h`/`r` | leader chord: hide / hard-delete (this row, or its whole fork lineage) / toggle show-hidden / re-read every transcript from disk (any other key cancels) |
//! | `Ctrl-/` | toggle the preview pane |
//! | `PgUp` / `PgDn` | scroll the preview a page (always) |
//! | `Ctrl-U` / `Ctrl-D` | scroll the preview a quarter page (always) |
//! | `Home` / `End` | jump the preview to top / bottom (always) |
//! | `Shift-Up` / `Shift-Down` | scroll the preview onto the previous / next MARKED line, but only while the query marks something in the previewed transcript; with nothing marked they fall through to plain selection movement. One stop per marked LINE, not per occurrence — a line saying the query twice is marked, and stopped at, once |
//! | `Backspace` | delete the last query character |
//! | printable char | type-to-search (append to the query) |
//! | terminal paste | inserted as TEXT — never as keystrokes (see below) |
//! | `q` | quit (only while the query is empty; otherwise typed) |
//! | `Esc` / `Ctrl-C` | quit (always) |
//!
//! `j`/`k`/`q` are disambiguated by whether the query is empty: in the default
//! browse state they navigate/quit; once you are typing a query they become
//! ordinary search input. Arrows, `Enter`, `Tab`, and every `Ctrl-` binding
//! work regardless of the query, so search is never blocked. The SHIFTED arrows
//! are disambiguated the same way, by whether there is anything marked to move
//! between — and fall through to the unshifted binding when there is not, so a
//! terminal that drops the modifier still moves the selection.
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
//! it at the caret, and the board appends it to the query with newlines flattened
//! to spaces. A paste can never submit, resume, or quit.

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};

use crate::defined_agents;
use crate::delete;
use crate::resume::{self, Ready};
use crate::send::{self, BgLaunchRequest, InterruptGate, InterruptRequest, ReplyGate, SendRequest};
use crate::store::SessionStore;
use crate::watch::AppEvent;

use super::app::{App, Interrupting, ModalAction, ModalLayout};
use super::{compose, view};

/// A decoded intent from a single keypress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Quit the app.
    Quit,
    /// Move the selection up one row.
    MoveUp,
    /// Move the selection down one row.
    MoveDown,
    /// Collapse the selected row's fork lineage back to its single head (`←`).
    CollapseLineage,
    /// Expand the selected row's fork lineage, showing the members its head
    /// stands for (`→`).
    ExpandLineage,
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
    Reply,
    /// Stop / interrupt the selected session's live background agent (`Ctrl-K`).
    /// [`apply_action`] runs the interrupt gate ([`send::interrupt_gate`]): an
    /// agent whose run is over is stopped immediately, every other live agent
    /// confirms first, and a non-live or interactive session is refused with a
    /// hint.
    Interrupt,
    /// Toggle name-only vs. name+content search.
    ToggleSearchMode,
    /// Flip the scope: current folder <-> project (`Ctrl-A`), or cycle it
    /// through all folders as well when the board was launched with
    /// `--all`/`-a`. The project state spans the launch repo's git worktrees;
    /// see [`super::app::Scope::toggled`] for the cycle itself and for why the
    /// widest state is off the key by default.
    ToggleScope,
    /// Toggle the preview pane.
    TogglePreview,
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
    /// Append a character to the query (type-to-search).
    Insert(char),
    /// Delete the last query character.
    Backspace,
    /// Enter the `Ctrl-X` leader chord: arm [`App::pending_chord`] so the NEXT key
    /// routes through the pure [`chord_key`] machine (hide / hard-delete /
    /// show-hidden / cancel) instead of the board.
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
}

impl Outcome {
    /// Whether this outcome ENDS the current board session — the terminal comes
    /// down and the merged event channel with it.
    ///
    /// True for [`Quit`](Self::Quit) and every [`Resume`](Self::Resume); false for
    /// the three no-teardown effects, which keep drawing on the SAME channel. Pure,
    /// so "does the board survive this?" is one greppable answer rather than a
    /// `matches!` repeated per call site.
    #[must_use]
    pub fn ends_board_session(&self) -> bool {
        matches!(self, Outcome::Quit | Outcome::Resume(_))
    }
}

/// Map a keypress to an [`Action`]. `query_empty` disambiguates the `j`/`k`/`q`
/// keys: they navigate/quit only in the default browse state and are otherwise
/// ordinary search input. `has_preview_matches` ([`App::has_preview_matches`])
/// decides whether the SHIFTED arrows have anywhere to go.
///
/// The shifted arrows are bound CONDITIONALLY and fall through to plain selection
/// movement otherwise, which buys two things at once. With no query — or a query
/// the previewed transcript does not say — `Shift-Up` is bit-for-bit the
/// `MoveUp` it has always been, so nothing a user already relies on changes. And
/// on a terminal that drops the modifier (or a multiplexer that eats the `CSI
/// 1;2A` form), the key arrives as a bare arrow and still moves the selection,
/// which is the graceful degradation rather than a dead key.
#[must_use]
pub fn key_to_action(key: KeyEvent, query_empty: bool, has_preview_matches: bool) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    if ctrl {
        return match key.code {
            KeyCode::Char('f') | KeyCode::Char('F') => Action::Resume { fork: true },
            // Ctrl-/ toggles the preview. Terminals that map Ctrl-/ to the
            // control code 0x1f surface it as Char('_'); accept both.
            KeyCode::Char('/') | KeyCode::Char('_') => Action::TogglePreview,
            KeyCode::Char('a') | KeyCode::Char('A') => Action::ToggleScope,
            KeyCode::Char('n') | KeyCode::Char('N') => Action::NewSession,
            KeyCode::Char('r') | KeyCode::Char('R') => Action::Reply,
            KeyCode::Char('k') | KeyCode::Char('K') => Action::Interrupt,
            KeyCode::Char('c') | KeyCode::Char('C') => Action::Quit,
            // Ctrl-X (0x18 CAN) is the board-trimming leader chord (hide /
            // hard-delete / show-hidden / forced rescan). Unbound and
            // terminal-safe — unlike Ctrl-H/I/M, which alias Backspace/Tab/Enter.
            // It only ARMS the chord; the follow-up key decides (see `chord_key`).
            KeyCode::Char('x') | KeyCode::Char('X') => Action::Chord,
            // Quarter-page preview scroll (readline-style). Acts regardless of
            // the query, like the arrows, so search never blocks preview scrolling.
            KeyCode::Char('u') | KeyCode::Char('U') => Action::PreviewHalfUp,
            KeyCode::Char('d') | KeyCode::Char('D') => Action::PreviewHalfDown,
            _ => Action::Ignore,
        };
    }

    match key.code {
        // Search-match navigation, on the SHIFTED arrows and only while there is
        // something marked to move between. It steps between marked LINES, not
        // between every occurrence: a line can carry several marked runs and is
        // still one stop, because a stop is a place to look and a line is what the
        // jump can scroll to. Deliberately NOT on Alt: snapback never pushes the kitty
        // keyboard protocol and actively clears it on every board (re)entry
        // (`tui::reset_terminal_state`), so Alt+arrow arrives on default macOS
        // terminals as a composed character that types junk into the query, and a
        // split ESC read surfaces as a bare `Esc` — which quits the board. `Shift`
        // needs none of that: it rides the ordinary `CSI 1;2A`/`B` encoding.
        KeyCode::Up if shift && !query_empty && has_preview_matches => Action::PreviewMatchPrev,
        KeyCode::Down if shift && !query_empty && has_preview_matches => Action::PreviewMatchNext,
        KeyCode::Up => Action::MoveUp,
        KeyCode::Down => Action::MoveDown,
        // Fork-lineage fold toggle, on the canonical tree idiom. Bound OUTSIDE
        // the `ctrl` block above on purpose: that namespace is already crowded,
        // and these keys are not printable, so — like the arrows and the preview
        // scroll keys — they act regardless of the query and can never be
        // swallowed by type-to-search the way a plain letter would.
        KeyCode::Left => Action::CollapseLineage,
        KeyCode::Right => Action::ExpandLineage,
        // Preview scroll: page + jump. Bound regardless of query state (they are
        // not printable, so they never collide with type-to-search).
        KeyCode::PageUp => Action::PreviewPageUp,
        KeyCode::PageDown => Action::PreviewPageDown,
        KeyCode::Home => Action::PreviewTop,
        KeyCode::End => Action::PreviewBottom,
        KeyCode::Enter => Action::Resume { fork: false },
        KeyCode::Esc => Action::Quit,
        KeyCode::Tab => Action::ToggleSearchMode,
        KeyCode::Backspace => Action::Backspace,
        KeyCode::Char(c) if alt => {
            let _ = c;
            Action::Ignore
        }
        KeyCode::Char('j') if query_empty => Action::MoveDown,
        KeyCode::Char('k') if query_empty => Action::MoveUp,
        KeyCode::Char('q') if query_empty => Action::Quit,
        KeyCode::Char(c) => Action::Insert(c),
        _ => Action::Ignore,
    }
}

/// Apply one merged [`AppEvent`] to the app, returning the driver's next step.
///
/// * `Input(Key)` (a press/repeat) -> decode + apply an [`Action`].
/// * `Input(Mouse)` -> a wheel notch scrolls the pane under the pointer, a
///   left-button press/drag/release on the list/preview seam resizes the split,
///   and a left-click on a rendered preview link opens its url in the default
///   browser ([`handle_mouse`]); all are independent of the overlay gate (a click
///   cannot start an orphaned drag or open a link while the overlay is open — see
///   [`App::begin_split_drag`] and the `App::overlay_active` gate).
/// * `SessionsChanged` -> reload `store` and re-apply query+scope, preserving
///   selection-by-id and scroll (see [`reload_board`]).
/// * `Tick` -> nothing costly (just a redraw upstream).
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
        // a placeholder and hold `overlay_active` true (killing link clicks and
        // splitter drags) until another compose was opened and cancelled.
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
            // While a modal overlay (the running-session choice, the new-session
            // agent picker, or the hard-delete confirm) is open it OWNS the
            // keyboard: keys navigate/confirm/cancel the modal, never the board.
            if app.modal.is_some() {
                return handle_modal_key(app, key, store);
            }
            // A pending `Ctrl-X` leader chord OWNS the next key too: route it through
            // the chord machine BEFORE normal handling so a printable follow-up
            // (`x`/`d`/`h`/`r`) completes the chord instead of leaking into the query.
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
            let action = key_to_action(key, app.query.is_empty(), app.has_preview_matches());
            apply_action(app, action)
        }
        // Mouse wheel scroll and splitter drag. A dedicated arm BEFORE the
        // input catch-all and INDEPENDENT of the modal overlay gate above:
        // neither routes into the modal handler — they just scroll a pane /
        // resize the split and never crash in any mode (query active, modal
        // open, ...). A stray click cannot start an orphaned drag while the
        // modal is open (`App::begin_split_drag` gates it).
        AppEvent::Input(Event::Mouse(mouse)) => {
            handle_mouse(app, mouse);
            Outcome::Continue
        }
        // A terminal PASTE (bracketed paste, enabled in `tui::init_terminal`). A
        // dedicated arm BEFORE the input catch-all that used to swallow it, and
        // routed by [`handle_paste`] through the SAME precedence the key arm above
        // uses — see that fn for what each keyboard owner does with one.
        AppEvent::Input(Event::Paste(text)) => {
            handle_paste(app, &text);
            Outcome::Continue
        }
        AppEvent::Input(_) => Outcome::Continue,
        AppEvent::SessionsChanged => {
            reload_board(app, store);
            Outcome::Continue
        }
        AppEvent::ReportedAgents(agents) => {
            // Delivered off-thread by the agents poller; just swap the map in.
            app.set_reported_agents(agents);
            Outcome::Continue
        }
        AppEvent::SendFinished {
            session_id,
            status,
            success,
        } => {
            // A one-shot quick-reply send completed off-thread. Clear the in-flight
            // indicator (if this is the send it was tracking) and surface the mapped
            // result (cost / error) on the status line. Successes are transient
            // confirmations; failures and refusals stay sticky.
            if app.sending_to(&session_id).is_some() {
                app.sending = None;
            }
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
            // `sending_to` guard above.
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
            // The identity check is the `sending_to` guard above, and it is
            // load-bearing for the same reason: the card outlives its editor, so a
            // result can land on a surface that is no longer this launch's — a
            // quick reply, or a second draft — and closing blindly would throw away
            // a buffer the user is still typing into.
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
        AppEvent::Tick => {
            // The tick already drove a redraw; counting it turns that existing
            // cadence into the board's clock, which `view::blink_visible` phases
            // the live-badge pulse from. `wrapping_add` so a board left running
            // for eons rolls over instead of overflow-panicking in debug.
            app.tick = app.tick.wrapping_add(1);
            // Age transient statuses (confirmations/nudges) on the same cadence.
            // Failures/refusals stay sticky until the next actionable keypress.
            app.tick_status();
            Outcome::Continue
        }
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
/// 6. **Board** — APPENDED to the search query, newlines flattened to spaces
///    ([`flatten_for_query`]), exactly as if typed.
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
}

/// Hit-test a wheel event at `(col, row)` against the pinned pane rects.
///
/// The preview wins when the point is inside `preview`; the list when inside
/// `list`; otherwise the preview is the default surface (it is the primary thing
/// you scroll). The hidden-preview case leaves `preview` EMPTY, so a point over
/// the now-full-width list still routes to the list. Pure so it is unit testable
/// from coordinates + rects without a terminal.
fn wheel_target(col: u16, row: u16, preview: Rect, list: Rect) -> WheelTarget {
    let pos = Position { x: col, y: row };
    if preview.contains(pos) {
        WheelTarget::Preview
    } else if list.contains(pos) {
        WheelTarget::List
    } else {
        WheelTarget::Preview
    }
}

/// Columns either side of the list/preview seam that still count as a
/// splitter grab, so the border is a comfortably wide target rather than a
/// single exact column.
const SPLITTER_TOLERANCE: u16 = 1;

/// Hit-test a mouse point at `(col, row)` against the seam between `list` and
/// `preview`. The seam sits at the list's right edge (`list.x + list.width`,
/// which — since `render_body` lays the two panes out with no gap — equals
/// `preview.x`); a point within [`SPLITTER_TOLERANCE`] columns of that seam
/// and vertically within the list's row range counts as a hit. A hidden
/// preview (the empty `Rect::default()` `render_body` sets when
/// `!show_preview`) never hits — there is no seam to grab. Pure so it is
/// unit-testable from coordinates + rects, exactly like [`wheel_target`].
fn on_splitter(col: u16, row: u16, list: Rect, preview: Rect) -> bool {
    if preview.is_empty() {
        return false;
    }
    let seam = list.x + list.width;
    let in_rows = row >= list.y && row < list.y.saturating_add(list.height);
    in_rows && col.abs_diff(seam) <= SPLITTER_TOLERANCE
}

/// Apply a mouse event: a vertical wheel notch scrolls whichever pane the
/// pointer is over; a left-button press on the list/preview seam begins
/// dragging the splitter, a left-button drag while dragging resizes it, and a
/// left-button release always ends the drag. A left-button press INSIDE the
/// preview pane (but not on the seam) that lands on a rendered link opens its url
/// in the default browser — fire-and-forget, off the render loop. Any other event
/// (other buttons, horizontal wheel, plain moves) is ignored. Never touches the
/// query, and the overlay gate is enforced by [`App::begin_split_drag`] (drag) and
/// the [`App::overlay_active`] gate (link open), so this never crashes or starts an
/// orphaned drag / stray link-open in any mode.
///
/// Arm order matters: the seam-drag arm is tried BEFORE the preview-link arm, so a
/// click on the border still resizes rather than opening a link.
fn handle_mouse(app: &mut App, mouse: MouseEvent) {
    match mouse.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let up = mouse.kind == MouseEventKind::ScrollUp;
            match wheel_target(mouse.column, mouse.row, app.preview_rect, app.list_rect) {
                WheelTarget::Preview => app.preview_wheel(up),
                WheelTarget::List => app.list_wheel(up),
            }
        }
        MouseEventKind::Down(MouseButton::Left)
            if on_splitter(mouse.column, mouse.row, app.list_rect, app.preview_rect) =>
        {
            app.begin_split_drag();
        }
        // A left-click inside the preview pane (the seam-drag arm above already
        // claimed the border). Gated by any modal overlay like the drag, so a
        // click while the running-session choice or the agent picker owns input
        // never opens a link.
        MouseEventKind::Down(MouseButton::Left)
            if !app.overlay_active()
                && app.preview_rect.contains(Position {
                    x: mouse.column,
                    y: mouse.row,
                }) =>
        {
            open_link_under_pointer(app, mouse.column, mouse.row);
        }
        MouseEventKind::Drag(MouseButton::Left) if app.is_dragging_split() => {
            let body_width = app.list_rect.width + app.preview_rect.width;
            app.drag_split_to(mouse.column, body_width);
        }
        MouseEventKind::Up(MouseButton::Left) => app.end_split_drag(),
        _ => {}
    }
}

/// The url of the rendered preview link under a pointer at screen `(col, row)`,
/// or `None` when the pointer is over no link.
///
/// The transcript does NOT own the whole preview pane: a REPORTED session pins a
/// status banner to the pane's first inner row (`view::preview_banner`), so its
/// transcript starts one row lower. Deriving the rect from the SAME
/// [`view::preview_split`] the view drew with is what keeps this honest — the
/// scroll offset and the cached line widths are both measured from that rect's
/// origin, so a click on screen row N resolves to the transcript line actually
/// drawn there. A session claude never reported splits off nothing and hit-tests
/// against the full inner rect, exactly as it did before the banner existed.
///
/// REPORTED, not live: an agent that reported completion still has a banner, so
/// asking the banner — never liveness — is what keeps this rect identical to the
/// one the view drew against. Liveness is a hand-off question answered by
/// [`App::is_live_now`], and it would be the wrong question here twice over: it
/// shells out to claude, and it would disagree with the drawn banner.
///
/// The wrapped-layout context (per-line widths + link regions) comes from the
/// SAME width-scoped cache the view drew from, and the url from the pure
/// [`view::link_at`]. Terminal- and process-free, so the geometry is unit
/// testable; [`open_link_under_pointer`] is the thin impure wrapper over it.
fn link_under_pointer(app: &mut App, col: u16, row: u16) -> Option<String> {
    let has_banner = view::preview_banner(app).is_some();
    let (_, transcript) = view::preview_split(app.preview_rect, has_banner);
    let (line_widths, regions) = app.preview_hit_context(transcript.width);
    view::link_at(
        col,
        row,
        transcript,
        app.preview_scroll,
        &line_widths,
        &regions,
    )
    .map(str::to_string)
}

/// Open the url of a rendered preview link under a left-click at screen
/// `(col, row)`, if any.
///
/// Thin driver over [`link_under_pointer`]: hands a hit to the fire-and-forget,
/// off-thread [`resume::open_url`]. A click that misses every link is a no-op.
/// Fails soft end to end — a bad url or missing opener never crashes the board.
fn open_link_under_pointer(app: &mut App, col: u16, row: u16) {
    if let Some(url) = link_under_pointer(app, col, row) {
        resume::open_url(&url);
    }
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
        Action::CollapseLineage => {
            app.collapse_selected();
            Outcome::Continue
        }
        Action::ExpandLineage => {
            app.expand_selected();
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
        Action::TogglePreview => {
            app.toggle_preview();
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
        Action::Chord => {
            // Arm the leader chord; `handle_event` routes the next key through
            // `handle_chord_key` before it can reach the board or the query.
            app.pending_chord = true;
            Outcome::Continue
        }
        Action::Ignore => Outcome::Continue,
    }
}

/// The four keys a pending `Ctrl-X` chord binds, plus cancel — the PURE decision
/// half of the leader chord (PATTERNS §10, keys -> actions -> outcomes). The impure
/// completion (hide / open confirm / toggle / rescan) lives in [`handle_chord_key`].
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
        _ => ChordOutcome::Cancel,
    }
}

/// Apply the key that FOLLOWS a pending `Ctrl-X` chord, then LEAVE the chord — a
/// leader chord resolves on exactly one key, hit or miss.
///
/// `x` hides / un-hides the selected session (persisting the change), `d` opens the
/// hard-delete confirm (it does NOT delete here — the confirm handler does), `h`
/// toggles the show-hidden view, `r` forces a full re-read of the store, and
/// anything else (`Esc` / `Ctrl-C` / an unbound key) abandons the chord with no side
/// effect. The pending state is cleared FIRST so an early return can never wedge the
/// board in the chord. Routed BEFORE `key_to_action` in [`handle_event`], so a
/// printable completion never leaks into the query.
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
enum ModalNav {
    /// Move the highlight forward (`→`/`↓`/`Tab`/`l`/`j`; horizontal keys Row-only).
    Next,
    /// Move the highlight backward (`←`/`↑`/`h`/`k`; horizontal keys Row-only).
    Prev,
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
/// The vertical keys (`↑`/`↓`, plus `k`/`j` and `Tab` forward) serve BOTH layouts.
/// The horizontal keys (`←`/`→`/`h`/`l`) are DERIVED from `layout`: a `Row` (button
/// strip) binds them, a `List` (vertical picker) deliberately does NOT — the two
/// overlays' key maps must not be unioned by accident. `Left`/`Right` are ALSO
/// bound on the BOARD (`CollapseLineage`/`ExpandLineage`); `handle_event`'s modal
/// gate keeps those dispatch contexts apart, so they never see the same keypress.
///
/// `Ctrl-O` is derived from `layout` for the same reason: only the `List` picker
/// has an interactive start to offer, so it must stay INERT on the running-session
/// Attach/Fork strip and the delete confirm rather than becoming a modal-wide key.
/// The action-level narrowing lives in [`launch_pick_interactively`] — the layout is
/// the key map's business, the choice's meaning is the handler's.
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
/// actually names a new session. Any other action — Attach, Fork, Delete, Cancel,
/// or an out-of-range highlight — is a NO-OP, so a future `List`-layout modal
/// cannot inherit an interactive start it has no meaning for.
///
/// The pick is recorded as the last-chosen agent FIRST — BEFORE the gate, so the
/// next `Ctrl-N` repeats it even across a refusal. This is one of the THREE points
/// a new session is actually started (the others are the draft pane's `Enter` and
/// its own `Ctrl-O`), and only those record: merely OPENING a draft must not
/// rewrite that memory, or a cancelled draft would.
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
    launch_new_session(app, agent.as_deref(), None)
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
/// * not held / no job id → refuse ([`send::INTERRUPT_NOT_LIVE`] /
///   [`send::INTERRUPT_NO_JOB_ID`]);
/// * `done`, or a TERMINAL `stopped`/`failed` → stop immediately (harmless;
///   nothing runs);
/// * any other live state → CONFIRM first ([`App::open_interrupt_confirm`]) —
///   stopping abandons live work — then stop on confirm.
///
/// The probe is the SAME authoritative bare read the resume/reply gates use
/// ([`App::live_agent_now`]) — never the polled `--all` map — a one-shot at a
/// hand-off (the documented OFF-UI-THREAD exception, PATTERNS.md §6). The id is
/// cloned so the `&Session` borrow ends before the probe and the app mutations.
fn interrupt(app: &mut App) -> Outcome {
    let Some(id) = app.selected_session().map(|s| s.session_id.clone()) else {
        return Outcome::Continue;
    };
    match send::interrupt_gate(app.live_agent_now(&id).as_ref()) {
        InterruptGate::StopNow { job_id } => dispatch_interrupt(app, &id, &job_id),
        InterruptGate::Confirm { job_id } => {
            app.open_interrupt_confirm(id, job_id);
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

/// Apply a keypress while the "stop this agent?" interrupt confirmation is open.
///
/// `Enter` confirms — the agent is stopped — so it resolves into
/// [`Outcome::Interrupt`]; `Esc`/`Ctrl-C` dismiss and return to the board. Any other
/// key is ignored: this is a deliberate confirmation, not a fat-finger.
fn handle_interrupt_confirm_key(app: &mut App, key: KeyEvent) -> Outcome {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Enter => {
            if let Some(pending) = app.pending_interrupt.take() {
                return dispatch_interrupt(app, &pending.session_id, &pending.job_id);
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
/// Those two callers differ only in whether a draft was typed, so `prompt` is what
/// separates them: the picker's `Ctrl-O` passes `None` and emits the bare argv
/// snapback has always emitted for a new session, while the draft pane passes
/// `Some(prompt)` whenever its buffer is non-empty. A `Some(prompt)` AUTO-SUBMITS
/// as the session's first turn (see [`resume::build_new_argv`]).
pub(super) fn launch_new_session(
    app: &mut App,
    agent: Option<&str>,
    prompt: Option<&str>,
) -> Outcome {
    match resume::check_new(&app.launch_dir, agent, prompt) {
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
    use crate::search::{filter, SearchMode};
    use crate::store::Session;
    use crate::tui::app::{NewSessionDraft, Scope, MIN_PANE_WIDTH, STATUS_DWELL_TICKS};
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
        }
    }

    fn reported_agent(kind: &str) -> ReportedAgent {
        ReportedAgent {
            kind: kind.to_string(),
            // Interactive by default (no attachable job id); the background
            // helper below supplies one when a test needs an attachable agent.
            id: None,
            state: None,
            status: None,
            name: None,
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
            name: None,
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
                app.set_reported_agents(reported);
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
        let id = "sess-send-e2e";
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
        let session = Session {
            file,
            session_id: id.to_string(),
            cwd: dir.clone(),
            git_branch: None,
            timestamp: None,
            repo: "repo".to_string(),
            label: "e2e".to_string(),
            root_uuid: None,
            msg_count: 0,
            content_index: String::new(),
        };
        (session, dir)
    }

    fn mouse_ev(kind: MouseEventKind, col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn wheel(app: &mut App, kind: MouseEventKind, col: u16, row: u16) -> Outcome {
        handle_event(
            app,
            AppEvent::Input(Event::Mouse(mouse_ev(kind, col, row))),
            &mut store_at(Path::new("/tmp")),
        )
    }

    // --- quick reply (Ctrl-R): gate, compose routing, send, completion ----

    /// Ctrl-R on an IDLE session opens the compose zone (force-showing the preview
    /// and targeting the selected session); on a LIVE session it refuses with the
    /// hint and opens nothing.
    #[test]
    fn ctrl_r_opens_compose_on_idle_and_refuses_a_live_session() {
        // Idle: compose opens and force-shows the preview.
        let mut app = app_with("idle", None);
        app.show_preview = false; // prove Reply force-shows it
        press_ctrl(&mut app, KeyCode::Char('r'));
        assert!(
            app.is_composing(),
            "Ctrl-R on an idle session opens compose"
        );
        assert!(app.show_preview, "opening compose force-shows the preview");
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
                    name: None,
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
                    name: None,
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
                    name: None,
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
            assert_eq!(pending.job_id, "job-k");
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
                    name: None,
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
                    name: None,
                },
            );
            map.insert(
                "b".to_string(),
                ReportedAgent {
                    kind: "background".to_string(),
                    id: Some("job-b".to_string()),
                    state: Some("done".to_string()),
                    status: None,
                    name: None,
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
            app.query.is_empty(),
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
            app.query.is_empty(),
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
        assert_eq!(app.query, "alpha bravo");
        assert!(!app.is_composing(), "a board paste opens no editor");

        // It APPENDS, exactly like type-to-search.
        paste(&mut app, "\ncharlie");
        assert_eq!(app.query, "alpha bravo charlie");
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
        assert!(app.query.is_empty(), "and must not reach the query");

        // Leader chord: `Ctrl-X` is armed and still waiting for its KEY.
        let mut app = app_with("idle", None);
        press_ctrl(&mut app, KeyCode::Char('x'));
        assert!(app.pending_chord);
        assert!(matches!(paste(&mut app, "junk\ntext"), Outcome::Continue));
        assert!(
            app.pending_chord,
            "a paste carries no chord completion, so the chord keeps waiting"
        );
        assert!(app.query.is_empty());

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
                name: None,
            },
        );
        app.set_live_probe(move || waiting.clone());
        press_ctrl(&mut app, KeyCode::Char('r'));
        assert!(app.pending_stop.is_some());
        assert!(matches!(paste(&mut app, "junk\ntext"), Outcome::Continue));
        assert!(app.pending_stop.is_some(), "the confirmation still stands");
        assert!(!app.is_composing(), "a paste must not confirm into compose");
        assert!(app.query.is_empty());

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
                name: None,
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
        assert!(app.query.is_empty());
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
        assert_eq!(app.query.chars().count(), PASTE_MAX_CHARS);
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
                name: None,
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
        assert_eq!(wheel_target(60, 10, preview, list), WheelTarget::Preview);
        assert_eq!(wheel_target(10, 10, preview, list), WheelTarget::List);
        // Outside both panes (e.g. the header row) -> preview default.
        assert_eq!(wheel_target(60, 100, preview, list), WheelTarget::Preview);
        // Hidden preview (empty rect): a point in the full-width list still
        // routes to the list.
        assert_eq!(
            wheel_target(10, 10, Rect::default(), list),
            WheelTarget::List
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

    // --- splitter drag: hit-test + full down/drag/up sequence --------------

    /// A list/preview pair mirroring `render_body`'s no-gap layout: the
    /// preview starts exactly where the list ends.
    fn split_panes() -> (Rect, Rect) {
        let list = Rect {
            x: 0,
            y: 0,
            width: 50,
            height: 20,
        };
        let preview = Rect {
            x: 50,
            y: 0,
            width: 40,
            height: 20,
        };
        (list, preview)
    }

    #[test]
    fn on_splitter_hits_the_seam_and_one_column_either_side() {
        let (list, preview) = split_panes();
        // Exactly on the seam (list.x + list.width == preview.x == 50).
        assert!(on_splitter(50, 10, list, preview));
        // One column into the list side, and one column into the preview side.
        assert!(on_splitter(49, 10, list, preview));
        assert!(on_splitter(51, 10, list, preview));
        // Two columns off either side is past the tolerance band.
        assert!(!on_splitter(48, 10, list, preview));
        assert!(!on_splitter(52, 10, list, preview));
    }

    #[test]
    fn on_splitter_requires_being_within_the_row_range() {
        let (list, preview) = split_panes();
        // On the seam column but above/below the pane rows.
        assert!(!on_splitter(50, 20, list, preview), "row is out of range");
    }

    #[test]
    fn on_splitter_never_hits_when_the_preview_is_hidden() {
        let (list, _) = split_panes();
        // `render_body` sets an EMPTY rect when the preview is hidden.
        assert!(!on_splitter(50, 10, list, Rect::default()));
    }

    #[test]
    fn mouse_up_always_clears_dragging_even_without_a_prior_down() {
        let mut app = app_with("s", None);
        assert!(!app.is_dragging_split());
        wheel(&mut app, MouseEventKind::Up(MouseButton::Left), 50, 10);
        assert!(
            !app.is_dragging_split(),
            "Up must clear dragging defensively, even with no prior Down"
        );
    }

    #[test]
    fn down_drag_up_on_the_seam_resizes_the_list_and_clears_dragging() {
        let mut app = app_with("s", None);
        let (list, preview) = split_panes();
        app.list_rect = list;
        app.preview_rect = preview;
        assert_eq!(app.list_width, None, "no drag has happened yet");

        // Press on the seam begins the drag.
        wheel(&mut app, MouseEventKind::Down(MouseButton::Left), 50, 10);
        assert!(app.is_dragging_split());

        // Dragging to column 60 moves the seam (and thus the list width) to 60.
        wheel(&mut app, MouseEventKind::Drag(MouseButton::Left), 60, 10);
        assert_eq!(app.list_width, Some(60));

        // Release ends the drag; the resized width sticks.
        wheel(&mut app, MouseEventKind::Up(MouseButton::Left), 60, 10);
        assert!(!app.is_dragging_split());
        assert_eq!(app.list_width, Some(60));
    }

    #[test]
    fn a_click_off_the_seam_never_starts_a_drag() {
        let mut app = app_with("s", None);
        let (list, preview) = split_panes();
        app.list_rect = list;
        app.preview_rect = preview;

        wheel(&mut app, MouseEventKind::Down(MouseButton::Left), 10, 10);
        assert!(
            !app.is_dragging_split(),
            "a click over the list body (not the seam) must not start a drag"
        );

        // A drag event without an active drag must be a no-op.
        wheel(&mut app, MouseEventKind::Drag(MouseButton::Left), 30, 10);
        assert_eq!(app.list_width, None, "no drag was in progress");
    }

    #[test]
    fn dragging_the_splitter_far_left_or_right_clamps_without_inverting() {
        let mut app = app_with("s", None);
        let (list, preview) = split_panes();
        app.list_rect = list;
        app.preview_rect = preview;
        let body_width = list.width + preview.width;

        wheel(&mut app, MouseEventKind::Down(MouseButton::Left), 50, 10);
        wheel(&mut app, MouseEventKind::Drag(MouseButton::Left), 0, 10);
        assert_eq!(app.list_width, Some(MIN_PANE_WIDTH));

        wheel(&mut app, MouseEventKind::Drag(MouseButton::Left), 5000, 10);
        assert_eq!(app.list_width, Some(body_width - MIN_PANE_WIDTH));
    }

    #[test]
    fn a_stray_click_on_the_seam_during_the_overlay_never_starts_a_drag() {
        // Mirrors `mouse_wheel_is_independent_of_the_overlay_gate`: a click on
        // the seam while the choice overlay owns input must not start an
        // orphaned drag that a later Drag/Up would then apply.
        let mut app = app_with("live-1", Some("background"));
        let (list, preview) = split_panes();
        app.list_rect = list;
        app.preview_rect = preview;
        press(&mut app, KeyCode::Enter); // open the overlay
        assert!(app.modal.is_some());

        wheel(&mut app, MouseEventKind::Down(MouseButton::Left), 50, 10);
        assert!(
            !app.is_dragging_split(),
            "a click on the seam during the overlay must not start a drag"
        );
        assert!(
            app.modal.is_some(),
            "the click must not disturb the overlay either"
        );
    }

    // --- preview link hit-testing across the pinned banner ------------------

    /// Board size for the link hit-tests: wide enough for a usable preview pane,
    /// short enough that `link_session`'s transcript overflows it.
    const BOARD: (u16, u16) = (100, 20);

    /// The url behind `link_session`'s one markdown link.
    const LINK_URL: &str = "https://example.com/page";

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

    /// A session whose transcript overflows the preview pane and ends in ONE
    /// markdown link, written to a real file under `dir`.
    ///
    /// A real file, not a synthetic `LinkRegion`: the hit-test resolves clicks
    /// through the SAME width-scoped preview cache the view draws from, so only a
    /// real render can prove the two agree about where the link landed.
    fn link_session(dir: &Path) -> Session {
        let file = dir.join("sess-link.jsonl");
        let mut body: String = (1..=LINK_FILLER_LINES)
            .map(|i| format!("filler line {i}\\n"))
            .collect();
        body.push_str(&format!("open [docs]({LINK_URL}) here"));
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
        }
    }

    /// An app over [`link_session`], optionally joined to a REPORTED agent in
    /// `state`.
    /// Left in `App`'s DEFAULT scroll state — bottom-anchored, as a user sees it.
    fn link_app(dir: &Path, agent_state: Option<&str>) -> App {
        let mut app = App::new(vec![link_session(dir)], Scope::All, PathBuf::from("/tmp"));
        if let Some(state) = agent_state {
            let mut reported = HashMap::new();
            reported.insert(
                "sess-link".to_string(),
                ReportedAgent {
                    kind: "background".to_string(),
                    id: None,
                    state: Some(state.to_string()),
                    status: None,
                    name: None,
                },
            );
            app.set_reported_agents(reported);
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

    #[test]
    fn a_click_on_a_drawn_link_opens_it_for_a_banner_less_session() {
        // No banner: the transcript owns the pane's whole inner rect, and the
        // hit-test must NOT shift by a row that was never reserved.
        let dir = unique_temp_dir("link-plain");
        let mut app = link_app(&dir, None);
        let buffer = render_board(&mut app);
        assert!(
            view::preview_banner(&app).is_none(),
            "a session with no joined agent reserves no banner row"
        );
        assert!(
            app.preview_scroll > 0,
            "the fixture must overflow the pane, or this never tests a scrolled hit"
        );

        let (col, row) = drawn_link_cell(&buffer, app.preview_rect);
        assert_eq!(
            link_under_pointer(&mut app, col, row).as_deref(),
            Some(LINK_URL),
            "a click on the cell the link was DRAWN on must resolve to its url"
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
            link_under_pointer(&mut app, col, row).as_deref(),
            Some(LINK_URL),
            "a click on the cell the link was DRAWN on must resolve to its url \
             even though the pinned banner pushed the transcript down a row"
        );
        // Precision, not just presence: the row ABOVE the label is a different
        // transcript line, so it must NOT resolve to the same link.
        assert_ne!(
            link_under_pointer(&mut app, col, row - 1).as_deref(),
            Some(LINK_URL),
            "the row above the label is another transcript line, not the link"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_left_click_in_the_preview_body_without_a_link_is_a_harmless_no_op() {
        // The new preview-link arm must never start a drag, open the overlay, or
        // panic when the pointer is not over a link. The synthetic session's file
        // does not exist, so the preview has no link regions — the click resolves
        // to nothing.
        let mut app = app_with("s", None);
        let (list, preview) = split_panes();
        app.list_rect = list;
        app.preview_rect = preview;

        // Well inside the preview body (col 70 of the 50..90 preview), not the seam.
        wheel(&mut app, MouseEventKind::Down(MouseButton::Left), 70, 10);
        assert!(
            !app.is_dragging_split(),
            "a preview-body click must not start a splitter drag"
        );
        assert!(
            app.modal.is_none(),
            "a preview-body click must not open the overlay"
        );
    }

    #[test]
    fn dragging_while_the_preview_is_hidden_is_a_no_op_and_never_panics() {
        let mut app = app_with("s", None);
        app.toggle_preview(); // hide the preview
        app.list_rect = Rect {
            x: 0,
            y: 0,
            width: 90,
            height: 20,
        };
        app.preview_rect = Rect::default();

        // A click anywhere never hits the (nonexistent) seam, so dragging
        // never starts; a stray Drag/Up is a harmless no-op.
        wheel(&mut app, MouseEventKind::Down(MouseButton::Left), 50, 10);
        assert!(!app.is_dragging_split());
        wheel(&mut app, MouseEventKind::Drag(MouseButton::Left), 60, 10);
        assert_eq!(app.list_width, None, "no drag was in progress to apply");
        wheel(&mut app, MouseEventKind::Up(MouseButton::Left), 60, 10);
        assert!(!app.is_dragging_split());
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
                name: None,
            },
        );
        app.set_reported_agents(reported);
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
                name: None,
            },
        );
        app.set_reported_agents(reported);
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

    /// A `ReportedAgents` event swaps the agent set in (off-thread delivery path).
    #[test]
    fn reported_agents_event_updates_the_agent_set() {
        let mut app = app_with("s", None);
        assert!(app.reported_agent("s").is_none());
        let mut reported = HashMap::new();
        reported.insert("s".to_string(), reported_agent("background"));
        handle_event(
            &mut app,
            AppEvent::ReportedAgents(reported),
            &mut store_at(Path::new("/tmp")),
        );
        assert_eq!(
            app.reported_agent("s").map(ReportedAgent::kind_label),
            Some("bg"),
            "a ReportedAgents event must update the agent set"
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
    fn jk_navigate_only_when_query_empty() {
        assert_eq!(
            key_to_action(key(KeyCode::Char('j')), true, false),
            Action::MoveDown
        );
        assert_eq!(
            key_to_action(key(KeyCode::Char('k')), true, false),
            Action::MoveUp
        );
        // Once typing, j/k are ordinary search input.
        assert_eq!(
            key_to_action(key(KeyCode::Char('j')), false, false),
            Action::Insert('j')
        );
        assert_eq!(
            key_to_action(key(KeyCode::Char('k')), false, false),
            Action::Insert('k')
        );
    }

    #[test]
    fn left_right_fold_and_expand_regardless_of_query() {
        // `←` folds a fork lineage back to its head, `→` expands it. Neither key
        // is printable, so — unlike `j`/`k`/`q` directly above — they must NOT be
        // gated on the query: a `(+N)` head found BY searching is exactly the row
        // a user most wants to open, and gating would make it unopenable without
        // first clearing the query. The `query_empty = false` half is the one with
        // teeth; it is what fails if these are ever gated like the letter keys.
        for empty in [true, false] {
            assert_eq!(
                key_to_action(key(KeyCode::Left), empty, false),
                Action::CollapseLineage
            );
            assert_eq!(
                key_to_action(key(KeyCode::Right), empty, false),
                Action::ExpandLineage
            );
        }
    }

    #[test]
    fn q_quits_only_when_query_empty() {
        assert_eq!(
            key_to_action(key(KeyCode::Char('q')), true, false),
            Action::Quit
        );
        assert_eq!(
            key_to_action(key(KeyCode::Char('q')), false, false),
            Action::Insert('q')
        );
    }

    #[test]
    fn esc_and_ctrl_c_always_quit() {
        assert_eq!(key_to_action(key(KeyCode::Esc), true, false), Action::Quit);
        assert_eq!(key_to_action(key(KeyCode::Esc), false, false), Action::Quit);
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
        // Tab toggles search mode; Ctrl-A scope; Ctrl-/ preview.
        assert_eq!(
            key_to_action(key(KeyCode::Tab), false, false),
            Action::ToggleSearchMode
        );
        assert_eq!(
            key_to_action(ctrl(KeyCode::Char('a')), false, false),
            Action::ToggleScope
        );
        assert_eq!(
            key_to_action(ctrl(KeyCode::Char('/')), false, false),
            Action::TogglePreview
        );
        // The 0x1f fallback encoding of Ctrl-/ also toggles the preview.
        assert_eq!(
            key_to_action(ctrl(KeyCode::Char('_')), false, false),
            Action::TogglePreview
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

        // The List deliberately does NOT bind the horizontal keys — they must not
        // be unioned into a vertical picker's key map.
        assert!(matches!(
            modal_key(key(KeyCode::Left), list),
            ModalNav::Ignore
        ));
        assert!(matches!(
            modal_key(key(KeyCode::Right), list),
            ModalNav::Ignore
        ));
        assert!(matches!(
            modal_key(key(KeyCode::Char('h')), list),
            ModalNav::Ignore
        ));
        assert!(matches!(
            modal_key(key(KeyCode::Char('l')), list),
            ModalNav::Ignore
        ));

        // A Row (button strip) DOES bind the horizontal keys on top of the shared
        // vertical ones — the only divergence between the two key maps.
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
            app.query.is_empty(),
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
        app.show_preview = false; // prove the draft pane force-shows it
        app.open_agent_picker(vec![def_agent("planner"), def_agent("reviewer")]);
        let out = press(&mut app, KeyCode::Enter);
        assert!(
            matches!(out, Outcome::Continue),
            "opening a pane launches nothing"
        );
        assert!(app.modal.is_none(), "Enter closes the picker");
        assert!(app.show_preview, "the draft pane force-shows the preview");
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
    /// `HOME` is redirected so `defined_agents::discover_agents` finds neither a
    /// user- nor a project-level `.claude/agents`; without that the developer's own
    /// agents would decide which branch this test exercises.
    #[test]
    fn ctrl_n_with_no_defined_agents_opens_the_draft_pane_with_no_agent() {
        let _guard = crate::config::env_lock();
        let home = unique_temp_dir("no-agents-home");
        let launch = unique_temp_dir("no-agents-launch");
        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);

        let mut app = App::new(vec![session("s")], Scope::All, launch.clone());
        app.show_preview = false; // prove the draft pane force-shows it
        let out = press_ctrl(&mut app, KeyCode::Char('n'));

        match previous_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
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
        assert!(app.show_preview, "the draft pane force-shows the preview");
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
            app.sending.is_none(),
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
            !app.overlay_active(),
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
    /// three no-teardown effects keep drawing on the same channel, so counting
    /// `BgLaunch` here would close the draft card at the moment of dispatch — which
    /// is precisely the snap-back-to-an-unrelated-transcript the card exists to
    /// prevent.
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
    }

    /// An in-flight card must not outlive the board session that dispatched it.
    ///
    /// The editor closes at dispatch, so `Ctrl-F` / `Enter` on a row stay routable
    /// while the card is up — and both hand the terminal over. The completion event
    /// that would have ended the card cannot survive that: `run_inner` builds a new
    /// `EventLoop` per board session and drops the old receiver, so the launch
    /// reports back into a channel nobody is reading and the SAME `App` re-enters
    /// the board still holding the card. That strands the preview on a placeholder
    /// for every session, with `overlay_active` stuck true (dead link clicks, dead
    /// splitter drags), recoverable only by opening and cancelling another compose.
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
                !app.overlay_active(),
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
    /// or two the launch took. The guard is the `App::sending_to` shape — the
    /// request carries an identity, the completion event carries it back, and the
    /// handler acts only on a match.
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
        assert!(app.sending.is_none());
        assert!(!app.is_composing());
    }

    // --- Ctrl-X leader chord: hide / show-hidden / hard-delete / rescan ---

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
                        name: None,
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
        app.query = "foo".to_string();
        let mut store = store_at(Path::new("/tmp"));

        feed(&mut app, ctrl(KeyCode::Char('x')), &mut store);
        assert!(app.pending_chord, "Ctrl-X arms the leader chord");

        // `h` (show-hidden) needs no selection or persistence to prove the guard,
        // and must be CONSUMED by the chord rather than typed into the query.
        feed(&mut app, key(KeyCode::Char('h')), &mut store);
        assert_eq!(
            app.query, "foo",
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
        app.sending = Some(crate::tui::app::Sending {
            session_id: "sbsend-1".to_string(),
            message: "still landing".to_string(),
            baseline_msg_count: 0,
        });
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
