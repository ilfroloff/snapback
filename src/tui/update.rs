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
//! | `Left` / `Right` | fold / expand the selected row's fork lineage (always) |
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
use ratatui::layout::{Position, Rect};

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
        AppEvent::ReportedAgents(agents) => {
            // Delivered off-thread by the agents poller; just swap the map in.
            app.set_reported_agents(agents);
            Outcome::Continue
        }
        AppEvent::Tick => {
            // The tick already drove a redraw; counting it turns that existing
            // cadence into the board's clock, which `view::blink_visible` phases
            // the live-badge pulse from. `wrapping_add` so a board left running
            // for eons rolls over instead of overflow-panicking in debug.
            app.tick = app.tick.wrapping_add(1);
            Outcome::Continue
        }
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
            // polled `--all` map. That map is up to ~1.3s stale (a ~0.26s poll
            // then a 1s sleep) and its `done` qualifier means "the agent reported
            // completion", NOT "claude will permit `-r`". Deciding from it is a
            // TOCTOU race we lose: claude re-evaluates liveness at spawn time and
            // refuses, and the user hit exactly that on a `● bg done` row. Probing
            // here shrinks the window to ~0.26s and, more importantly, replaces an
            // inference about claude's gate with claude's own answer.
            //
            // On AGENTS.md's "OFF-UI-THREAD blocking work": that rule exists so the
            // 1s POLL never blocks rendering, and the poll is untouched — still one
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
/// **Attach re-asks claude here, at the hand-off.** Its target is the agent-view
/// job `id` (the SHORT id from `claude agents --json`) taken from
/// [`App::live_agent_now`]'s fresh record — NEVER from the polled `--all` map.
/// That map is the same ~1.3s-stale snapshot the resume gate was moved off, and
/// reading an attach id from it is the identical bug one layer down: an
/// authoritative decision made from stale data. Here it is worse than at the
/// gate, because the overlay can sit open INDEFINITELY while the user decides —
/// even the probe that opened it is stale by the time Attach is chosen, so the
/// window is unbounded rather than ~1.3s. The rule is uniform: every hand-off
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
    use std::time::{SystemTime, UNIX_EPOCH};

    use ratatui::backend::TestBackend;
    use ratatui::style::Modifier;
    use ratatui::Terminal;

    use crate::agents::ReportedAgent;
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
            app.pending_live.is_none(),
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
    /// The `--all` poll badged this session `done` (up to ~1.3s ago), but claude's
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
        let pending = app.pending_live.clone().expect(
            "claude reports this session LIVE, so Enter must open the \
             Attach/Fork overlay even though the polled badge still says `done` \
             — trusting the stale badge here is the TOCTOU bug",
        );
        assert_eq!(pending.session_id, "sess-raced");

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
        let pending = app
            .pending_live
            .clone()
            .expect("a working agent is live, so Enter must open the overlay");
        assert_eq!(pending.session_id, "sess-working");

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
            app.pending_live.is_none(),
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
            app.pending_live.is_none(),
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
            app.pending_live.as_ref().unwrap().selected,
            LiveChoice::Attach
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
             `stale-job` here means it was read back off the ~1.3s-stale `--all` map"
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
            app.pending_live.is_some(),
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
        assert!(app.pending_live.is_none(), "the overlay closes on confirm");

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
        assert!(app.pending_live.is_some());

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
            app.pending_live.as_ref().unwrap().selected,
            LiveChoice::Fork
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
            Path::new("/tmp"),
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
            let out = handle_event(&mut app, AppEvent::Tick, Path::new("/tmp"));
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
        handle_event(&mut app, AppEvent::Tick, Path::new("/tmp"));
        assert_eq!(app.tick, 0, "the clock must wrap from u64::MAX back to 0");
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
    fn left_right_fold_and_expand_regardless_of_query() {
        // `←` folds a fork lineage back to its head, `→` expands it. Neither key
        // is printable, so — unlike `j`/`k`/`q` directly above — they must NOT be
        // gated on the query: a `(+N)` head found BY searching is exactly the row
        // a user most wants to open, and gating would make it unopenable without
        // first clearing the query. The `query_empty = false` half is the one with
        // teeth; it is what fails if these are ever gated like the letter keys.
        for empty in [true, false] {
            assert_eq!(
                key_to_action(key(KeyCode::Left), empty),
                Action::CollapseLineage
            );
            assert_eq!(
                key_to_action(key(KeyCode::Right), empty),
                Action::ExpandLineage
            );
        }
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
