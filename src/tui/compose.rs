//! The compose zone: multiline input + its key dispatch.
//!
//! This module OWNS the `ratatui_textarea` dependency the way `search.rs` owns
//! nucleo: every reference to the text-editor widget lives here (plus the
//! `App::compose` field, whose type is [`ComposeState`]). The compose zone is a
//! modal — while it is open it owns the keyboard, exactly like the running-session
//! and agent-pick overlays — and leaves by submitting (`Enter`), running
//! interactively (`Ctrl-O`), or cancelling (`Esc`).
//!
//! It is also the only installer of the PANE-level twin,
//! [`App::draft`](super::app::App::draft): [`open_background`] opens the editor and
//! the placeholder card together (via `App::open_compose`), and every exit closes
//! both through `App::close_compose`. `Enter` on a BACKGROUND draft is the one
//! exception, and a deliberate one — `App::dispatch_draft` closes the editor but
//! leaves the card up, in flight, until `AppEvent::BgLaunchFinished` lands.
//!
//! ONE editor and ONE key router serve TWO drafts, distinguished by
//! [`ComposeTarget`] rather than by parallel state:
//!
//! * [`ComposeTarget::Reply`] — the quick reply `Ctrl-R` opens on a selected
//!   session. `Enter` sends a one-shot `claude -p -r` ([`crate::send`]).
//! * [`ComposeTarget::NewBackgroundAgent`] — the draft `Ctrl-N` opens: via `Enter`
//!   on the agent picker's highlighted row, or directly when no agents are defined.
//!   `Enter` starts a BACKGROUND agent with the draft as its first prompt, and
//!   `Ctrl-O` runs it interactively instead (the one action that leaves the board)
//!   — the same verb the picker's own `Ctrl-O` names, so "open interactive claude"
//!   reads identically on both surfaces.
//!
//! The pure DECISION is [`compose_key_to_action`], unit-tested like
//! [`super::update::key_to_action`] and free of any `TextArea` reference. The
//! impure edits (insert a newline, forward a keystroke to the widget) and both
//! hand-offs live in [`handle_compose_key`], a thin driver over that decision and
//! over the pure cores in [`crate::send`] / [`crate::resume`].
//!
//! [`insert_paste`] is the ONE entry point that is not a keypress: a terminal paste
//! arrives as whole TEXT (`super::update::handle_paste`) and goes straight into the
//! editor, bypassing the key router entirely. That bypass is the point — routed as
//! keystrokes, a pasted newline reached the `Enter` = Send arm above.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_textarea::{CursorMove, TextArea, WrapMode};

use crate::send::{self, BgLaunchRequest, SendPlan, SendRequest};

use super::app::{App, NewSessionDraft};
use super::update::Outcome;

/// Status shown when Send is pressed on an empty / whitespace-only reply buffer:
/// the compose zone stays open (nothing was sent), so this is a gentle nudge
/// rather than a refusal.
const COMPOSE_EMPTY_HINT: &str = "nothing to send — type a message first";

/// The [`COMPOSE_EMPTY_HINT`] of the background draft: `Enter` on an empty buffer
/// keeps the pane open rather than launching. Its own const because the nudge is
/// different in kind — a background agent started with no prompt would sit there
/// doing nothing, so this is not "you forgot to type", it is "there would be
/// nothing for it to do".
const COMPOSE_EMPTY_BG_HINT: &str = "nothing to run — a background agent needs a first message";

/// Status when the composed session vanished from the store between opening the
/// compose zone and pressing Send (e.g. its file was removed).
const COMPOSE_SESSION_GONE: &str = "that session is no longer loaded — nothing sent";

/// What an open compose buffer is addressed to — the ONE fork the shared editor
/// and key router branch on.
///
/// Modeled as an enum rather than as optional fields beside each other so the two
/// drafts cannot be half-built: a reply always has a session, a background launch
/// never does, and no state can claim both. Each variant carries exactly what its
/// own submit needs and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposeTarget {
    /// A quick reply to an EXISTING session (`Ctrl-R`).
    Reply {
        /// Stable `session_id` the reply is addressed to (the row that was
        /// selected when `Ctrl-R` opened compose) — STABLE-ID STATE, re-resolved
        /// to the authoritative `(cwd, session_id)` from inside the file at Send
        /// time and never trusted as a live path.
        session_id: String,
        /// Short agent-view job id to `claude stop` before sending — set when the
        /// target is a held (`done`/`needs input`) background agent that must be
        /// deregistered first (see [`crate::send::reply_gate`]). `None` for a
        /// plain in-place reply.
        stop_job: Option<String>,
    },
    /// A first prompt for a BRAND-NEW background agent (`Enter` on the new-session
    /// agent picker, or `Ctrl-N` itself when no agents are defined). There is no
    /// session id — claude mints one — so this variant structurally cannot pretend
    /// to address a row on the board.
    NewBackgroundAgent {
        /// The picker row's agent name, or `None` for its "default (no agent)"
        /// row. Nothing else about the pick is retained: the transcript's own
        /// `agent-setting` record is what answers "which agent was this?" later.
        agent: Option<String>,
    },
}

