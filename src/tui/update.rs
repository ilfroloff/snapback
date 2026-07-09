//! The elm-style update loop (pure state transitions).
//!
//! On `Input` handles keybindings; on `SessionsChanged` reloads the store and
//! re-applies query/scope while preserving selection-by-id and scroll; on
//! `Tick` does nothing costly. Restores selection by locating the selected
//! `session_id` in the new filtered list (clamps to nearest if it vanished).
//!
//! This module is the *decision* half of the loop: [`key_to_action`] maps a key
//! to an [`Action`], and [`handle_event`] applies an [`AppEvent`] to the [`App`]
//! and returns an [`Outcome`] telling the driver (in [`crate::tui`]) whether to
//! continue, quit, or hand off a resume. All of it is terminal-free and unit
//! tested; the terminal-driving loop that calls it lives in [`crate::tui::run`].
//!
//! ## Keybindings
//!
//! | Key | Action |
//! | --- | ------ |
//! | `Up` / `Down` | move selection (always) |
//! | `j` / `k` | move selection (only while the query is empty; otherwise typed) |
//! | `Enter` | resume the selected session |
//! | `Ctrl-F` | fork-resume the selected session |
//! | `Ctrl-N` | start a new session in the launch directory (pick an agent when any are defined) |
//! | `Tab` | toggle name-only vs. name+content search |
//! | `Ctrl-A` | toggle scope (current-folder <-> all) |
//! | `Ctrl-/` | toggle the preview pane |
//! | `PgUp` / `PgDn` | scroll the preview a page (always) |
//! | `Ctrl-U` / `Ctrl-D` | scroll the preview a quarter page (always) |
//! | `Home` / `End` | jump the preview to top / bottom (always) |
//! | `Backspace` | delete the last query character |
//! | printable char | type-to-search (append to the query) |
//! | `q` | quit (only while the query is empty; otherwise typed) |
//! | `Esc` / `Ctrl-C` | quit (always) |
//!
//! `j`/`k`/`q` are disambiguated by whether the query is empty: in the default
//! browse state they navigate/quit; once you are typing a query they become
//! ordinary search input. Arrows, `Enter`, `Tab`, and every `Ctrl-` binding
//! work regardless of the query, so search is never blocked.

use std::path::Path;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Margin, Position, Rect};

use crate::defined_agents;
use crate::resume::{self, Ready};
use crate::store::SessionStore;
use crate::watch::AppEvent;

use super::app::{App, LiveChoice};
use super::view;

/// A decoded intent from a single keypress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Quit the app.
    Quit,
    /// Move the selection up one row.
    MoveUp,
    /// Move the selection down one row.
    MoveDown,
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
    /// Toggle name-only vs. name+content search.
    ToggleSearchMode,
    /// Toggle current-folder vs. all scope.
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
    /// Append a character to the query (type-to-search).
    Insert(char),
    /// Delete the last query character.
    Backspace,
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
}