/// The open compose zone: what it is addressed to and the live editor buffer.
///
/// Modeled as explicit `App` state (a sibling of the other overlay states such
/// as `modal` and `pending_stop`) so the compose modal is a small, inspectable
/// piece of state.
pub struct ComposeState {
    /// What this draft is addressed to — a reply, or a new background agent.
    pub target: ComposeTarget,
    /// The multiline editor buffer. The ONLY `ratatui_textarea` value in the
    /// program outside this module's functions, shared by BOTH targets.
    pub textarea: TextArea<'static>,
}

impl ComposeState {
    /// Open a fresh compose buffer for `target`, configured for plain multiline
    /// input. The single constructor, so both drafts get an identically-configured
    /// editor and the widget setup lives in exactly one place.
    #[must_use]
    pub fn new(target: ComposeTarget) -> Self {
        let mut textarea = TextArea::default();
        // No current-line underline: the compose box is a plain multiline field,
        // not a code editor. Styled via a ratatui `Style` (TERMINAL-SAFE STYLING).
        textarea.set_cursor_line_style(ratatui::style::Style::default());
        // Soft-wrap long lines at word boundaries (grapheme fallback for a word
        // wider than the box) so a long sentence stays visible instead of scrolling
        // off to the right.
        textarea.set_wrap_mode(WrapMode::WordOrGlyph);
        Self { target, textarea }
    }

    /// Open a fresh REPLY buffer for `session_id`. `stop_job` carries the job id to
    /// stop first (the stop-then-reply path).
    #[must_use]
    pub fn new_reply(session_id: String, stop_job: Option<String>) -> Self {
        Self::new(ComposeTarget::Reply {
            session_id,
            stop_job,
        })
    }

    /// Open a fresh BACKGROUND-AGENT draft buffer for `agent` (`None` = the
    /// picker's default / no-agent row).
    #[must_use]
    pub fn new_background(agent: Option<String>) -> Self {
        Self::new(ComposeTarget::NewBackgroundAgent { agent })
    }

    /// How many SCREEN rows the draft currently occupies — its soft-wrapped height,
    /// which is what the auto-growing box has to be sized from.
    ///
    /// ASKED OF THE WIDGET, never modeled by the view. The editor word-wraps
    /// ([`WrapMode::WordOrGlyph`], set in [`ComposeState::new`]) and expands tabs to
    /// [`TextArea::tab_length`]; any second implementation of that in the renderer is
    /// a DIFFERENT function of the same text — a character-packing `ceil(width /
    /// inner)` model, for instance, always counts at or below word wrap, by more the
    /// longer the words — and the box then under-grows and the editor scrolls its own
    /// first row out of view. There is no public row-count API at the pinned `=0.9.2`
    /// (`screen_lines_count` is `pub(crate)`), so the count is PROBED through the
    /// public one: park a throwaway clone's cursor on the last character of the last
    /// logical line and read the screen row it landed on. [`CursorMove::Bottom`] and
    /// [`CursorMove::End`] are both DATA-line moves (they resolve a `DataCursor` and
    /// map it forward), so the pair lands on the last screen row of the last logical
    /// line whatever the wrap mode, and `row + 1` is the row COUNT.
    ///
    /// The clone is a WHOLE `TextArea` copy — the draft's lines, its undo history (50
    /// edits at the crate's default) and its already-built screen map — not a cheap
    /// handle. The two moves on it then RE-WRAP NOTHING: `move_cursor` only resolves
    /// the cursor against that copied map, which the editor rebuilds on an EDIT and on
    /// a RENDER whose area changed, never on a move. So the cost is one copy of a
    /// human-typed draft per frame, on the redraw path and only while the compose zone
    /// is open — affordable, not free. Do NOT "optimize" it into a shared cursor move:
    /// the probe must not disturb where the user's caret actually is.
    ///
    /// The logical line count is a FLOOR (`max`): a draft can never need fewer rows
    /// than it has lines, so a probe that ever degrades still cannot report a box
    /// shorter than the un-wrapped text.
    ///
    /// CAVEAT, by design: the widget builds its screen map from the width it was LAST
    /// RENDERED at, so a terminal resize (or a splitter drag) leaves this one frame
    /// stale. Edits refresh the map immediately, and the next redraw — which the
    /// resize itself triggers — self-corrects, so the box settles a frame later rather
    /// than wrongly. Before the editor has EVER been rendered its area is still zero
    /// wide and the map is built unwrapped, so this reports logical lines; the draft
    /// is empty on that frame, which is exactly one row either way.
    #[must_use]
    pub fn screen_rows(&self) -> usize {
        let mut probe = self.textarea.clone();
        probe.move_cursor(CursorMove::Bottom);
        probe.move_cursor(CursorMove::End);
        let probed = probe.screen_cursor().row.saturating_add(1);
        probed.max(self.textarea.lines().len())
    }
}

/// A decoded intent from one keypress while the compose zone owns the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeAction {
    /// Submit the buffer (bare `Enter`) — send a reply, or launch the background
    /// agent, depending on the open [`ComposeTarget`].
    Send,
    /// Insert a newline (`Ctrl-J` primary, `Alt+Enter` guaranteed fallback,
    /// `Shift+Enter` opportunistic — see [`compose_key_to_action`]).
    Newline,
    /// Forward the keystroke to the text editor (ordinary typing / editing).
    Forward,
    /// Cancel compose and return to the board (`Esc`).
    Cancel,
    /// Run the draft INTERACTIVELY instead of in the background (`Ctrl-O`).
    ///
    /// Only [`ComposeTarget::NewBackgroundAgent`] acts on this; on a
    /// [`ComposeTarget::Reply`] it is INERT (there is no interactive launch to
    /// escape to — a reply addresses a session that already exists). The decision
    /// stays target-free here so the pure router remains a plain key → intent map;
    /// [`handle_compose_key`] is where the target decides whether the intent is
    /// actionable. `Ctrl-O` is unbound in `ratatui_textarea`, so claiming it costs
    /// the reply editor nothing it previously did.
    OpenInteractive,
}

/// Map a keypress to a [`ComposeAction`]. PURE and free of any `TextArea`
/// reference, so it is unit-testable exactly like [`super::update::key_to_action`].
///
/// The chords own the two directions `ratatui_textarea` cannot separate for us —
/// its `input`/`input_without_shortcuts` both treat `Enter` as a newline — so the
/// router intercepts `Enter` (Send) and the newline chords BEFORE anything reaches
/// the widget, and forwards only the remainder:
///
/// * `Enter` (no modifier) → **Send**.
/// * `Ctrl-J` → **Newline** (primary). In raw mode — always, for the TUI —
///   crossterm 0.29 delivers `Ctrl-J` as `Char('j')`+`CONTROL`: the `\n`/0x0A byte
///   skips the `!is_raw_mode_enabled()` `Enter` arm and falls to the
///   `0x01..=0x1A` control-char arm (`0x0A - 0x01 + b'a' == 'j'`); crossterm's own
///   Issue-#371 comment documents this. Both `'j'` and `'J'` are matched for the
///   kitty path's sake.
/// * `Alt+Enter` → **Newline** (GUARANTEED fallback; needs no keyboard protocol).
/// * `Shift+Enter` → **Newline** (opportunistic). On the legacy path `Shift+Enter`
///   arrives as a bare `Enter` (indistinguishable), so this only fires if the
///   terminal INDEPENDENTLY reports `Enter`+`SHIFT` (e.g. a kitty protocol the user
///   enabled). snapback enables NO kitty protocol (AGENTS.md TERMINAL SAFETY treats
///   a leftover level as corruption), so under its own setup this arm is dead and
///   `Alt+Enter` remains the guaranteed newline.
/// * `Ctrl-O` → **OpenInteractive** (the background draft's escape hatch; inert on
///   a reply — see [`ComposeAction::OpenInteractive`]).
/// * `Esc` → **Cancel** (dismiss compose, not the app).
/// * everything else → **Forward** to the editor.
#[must_use]
pub fn compose_key_to_action(key: KeyEvent) -> ComposeAction {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Char('j' | 'J') if ctrl => ComposeAction::Newline,
        KeyCode::Char('o' | 'O') if ctrl => ComposeAction::OpenInteractive,
        KeyCode::Enter if alt || shift => ComposeAction::Newline,
        KeyCode::Enter => ComposeAction::Send,
        KeyCode::Esc => ComposeAction::Cancel,
        _ => ComposeAction::Forward,
    }
}