/// Map a keypress to an [`Action`]. `query_empty` disambiguates the `j`/`k`/`q`
/// keys: they navigate/quit only in the default browse state and are otherwise
/// ordinary search input.
#[must_use]
pub fn key_to_action(key: KeyEvent, query_empty: bool) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    if ctrl {
        return match key.code {
            KeyCode::Char('f') | KeyCode::Char('F') => Action::Resume { fork: true },
            // Ctrl-/ toggles the preview. Terminals that map Ctrl-/ to the
            // control code 0x1f surface it as Char('_'); accept both.
            KeyCode::Char('/') | KeyCode::Char('_') => Action::TogglePreview,
            KeyCode::Char('a') | KeyCode::Char('A') => Action::ToggleScope,
            KeyCode::Char('n') | KeyCode::Char('N') => Action::NewSession,
            KeyCode::Char('c') | KeyCode::Char('C') => Action::Quit,
            // Quarter-page preview scroll (readline-style). Acts regardless of
            // the query, like the arrows, so search never blocks preview scrolling.
            KeyCode::Char('u') | KeyCode::Char('U') => Action::PreviewHalfUp,
            KeyCode::Char('d') | KeyCode::Char('D') => Action::PreviewHalfDown,
            _ => Action::Ignore,
        };
    }

    match key.code {
        KeyCode::Up => Action::MoveUp,
        KeyCode::Down => Action::MoveDown,
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
///   [`App::begin_split_drag`] and the `pending_live` guard).
/// * `SessionsChanged` -> reload the store from `root` and re-apply query+scope,
///   preserving selection-by-id and scroll (see [`App::apply_sessions`]).
/// * `Tick` -> nothing costly (just a redraw upstream).
pub fn handle_event(app: &mut App, event: AppEvent, root: &Path) -> Outcome {
    match event {
        AppEvent::Input(Event::Key(key)) if is_actionable(key) => {
            // While the running-session choice overlay is open it OWNS the
            // keyboard: keys navigate/confirm/cancel the overlay, never the board.
            if app.pending_live.is_some() {
                return handle_live_choice_key(app, key);
            }
            // The new-session agent picker likewise owns the keyboard while open.
            if app.pending_agent.is_some() {
                return handle_agent_pick_key(app, key);
            }
            // A transient status (e.g. a resume refusal) lives exactly until the
            // next key; clear it first so this keypress may set a fresh one.
            app.clear_status();
            apply_action(app, key_to_action(key, app.query.is_empty()))
        }
        // Mouse wheel scroll and splitter drag. A dedicated arm BEFORE the
        // input catch-all and INDEPENDENT of the `pending_live` overlay gate
        // above: neither routes into the overlay handler — they just scroll a
        // pane / resize the split and never crash in any mode (query active,
        // overlay open, ...). A stray click cannot start an orphaned drag
        // while the overlay is open (`App::begin_split_drag` gates it).
        AppEvent::Input(Event::Mouse(mouse)) => {
            handle_mouse(app, mouse);
            Outcome::Continue
        }
        AppEvent::Input(_) => Outcome::Continue,
        AppEvent::SessionsChanged => {
            app.apply_sessions(SessionStore::load_from(root));
            Outcome::Continue
        }
        AppEvent::LiveAgents(live) => {
            // Delivered off-thread by the agents poller; just swap the map in.
            app.set_live(live);
            Outcome::Continue
        }
        AppEvent::Tick => Outcome::Continue,
    }
}

/// Only press/repeat key events act; release events (kitty protocol / Windows)
/// are ignored so a keystroke is never handled twice.
fn is_actionable(key: KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
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
/// a `pending_live` guard (link open), so this never crashes or starts an
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

/// The preview pane's INNER rect (inside the borders), matching how
/// `view::render_preview` derives its inner width/height. Borders steal one cell
/// on each side; [`Rect::inner`] saturates, so a degenerate pane yields a
/// zero-area rect that hit-tests to nothing rather than panicking.
fn preview_inner(rect: Rect) -> Rect {
    rect.inner(Margin {
        horizontal: 1,
        vertical: 1,
    })
}

/// Open the url of a rendered preview link under a left-click at screen
/// `(col, row)`, if any.
///
/// Pulls the wrapped-layout context (per-line widths + link regions) from the
/// SAME width-scoped cache the view drew from, so hit-testing matches the screen;
/// resolves the url with the pure [`view::link_at`]; and hands a hit to the
/// fire-and-forget, off-thread [`resume::open_url`]. A click that misses every
/// link is a no-op. Fails soft end to end — a bad url or missing opener never
/// crashes the board.
fn open_link_under_pointer(app: &mut App, col: u16, row: u16) {
    let inner = preview_inner(app.preview_rect);
    let (line_widths, regions) = app.preview_hit_context(inner.width);
    if let Some(url) = view::link_at(col, row, inner, app.preview_scroll, &line_widths, &regions) {
        resume::open_url(url);
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
        Action::Resume { fork } => {
            // Smart Enter: `claude -r` REFUSES to plain-resume a LIVE session, so
            // Enter (not Ctrl-F) on a running row opens the Attach/Fork/Cancel
            // choice instead. Ctrl-F fork stays a direct hand-off for ANY session.
            if !fork {
                if let Some(session) = app.selected_session() {
                    if app.is_live(&session.session_id) {
                        let id = session.session_id.clone();
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
        Action::Insert(c) => {
            app.push_query_char(c);
            Outcome::Continue
        }
        Action::Backspace => {
            app.pop_query_char();
            Outcome::Continue
        }
        Action::Ignore => Outcome::Continue,
    }
}

/// A decoded intent while the running-session choice overlay owns the keyboard.
enum LiveNav {
    /// Move the highlight forward (`→`/`↓`/`Tab`/`l`/`j`).
    Next,
    /// Move the highlight backward (`←`/`↑`/`h`/`k`).
    Prev,
    /// Act on the highlighted choice (`Enter`).
    Confirm,
    /// Dismiss the overlay (`Esc`/`Ctrl-C`).
    Cancel,
    /// A key with no binding in the overlay.
    Ignore,
}

/// Map a keypress to a [`LiveNav`] while the choice overlay is open.
fn live_choice_key(key: KeyEvent) -> LiveNav {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') | KeyCode::Char('C') => LiveNav::Cancel,
            _ => LiveNav::Ignore,
        };
    }
    match key.code {
        KeyCode::Left | KeyCode::Up | KeyCode::Char('h') | KeyCode::Char('k') => LiveNav::Prev,
        KeyCode::Right | KeyCode::Down | KeyCode::Tab | KeyCode::Char('l') | KeyCode::Char('j') => {
            LiveNav::Next
        }
        KeyCode::Enter => LiveNav::Confirm,
        KeyCode::Esc => LiveNav::Cancel,
        _ => LiveNav::Ignore,
    }
}

/// Apply an overlay keypress: navigation stays on the board; Confirm routes the
/// highlighted choice; Esc/Ctrl-C dismiss.
fn handle_live_choice_key(app: &mut App, key: KeyEvent) -> Outcome {
    match live_choice_key(key) {
        LiveNav::Next => {
            app.live_choice_next();
            Outcome::Continue
        }
        LiveNav::Prev => {
            app.live_choice_prev();
            Outcome::Continue
        }
        LiveNav::Cancel => {
            app.live_choice_cancel();
            Outcome::Continue
        }
        LiveNav::Confirm => confirm_live_choice(app),
        LiveNav::Ignore => Outcome::Continue,
    }
}

/// Which teardown-safe hand-off a confirmed overlay choice runs.
enum Handoff {
    /// `claude attach <job-id>` — reattach to the running agent in this
    /// terminal, keyed on its short agent-view id (resolved in [`route_handoff`]).
    Attach,
    /// `claude -r <id> --fork-session` — branch off a copy.
    Fork,
}

/// Resolve the highlighted overlay choice into a driver [`Outcome`].
///
/// The overlay closes on any confirm. Attach/Fork run the terminal-up refusal
/// gate and, on success, escalate to [`Outcome::Resume`] so the driver spawns
/// them through the IDENTICAL teardown→spawn→wait→return round trip as a plain
/// resume; a refusal (deleted worktree / unreadable file) sets a board status.
fn confirm_live_choice(app: &mut App) -> Outcome {
    let Some(pending) = app.pending_live.clone() else {
        return Outcome::Continue;
    };
    app.live_choice_cancel();
    match pending.selected {
        LiveChoice::Cancel => Outcome::Continue,
        LiveChoice::Attach => route_handoff(app, &pending.session_id, Handoff::Attach),
        LiveChoice::Fork => route_handoff(app, &pending.session_id, Handoff::Fork),
    }
}

/// Run the refusal gate for a chosen hand-off and escalate a confirmed plan to
/// [`Outcome::Resume`]; a refusal sets a board status. The `map` drops the
/// `&Session` borrow before `set_status` mutably touches `app`.
///
/// For Attach, the target is the matched live agent's agent-view job `id` (the
/// SHORT id from `claude agents --json`) — resolved here and handed to
/// [`resume::check_attach`], which gates the interactive (no-id) case. The
/// clone releases the `live` borrow before `session_by_id` re-borrows `app`.
fn route_handoff(app: &mut App, session_id: &str, kind: Handoff) -> Outcome {
    let agent_id = app.live_agent(session_id).and_then(|a| a.id.clone());
    let checked = app.session_by_id(session_id).map(|s| match kind {
        Handoff::Attach => resume::check_attach(s, agent_id.as_deref()),
        Handoff::Fork => resume::check(s, true),
    });
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
/// picker (pre-highlighted on the last pick) and stay on the board; otherwise
/// launch a bare `claude` immediately, so the common no-agent case keeps its
/// zero-extra-keystroke path. Discovery is FAIL-SOFT — any error yields an empty
/// list, which just means the bare-launch branch (see
/// [`defined_agents::discover_agents`]).
fn new_session(app: &mut App) -> Outcome {
    let agents = defined_agents::discover_agents(&app.launch_dir);
    if agents.is_empty() {
        // No selectable agents: a one-entry picker would be pure friction —
        // launch straight into a bare `claude`, exactly as before agents existed.
        return launch_new_session(app, None);
    }
    app.open_agent_picker(agents);
    Outcome::Continue
}

/// Run the new-session existence gate for `agent` (`None` = no agent) while the
/// terminal is still up, and escalate a confirmed plan to [`Outcome::Resume`]; a
/// refusal (a deleted launch dir) sets a transient board status. Shared by the
/// no-agent fast path and the picker confirm so the gate + status handling live
/// in one place. `check_new` returns an owned `Result`, so the `&launch_dir`
/// borrow is released before we mutably touch `app` for `set_status`.
fn launch_new_session(app: &mut App, agent: Option<&str>) -> Outcome {
    match resume::check_new(&app.launch_dir, agent) {
        Ok(ready) => Outcome::Resume(ready),
        Err(err) => {
            app.set_status(err.message().to_string());
            Outcome::Continue
        }
    }
}

/// A decoded intent while the new-session agent picker owns the keyboard.
enum AgentNav {
    /// Move the highlight down one row (`↓`/`Tab`/`j`).
    Next,
    /// Move the highlight up one row (`↑`/`k`).
    Prev,
    /// Start the session bound to the highlighted agent (`Enter`).
    Confirm,
    /// Dismiss the picker without starting a session (`Esc`/`Ctrl-C`).
    Cancel,
    /// A key with no binding in the picker.
    Ignore,
}

/// Map a keypress to an [`AgentNav`] while the agent picker is open. The picker
/// is a vertical list, so Up/Down (and `k`/`j`, mirroring the board's nav keys)
/// move the highlight; `Tab` also steps forward for parity with the running-
/// session overlay.
fn agent_pick_key(key: KeyEvent) -> AgentNav {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') | KeyCode::Char('C') => AgentNav::Cancel,
            _ => AgentNav::Ignore,
        };
    }
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => AgentNav::Prev,
        KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => AgentNav::Next,
        KeyCode::Enter => AgentNav::Confirm,
        KeyCode::Esc => AgentNav::Cancel,
        _ => AgentNav::Ignore,
    }
}

/// Apply a picker keypress: navigation stays on the board; Confirm starts the
/// chosen agent's session; Esc/Ctrl-C dismiss.
fn handle_agent_pick_key(app: &mut App, key: KeyEvent) -> Outcome {
    match agent_pick_key(key) {
        AgentNav::Next => {
            app.agent_pick_next();
            Outcome::Continue
        }
        AgentNav::Prev => {
            app.agent_pick_prev();
            Outcome::Continue
        }
        AgentNav::Cancel => {
            app.agent_pick_cancel();
            Outcome::Continue
        }
        AgentNav::Confirm => confirm_agent_pick(app),
        AgentNav::Ignore => Outcome::Continue,
    }
}

/// Resolve the highlighted picker row into a driver [`Outcome`].
///
/// Records the chosen agent as the last pick (so the next `Ctrl-N` repeats it),
/// closes the picker, and runs the new-session gate for it — a confirmed plan
/// escalates to [`Outcome::Resume`] through the IDENTICAL teardown→spawn→wait→
/// return round trip as a resume; a refusal sets a board status. The clone
/// releases the `pending_agent` borrow before `set_last_new_agent` / `check_new`
/// re-borrow `app`.
fn confirm_agent_pick(app: &mut App) -> Outcome {
    let Some(pending) = app.pending_agent.clone() else {
        return Outcome::Continue;
    };
    let agent = pending.selected_agent().map(str::to_owned);
    app.set_last_new_agent(agent.clone());
    app.agent_pick_cancel();
    launch_new_session(app, agent.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;
    use std::path::PathBuf;

    use crate::agents::LiveAgent;
    use crate::store::Session;
    use crate::tui::app::{Scope, MIN_PANE_WIDTH};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
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
            content_index: String::new(),
        }
    }

    fn live_agent(kind: &str) -> LiveAgent {
        LiveAgent {
            kind: kind.to_string(),
            // Interactive by default (no attachable job id); the background
            // helper below supplies one when a test needs an attachable agent.
            id: None,
            state: None,
            status: None,
            name: None,
        }
    }

    /// An app over one session, optionally marked live, with the row selected.
    fn app_with(id: &str, live_kind: Option<&str>) -> App {
        let mut app = App::new(vec![session(id)], Scope::All, PathBuf::from("/tmp"));
        if let Some(kind) = live_kind {
            let mut live = HashMap::new();
            live.insert(id.to_string(), live_agent(kind));
            app.set_live(live);
        }
        assert_eq!(app.selected.as_deref(), Some(id));
        app
    }

    fn press(app: &mut App, code: KeyCode) -> Outcome {
        handle_event(
            app,
            AppEvent::Input(Event::Key(key(code))),
            Path::new("/tmp"),
        )
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
            Path::new("/tmp"),
        )
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
        // Repeated down notches near the ceiling never overflow past u16::MAX.
        app.preview_scroll = u16::MAX - 1;
        wheel(&mut app, MouseEventKind::ScrollDown, 60, 10);
        assert_eq!(app.preview_scroll, u16::MAX);
        wheel(&mut app, MouseEventKind::ScrollDown, 60, 10);
        assert_eq!(
            app.preview_scroll,
            u16::MAX,
            "wheel-down saturates at u16::MAX"
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
        assert!(
            app.pending_live.is_none(),
            "a list wheel must not open an overlay"
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
        assert!(app.pending_live.is_some());
        let highlight = app.pending_live.as_ref().unwrap().selected;

        wheel(&mut app, MouseEventKind::ScrollDown, 60, 10);
        assert!(
            app.pending_live.is_some(),
            "a wheel must not dismiss the overlay"
        );
        assert_eq!(
            app.pending_live.as_ref().unwrap().selected,
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
        assert!(app.pending_live.is_some());

        wheel(&mut app, MouseEventKind::Down(MouseButton::Left), 50, 10);
        assert!(
            !app.is_dragging_split(),
            "a click on the seam during the overlay must not start a drag"
        );
        assert!(
            app.pending_live.is_some(),
            "the click must not disturb the overlay either"
        );
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
            app.pending_live.is_none(),
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
        assert!(app.pending_live.is_none());

        let out = press(&mut app, KeyCode::Enter);
        assert!(matches!(out, Outcome::Continue));
        let pending = app
            .pending_live
            .clone()
            .expect("Enter on a live row opens the overlay");
        assert_eq!(pending.session_id, "live-1");
        assert_eq!(pending.selected, LiveChoice::Attach, "defaults to Attach");

        // Overlay owns the keyboard: → cycles Attach -> Fork -> Cancel.
        press(&mut app, KeyCode::Right);
        assert_eq!(
            app.pending_live.as_ref().unwrap().selected,
            LiveChoice::Fork
        );
        press(&mut app, KeyCode::Right);
        assert_eq!(
            app.pending_live.as_ref().unwrap().selected,
            LiveChoice::Cancel
        );

        // Confirming Cancel dismisses the overlay and stays on the board.
        let out = press(&mut app, KeyCode::Enter);
        assert!(matches!(out, Outcome::Continue));
        assert!(app.pending_live.is_none(), "Cancel returns to the board");
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
            app.pending_live.as_ref().unwrap().selected,
            LiveChoice::Attach
        );
        let out = press(&mut app, KeyCode::Enter);

        // Stays on the board (no Resume escalation) with the no-job hint shown.
        assert!(
            matches!(out, Outcome::Continue),
            "an interactive session must not escalate to a hand-off"
        );
        assert!(app.pending_live.is_none(), "the overlay closes on confirm");
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
            app.pending_live.is_none(),
            "a non-live Enter must not open the overlay"
        );
    }

    /// Esc dismisses the overlay without acting.
    #[test]
    fn esc_dismisses_the_choice_overlay() {
        let mut app = app_with("live-1", Some("interactive"));
        press(&mut app, KeyCode::Enter);
        assert!(app.pending_live.is_some());
        press(&mut app, KeyCode::Esc);
        assert!(app.pending_live.is_none(), "Esc dismisses the overlay");
    }

    /// Ctrl-F fork stays a direct hand-off for a LIVE session (no overlay).
    #[test]
    fn ctrl_f_forks_a_live_session_directly_without_the_overlay() {
        let mut app = app_with("live-1", Some("background"));
        let out = handle_event(
            &mut app,
            AppEvent::Input(Event::Key(ctrl(KeyCode::Char('f')))),
            Path::new("/tmp"),
        );
        assert!(matches!(out, Outcome::Continue));
        assert!(
            app.pending_live.is_none(),
            "Ctrl-F must not open the choice overlay, even for a live session"
        );
    }

    /// A `LiveAgents` event swaps the live set in (off-thread delivery path).
    #[test]
    fn live_agents_event_updates_the_live_set() {
        let mut app = app_with("s", None);
        assert!(!app.is_live("s"));
        let mut live = HashMap::new();
        live.insert("s".to_string(), live_agent("background"));
        handle_event(&mut app, AppEvent::LiveAgents(live), Path::new("/tmp"));
        assert!(
            app.is_live("s"),
            "a LiveAgents event must update the live set"
        );
    }

    #[test]
    fn arrows_always_move() {
        assert_eq!(key_to_action(key(KeyCode::Up), true), Action::MoveUp);
        assert_eq!(key_to_action(key(KeyCode::Down), true), Action::MoveDown);
        // Arrows navigate even mid-query.
        assert_eq!(key_to_action(key(KeyCode::Up), false), Action::MoveUp);
        assert_eq!(key_to_action(key(KeyCode::Down), false), Action::MoveDown);
    }

    #[test]
    fn jk_navigate_only_when_query_empty() {
        assert_eq!(
            key_to_action(key(KeyCode::Char('j')), true),
            Action::MoveDown
        );
        assert_eq!(key_to_action(key(KeyCode::Char('k')), true), Action::MoveUp);
        // Once typing, j/k are ordinary search input.
        assert_eq!(
            key_to_action(key(KeyCode::Char('j')), false),
            Action::Insert('j')
        );
        assert_eq!(
            key_to_action(key(KeyCode::Char('k')), false),
            Action::Insert('k')
        );
    }

    #[test]
    fn q_quits_only_when_query_empty() {
        assert_eq!(key_to_action(key(KeyCode::Char('q')), true), Action::Quit);
        assert_eq!(
            key_to_action(key(KeyCode::Char('q')), false),
            Action::Insert('q')
        );
    }

    #[test]
    fn esc_and_ctrl_c_always_quit() {
        assert_eq!(key_to_action(key(KeyCode::Esc), true), Action::Quit);
        assert_eq!(key_to_action(key(KeyCode::Esc), false), Action::Quit);
        assert_eq!(key_to_action(ctrl(KeyCode::Char('c')), false), Action::Quit);
    }

    #[test]
    fn enter_resumes_and_ctrl_f_forks() {
        assert_eq!(
            key_to_action(key(KeyCode::Enter), true),
            Action::Resume { fork: false }
        );
        assert_eq!(
            key_to_action(ctrl(KeyCode::Char('f')), false),
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
                key_to_action(ctrl(KeyCode::Char('n')), empty),
                Action::NewSession
            );
            assert_eq!(
                key_to_action(ctrl(KeyCode::Char('N')), empty),
                Action::NewSession
            );
        }
    }

    #[test]
    fn toggles_are_reachable_regardless_of_query() {
        // Tab toggles search mode; Ctrl-A scope; Ctrl-/ preview.
        assert_eq!(
            key_to_action(key(KeyCode::Tab), false),
            Action::ToggleSearchMode
        );
        assert_eq!(
            key_to_action(ctrl(KeyCode::Char('a')), false),
            Action::ToggleScope
        );
        assert_eq!(
            key_to_action(ctrl(KeyCode::Char('/')), false),
            Action::TogglePreview
        );
        // The 0x1f fallback encoding of Ctrl-/ also toggles the preview.
        assert_eq!(
            key_to_action(ctrl(KeyCode::Char('_')), false),
            Action::TogglePreview
        );
    }

    #[test]
    fn preview_scroll_keys_act_regardless_of_query() {
        // Page + jump keys are not printable, so they scroll the preview whether
        // or not the user is mid-query.
        for empty in [true, false] {
            assert_eq!(
                key_to_action(key(KeyCode::PageUp), empty),
                Action::PreviewPageUp
            );
            assert_eq!(
                key_to_action(key(KeyCode::PageDown), empty),
                Action::PreviewPageDown
            );
            assert_eq!(key_to_action(key(KeyCode::Home), empty), Action::PreviewTop);
            assert_eq!(
                key_to_action(key(KeyCode::End), empty),
                Action::PreviewBottom
            );
            // Ctrl-U / Ctrl-D quarter-page, also independent of query state.
            assert_eq!(
                key_to_action(ctrl(KeyCode::Char('u')), empty),
                Action::PreviewHalfUp
            );
            assert_eq!(
                key_to_action(ctrl(KeyCode::Char('d')), empty),
                Action::PreviewHalfDown
            );
        }
    }

    #[test]
    fn printable_characters_type_into_the_query() {
        assert_eq!(
            key_to_action(key(KeyCode::Char('a')), true),
            Action::Insert('a')
        );
        assert_eq!(
            key_to_action(key(KeyCode::Char('z')), false),
            Action::Insert('z')
        );
        assert_eq!(
            key_to_action(key(KeyCode::Backspace), false),
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
        // The picker is a vertical list: Up/Down (and k/j, plus Tab forward)
        // navigate; Enter confirms; Esc / Ctrl-C cancel.
        assert!(matches!(agent_pick_key(key(KeyCode::Down)), AgentNav::Next));
        assert!(matches!(
            agent_pick_key(key(KeyCode::Char('j'))),
            AgentNav::Next
        ));
        assert!(matches!(agent_pick_key(key(KeyCode::Tab)), AgentNav::Next));
        assert!(matches!(agent_pick_key(key(KeyCode::Up)), AgentNav::Prev));
        assert!(matches!(
            agent_pick_key(key(KeyCode::Char('k'))),
            AgentNav::Prev
        ));
        assert!(matches!(
            agent_pick_key(key(KeyCode::Enter)),
            AgentNav::Confirm
        ));
        assert!(matches!(
            agent_pick_key(key(KeyCode::Esc)),
            AgentNav::Cancel
        ));
        assert!(matches!(
            agent_pick_key(ctrl(KeyCode::Char('c'))),
            AgentNav::Cancel
        ));
    }

    #[test]
    fn picker_confirm_on_the_default_row_starts_a_bare_claude() {
        // `app_with` uses `/tmp` as the launch dir (exists), so the new-session
        // gate proceeds and the default (row 0) confirms to a bare `claude`.
        let mut app = app_with("s", None);
        app.open_agent_picker(vec![def_agent("planner"), def_agent("reviewer")]);
        let out = press(&mut app, KeyCode::Enter);
        match out {
            Outcome::Resume(ready) => assert_eq!(ready.argv.join(" "), "claude"),
            _ => panic!("the default pick must start a bare claude"),
        }
        assert!(app.pending_agent.is_none(), "confirm closes the picker");
    }

    #[test]
    fn picker_confirm_on_an_agent_row_binds_it_and_remembers_the_pick() {
        let mut app = app_with("s", None);
        app.open_agent_picker(vec![def_agent("planner"), def_agent("reviewer")]);
        // Down once from the default row -> the first agent (planner).
        press(&mut app, KeyCode::Down);
        assert_eq!(
            app.pending_agent.as_ref().unwrap().selected_agent(),
            Some("planner")
        );
        let out = press(&mut app, KeyCode::Enter);
        match out {
            Outcome::Resume(ready) => {
                assert_eq!(ready.argv.join(" "), "claude --agent planner");
                // The plan carries the new-session hint, not the resume one.
                assert_eq!(ready.nonzero_hint, resume::NEW_SESSION_NONZERO_HINT);
            }
            _ => panic!("an agent pick must start `claude --agent planner`"),
        }
        // The pick is remembered in-memory: the NEXT picker pre-highlights it.
        app.open_agent_picker(vec![def_agent("planner"), def_agent("reviewer")]);
        assert_eq!(
            app.pending_agent.as_ref().unwrap().selected_agent(),
            Some("planner"),
            "the last pick pre-highlights on the next Ctrl-N"
        );
    }

    #[test]
    fn picker_esc_dismisses_without_starting_a_session() {
        let mut app = app_with("s", None);
        app.open_agent_picker(vec![def_agent("planner")]);
        let out = press(&mut app, KeyCode::Esc);
        assert!(matches!(out, Outcome::Continue));
        assert!(app.pending_agent.is_none(), "Esc dismisses the picker");
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
        assert!(
            app.pending_agent.is_some(),
            "an inert key leaves the picker open"
        );
    }
}