/// Open the REPLY compose zone for `session_id`, FORCE-SHOWING the preview (the
/// compose zone docks in the preview pane, or falls back to a full-width bottom
/// bar on a short terminal — the renderer decides). `stop_job` is the job id to
/// `claude stop` before sending, or `None` for a plain in-place reply. The reply
/// gate (and, for a waiting agent, the stop confirmation) has already run at the
/// call site (`Ctrl-R` in `update`).
pub fn open(app: &mut App, session_id: String, stop_job: Option<String>) {
    // No draft card: a reply previews the REAL session it is addressed to.
    app.open_compose(ComposeState::new_reply(session_id, stop_job), None);
}

/// Open the BACKGROUND-AGENT draft pane for `agent` (`None` = the picker's
/// "default (no agent)" row), FORCE-SHOWING the preview exactly like [`open`].
///
/// The DEFAULT destination of `Ctrl-N`, reached two ways (`update`): the agent
/// picker's `Enter` confirm, which has already closed the picker — the draft pane
/// replaces it as the keyboard owner — and the no-agent fast path, which had no
/// picker to close.
///
/// Installs the editor AND the pane-level [`NewSessionDraft`] card together, so
/// the preview shows a PLACEHOLDER for the session about to exist rather than the
/// transcript of whichever row happened to be selected.
pub fn open_background(app: &mut App, agent: Option<String>) {
    app.open_compose(
        ComposeState::new_background(agent.clone()),
        Some(NewSessionDraft {
            agent,
            launch_id: None,
        }),
    );
}

/// Apply one keypress while the compose zone owns the keyboard.
///
/// Newline inserts into the editor; Forward hands the keystroke to the editor's
/// FULL key handler ([`TextArea::input`]); Cancel clears the compose state; Send
/// resolves the buffer through [`submit_compose`] (a reply send, or a background
/// launch) and OpenInteractive escalates a background draft to the interactive
/// hand-off.
///
/// Forward uses `input` (not `input_without_shortcuts`) so arrows/Home/End actually
/// MOVE the caret and word-delete works — `input_without_shortcuts` handles only
/// insert/delete of single characters, so with it the caret cannot move at all.
/// Using the full handler is safe because the router has already claimed the keys
/// with editor meaning we override: `Enter` (Send) and the newline chords never
/// reach `input`, so it never mistakes `Enter` for a newline.
pub fn handle_compose_key(app: &mut App, key: KeyEvent) -> Outcome {
    match compose_key_to_action(key) {
        ComposeAction::Newline => {
            if let Some(compose) = app.compose.as_mut() {
                compose.textarea.insert_newline();
            }
            Outcome::Continue
        }
        ComposeAction::Forward => {
            if let Some(compose) = app.compose.as_mut() {
                compose.textarea.input(key);
            }
            Outcome::Continue
        }
        ComposeAction::Cancel => {
            // Esc drops the editor AND the draft card together — one teardown, so a
            // cancelled draft can never leave a placeholder pane behind.
            app.close_compose();
            Outcome::Continue
        }
        ComposeAction::Send => submit_compose(app),
        ComposeAction::OpenInteractive => open_interactive(app),
    }
}

/// Insert a terminal PASTE into the open draft, at the caret.
///
/// Lives here, next to [`handle_compose_key`], because this module owns every
/// `ratatui_textarea` reference — and because the insert is the whole fix. A paste
/// is TEXT, so it goes in through [`TextArea::insert_str`], which splits the string
/// on `\n` and inserts a real multi-line chunk. Routing it as keystrokes instead is
/// what made an ordinary Cmd+V destructive: the router maps a bare `Enter` to
/// [`ComposeAction::Send`], so the first embedded newline SENT the draft's first
/// line and dropped the rest onto the board.
///
/// There is deliberately no [`Outcome`] here. Every compose exit
/// (Send / OpenInteractive / Cancel) is reachable only from
/// [`compose_key_to_action`], so a paste STRUCTURALLY cannot submit, launch, or
/// close the draft — it can only edit the buffer. `text` is already normalized and
/// capped by `update::accept_paste`; a no-op on an empty string, and on a paste that
/// arrives with no draft open (the caller checks, but the `if let` keeps this total).
pub fn insert_paste(app: &mut App, text: &str) {
    if let Some(compose) = app.compose.as_mut() {
        compose.textarea.insert_str(text);
    }
}

/// Read the open draft's `(text, target)` out of the app, ending the borrow before
/// anything mutates it — the clone-then-mutate discipline every other handler here
/// follows.
fn draft(app: &App) -> Option<(String, ComposeTarget)> {
    let compose = app.compose.as_ref()?;
    Some((compose.textarea.lines().join("\n"), compose.target.clone()))
}

/// Resolve the compose buffer into a driver [`Outcome`], routing on the open
/// [`ComposeTarget`]: a reply sends, a background draft launches.
fn submit_compose(app: &mut App) -> Outcome {
    match draft(app) {
        Some((
            message,
            ComposeTarget::Reply {
                session_id,
                stop_job,
            },
        )) => submit_reply(app, message, session_id, stop_job),
        Some((message, ComposeTarget::NewBackgroundAgent { agent })) => {
            submit_bg_launch(app, message, agent)
        }
        None => Outcome::Continue,
    }
}

/// Launch the drafted background agent — the `Enter` half of the draft pane.
///
/// It rides the [`crate::send`] family, NOT [`Outcome::Resume`]: there is no
/// terminal teardown, so the board stays up and the result arrives as an
/// [`AppEvent::BgLaunchFinished`](crate::watch::AppEvent::BgLaunchFinished).
/// An empty/whitespace draft keeps the pane open with a gentle nudge (a background
/// agent with no prompt would do nothing); otherwise the launch dir is gated
/// ([`send::plan_bg_launch`]), the argv built, the in-flight status set, and a
/// [`BgLaunchRequest`] handed to the driver.
///
/// The ONE thing recorded is the AGENT, as the pick `Ctrl-N`'s picker pre-highlights
/// next time — because this is a real launch, and that memory means "the agent of
/// the last new session actually started". It is written past the empty-buffer nudge
/// (a draft that launched nothing is not a start) and before the launch-dir gate, so
/// it survives a refusal, exactly like the picker's `Ctrl-O`.
///
/// Nothing ELSE is recorded about the launch: no virtual/pending row is created and
/// no attempt is made to reconcile the short job id back to a `sessionId`. The new
/// agent reaches the board through the existing watcher → reload path, and its own
/// transcript already records which agent it is.
fn submit_bg_launch(app: &mut App, message: String, agent: Option<String>) -> Outcome {
    if message.trim().is_empty() {
        // Nothing to run: keep the draft pane open so the user can type.
        app.set_status(COMPOSE_EMPTY_BG_HINT);
        return Outcome::Continue;
    }
    app.set_last_new_agent(agent.clone());
    match send::plan_bg_launch(&app.launch_dir) {
        Ok(cwd) => {
            let argv = send::build_bg_launch_argv(agent.as_deref(), &message);
            // The editor closes but the CARD stays, marked in flight: there is
            // nothing left to type, yet still no session to preview, so the
            // placeholder reports the launch until THIS launch's `BgLaunchFinished`
            // ends it. The id the card is stamped with rides out on the request so
            // the completion can be matched back to it rather than to whatever
            // surface is open by then.
            let launch_id = app.dispatch_draft();
            app.set_status(send::BG_LAUNCH_IN_FLIGHT);
            Outcome::BgLaunch(BgLaunchRequest {
                launch_id,
                argv,
                cwd,
            })
        }
        Err(refusal) => {
            // Nothing was dispatched, so there is nothing for a card to report.
            app.close_compose();
            app.set_status(refusal);
            Outcome::Continue
        }
    }
}

/// Run the drafted background agent INTERACTIVELY instead (`Ctrl-O`) — the one
/// action in the compose zone that leaves the board.
///
/// Unlike `Enter` this is an ordinary hand-off, and it builds NO argv of its own:
/// it delegates to [`super::update::launch_new_session`] — the same seam the
/// picker's OWN `Ctrl-O` uses — which runs [`crate::resume::check_new`] over the
/// existing `SessionAction::New` / `HandoffCtx` / `argv_for` machinery. The draft
/// becomes `claude [--agent <name>] <prompt>` through the IDENTICAL teardown →
/// spawn → wait → return round trip as every other `Outcome::Resume`. An EMPTY
/// draft launches bare (no positional), i.e. exactly what the picker's `Ctrl-O`
/// emits.
///
/// That shared seam is the point of the shared key: `Ctrl-O` means "open
/// interactive claude" on the picker and in the draft alike, so a user who wants
/// the terminal pays one keypress either way.
///
/// This IS a launch — bare draft or not — so it records the agent as the last new
/// session started, the same memory the picker's `Ctrl-O` and the `--bg` submit
/// write. Only real launches do; a draft that is opened and cancelled leaves it
/// alone.
///
/// The prompt AUTO-SUBMITS as the session's first turn — the trailing positional is
/// the only mechanism claude offers (see [`crate::resume::build_new_argv`]), which
/// is why every user-facing string for this key says "run interactively" and never
/// promises a chance to review or edit it inside claude.
///
/// INERT on a [`ComposeTarget::Reply`]: a reply addresses a session that already
/// exists, so there is no new-session launch to escape to.
fn open_interactive(app: &mut App) -> Outcome {
    let Some((message, ComposeTarget::NewBackgroundAgent { agent })) = draft(app) else {
        return Outcome::Continue; // no interactive launch on the reply target
    };
    // An empty / whitespace draft launches BARE — no positional at all — which is
    // exactly what the picker's own `Ctrl-O` emits.
    let prompt = (!message.trim().is_empty()).then_some(message);
    // The board is about to be torn down for the interactive child, so the card has
    // nothing left to report: close the whole surface.
    app.close_compose();
    app.set_last_new_agent(agent.clone());
    super::update::launch_new_session(app, agent.as_deref(), prompt.as_deref())
}

/// Send the drafted quick reply — the `Enter` half of the reply target.
///
/// Guards an empty/whitespace buffer (keep composing, gentle status). Otherwise it
/// re-reads the AUTHORITATIVE `(cwd, session_id)` from inside the file
/// ([`send::plan_send`]) — never the stale in-memory copy — builds the argv, sets
/// the "sending…" status, clears the compose state, and hands a [`SendRequest`] to
/// the driver as [`Outcome::Send`]. A refusal (deleted worktree / unreadable file)
/// sets a board status and stays on the board.
fn submit_reply(
    app: &mut App,
    message: String,
    session_id: String,
    stop_job: Option<String>,
) -> Outcome {
    if message.trim().is_empty() {
        // Nothing to send: keep the compose zone open so the user can type.
        app.set_status(COMPOSE_EMPTY_HINT);
        return Outcome::Continue;
    }

    let (file, baseline_msg_count) = match app.session_by_id(&session_id) {
        Some(session) => (session.file.clone(), session.msg_count),
        None => {
            app.close_compose();
            app.set_status(COMPOSE_SESSION_GONE);
            return Outcome::Continue;
        }
    };

    match send::plan_send(&file) {
        SendPlan::Ready {
            cwd,
            session_id: authoritative_id,
        } => {
            let argv = send::build_send_argv(&authoritative_id, &message);
            app.close_compose();
            app.set_status(send::SEND_IN_FLIGHT);
            // Mark the send in flight so the preview echoes the message under a
            // synthetic `▶ you` turn plus a live "sending… / cooking…" indicator
            // until the completion event lands. `baseline_msg_count` lets the echo
            // step aside the instant claude writes the real turn to disk.
            app.sending = Some(super::app::Sending {
                session_id: authoritative_id.clone(),
                message,
                baseline_msg_count,
            });
            Outcome::Send(SendRequest {
                argv,
                cwd,
                session_id: authoritative_id,
                stop_job,
            })
        }
        SendPlan::Refuse(message) => {
            app.close_compose();
            app.set_status(message);
            Outcome::Continue
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rows the tests below draw the editor into. The widget's screen map — and so
    /// [`ComposeState::screen_rows`] — is built from the drawn WIDTH alone, never the
    /// height, so one row seeds everything these assertions need and keeps the
    /// scratch buffer tiny.
    const PROBE_RENDER_ROWS: u16 = 1;

    /// Draw `state`'s editor once at `width` columns, exactly as `render_compose_zone`
    /// does, so its screen map is built at a REAL width.
    ///
    /// Load-bearing setup, not ceremony: until the widget has been rendered its area
    /// is zero wide, the screen map is built with no wrapping at all, and every wrap
    /// assertion below would pass vacuously against the logical line count.
    fn draw_editor_at(state: &ComposeState, width: u16) {
        let area = ratatui::layout::Rect {
            x: 0,
            y: 0,
            width,
            height: PROBE_RENDER_ROWS,
        };
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::Widget::render(&state.textarea, area, &mut buffer);
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn with_mods(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    /// A bare `Enter` sends — the whole reason the router owns `Enter` rather than
    /// forwarding it to the widget (which would insert a newline).
    #[test]
    fn bare_enter_sends() {
        assert_eq!(
            compose_key_to_action(key(KeyCode::Enter)),
            ComposeAction::Send
        );
    }

    /// `Ctrl-J` is the primary newline chord — and the form crossterm 0.29
    /// actually delivers it as in raw mode: `Char('j')`+`CONTROL`.
    #[test]
    fn ctrl_j_inserts_a_newline() {
        assert_eq!(
            compose_key_to_action(with_mods(KeyCode::Char('j'), KeyModifiers::CONTROL)),
            ComposeAction::Newline,
        );
        // Uppercase 'J' too, for the kitty path's sake.
        assert_eq!(
            compose_key_to_action(with_mods(KeyCode::Char('J'), KeyModifiers::CONTROL)),
            ComposeAction::Newline,
        );
    }

    /// `Alt+Enter` is the GUARANTEED newline fallback (needs no keyboard protocol).
    #[test]
    fn alt_enter_inserts_a_newline() {
        assert_eq!(
            compose_key_to_action(with_mods(KeyCode::Enter, KeyModifiers::ALT)),
            ComposeAction::Newline,
        );
    }

    /// `Shift+Enter` is honored opportunistically: if a terminal reports it
    /// distinctly (a kitty protocol the user enabled), it inserts a newline. We
    /// enable no such protocol ourselves, so under snapback's own setup this arm
    /// never fires — but wiring it is free and correct where it IS delivered.
    #[test]
    fn shift_enter_inserts_a_newline_when_delivered_distinctly() {
        assert_eq!(
            compose_key_to_action(with_mods(KeyCode::Enter, KeyModifiers::SHIFT)),
            ComposeAction::Newline,
        );
    }

    /// `Esc` cancels compose — never the app (the app-level `Esc`-quits binding is
    /// bypassed while the compose zone owns the keyboard).
    #[test]
    fn esc_cancels() {
        assert_eq!(
            compose_key_to_action(key(KeyCode::Esc)),
            ComposeAction::Cancel
        );
    }

    /// A plain character is ordinary typing — forwarded to the editor, NOT any
    /// chord. In particular a bare `j` (no Ctrl) types a `j` rather than a newline.
    #[test]
    fn a_plain_char_is_forwarded() {
        assert_eq!(
            compose_key_to_action(key(KeyCode::Char('a'))),
            ComposeAction::Forward
        );
        assert_eq!(
            compose_key_to_action(key(KeyCode::Char('j'))),
            ComposeAction::Forward,
            "a bare `j` types a `j`; only Ctrl-J is the newline chord"
        );
    }

    /// Editing keys (Backspace, arrows) forward to the widget, which owns cursor
    /// movement and deletion.
    #[test]
    fn editing_keys_are_forwarded() {
        for code in [
            KeyCode::Backspace,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Home,
            KeyCode::End,
        ] {
            assert_eq!(
                compose_key_to_action(key(code)),
                ComposeAction::Forward,
                "{code:?} must forward to the editor"
            );
        }
    }

    /// `Ctrl-O` decodes to the interactive escape hatch, alongside the existing
    /// chords. A bare `o` is still ordinary typing — only the Ctrl form is claimed.
    #[test]
    fn ctrl_o_runs_interactively() {
        assert_eq!(
            compose_key_to_action(with_mods(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            ComposeAction::OpenInteractive,
        );
        // Uppercase 'O' too, for the kitty path's sake (like the Ctrl-J arm).
        assert_eq!(
            compose_key_to_action(with_mods(KeyCode::Char('O'), KeyModifiers::CONTROL)),
            ComposeAction::OpenInteractive,
        );
        assert_eq!(
            compose_key_to_action(key(KeyCode::Char('o'))),
            ComposeAction::Forward,
            "a bare `o` types an `o`; only Ctrl-O is the interactive chord"
        );
    }

    /// The target enum is the ONLY thing that distinguishes the two drafts, and it
    /// makes the reply-shaped fields STRUCTURALLY absent from a background draft:
    /// there is no session id (claude has not minted one) and no stop job (nothing
    /// is being resumed, so nothing needs deregistering), rather than `None`s that
    /// a future edit could quietly start filling in.
    #[test]
    fn each_constructor_builds_only_its_own_target() {
        assert_eq!(
            ComposeState::new_reply("sess-1".to_string(), Some("job-1".to_string())).target,
            ComposeTarget::Reply {
                session_id: "sess-1".to_string(),
                stop_job: Some("job-1".to_string()),
            }
        );
        assert_eq!(
            ComposeState::new_reply("sess-1".to_string(), None).target,
            ComposeTarget::Reply {
                session_id: "sess-1".to_string(),
                stop_job: None,
            },
            "a plain reply carries no stop job"
        );
        assert_eq!(
            ComposeState::new_background(Some("planner".to_string())).target,
            ComposeTarget::NewBackgroundAgent {
                agent: Some("planner".to_string())
            }
        );
        // The picker's "default (no agent)" row carries `None`, not a blank name.
        assert_eq!(
            ComposeState::new_background(None).target,
            ComposeTarget::NewBackgroundAgent { agent: None }
        );
    }

    /// An empty draft occupies exactly one screen row — the height the compose box
    /// opens at.
    #[test]
    fn screen_rows_is_one_for_an_empty_draft() {
        let state = ComposeState::new_background(None);
        draw_editor_at(&state, 40);
        assert_eq!(state.screen_rows(), 1);
    }

    /// Every logical line costs a row, wrapping or not — the floor the probe can
    /// never report below.
    #[test]
    fn screen_rows_counts_every_logical_line() {
        let mut state = ComposeState::new_background(None);
        state.textarea.insert_str("one");
        state.textarea.insert_newline();
        state.textarea.insert_str("two");
        state.textarea.insert_newline();
        state.textarea.insert_str("three");
        // Wide enough that nothing here wraps, so only the line count is in play.
        draw_editor_at(&state, 40);
        assert_eq!(state.screen_rows(), 3);

        // A trailing newline is a real (empty) line and gets its own row.
        state.textarea.insert_newline();
        draw_editor_at(&state, 40);
        assert_eq!(state.screen_rows(), 4);
    }

    /// The load-bearing one: the editor WORD-wraps, so a draft of medium words in a
    /// narrow box needs MORE rows than the character-packing `ceil(width / inner)`
    /// model the view used to apply — each row ends early at a word boundary and
    /// leaves its tail unused.
    ///
    /// The gap is exactly the bug this accessor exists to close: sized by the ceil
    /// model the box grew to 3 rows while the editor needed 4, so the editor scrolled
    /// its own first row out of view.
    #[test]
    fn screen_rows_counts_word_wrapped_rows_not_packed_characters() {
        // 4 x 6-char words + 3 spaces = 27 columns, in a 10-column editor.
        let draft = "abcdef abcdef abcdef abcdef";
        let editor_width = 10u16;

        let mut state = ComposeState::new_background(None);
        state.textarea.insert_str(draft);
        draw_editor_at(&state, editor_width);

        // Word wrap fits ONE word plus its space per row: 4 rows.
        assert_eq!(state.screen_rows(), 4);
        // What the old model said: ceil(27 / 10) = 3 — one row short of the truth.
        let packed = draft.len().div_ceil(usize::from(editor_width));
        assert_eq!(packed, 3);
        assert!(
            state.screen_rows() > packed,
            "word wrap must out-count character packing here, or this pins nothing"
        );
    }

    /// A long UNBROKEN word still wraps (the `WordOrGlyph` grapheme fallback), so a
    /// draft with no spaces at all is measured too rather than reported as one row.
    #[test]
    fn screen_rows_counts_a_word_too_long_to_break() {
        let mut state = ComposeState::new_background(None);
        state.textarea.insert_str("x".repeat(25));
        draw_editor_at(&state, 10);
        assert_eq!(state.screen_rows(), 3, "25 columns at width 10 is 3 rows");
    }

    /// Documented degradation, pinned so it stays a known shape rather than a
    /// surprise: before the editor has EVER been drawn its area is zero wide, the
    /// widget wraps nothing, and the probe falls back to the logical line count.
    /// Harmless in the board's own timing (the draft is empty on that one frame) and
    /// the next redraw self-corrects.
    #[test]
    fn screen_rows_degrades_to_logical_lines_before_the_editor_is_ever_drawn() {
        let mut state = ComposeState::new_background(None);
        state.textarea.insert_str("abcdef abcdef abcdef abcdef");
        // Deliberately NOT drawn.
        assert_eq!(state.screen_rows(), 1);
    }

    /// The probe must not move the user's caret: it runs on a throwaway clone, so the
    /// real cursor is exactly where it was.
    #[test]
    fn screen_rows_leaves_the_editors_own_cursor_alone() {
        let mut state = ComposeState::new_background(None);
        state.textarea.insert_str("alpha");
        state.textarea.insert_newline();
        state.textarea.insert_str("bravo");
        state.textarea.move_cursor(CursorMove::Top);
        draw_editor_at(&state, 40);

        let before = state.textarea.cursor();
        let _ = state.screen_rows();
        assert_eq!(
            state.textarea.cursor(),
            before,
            "measuring the draft must not relocate the caret"
        );
    }
}
