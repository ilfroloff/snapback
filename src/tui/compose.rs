//! The quick-reply compose zone: multiline input + its key dispatch.
//!
//! This module OWNS the `ratatui_textarea` dependency the way `search.rs` owns
//! nucleo: every reference to the text-editor widget lives here (plus the
//! `App::compose` field, whose type is [`ComposeState`]). The compose zone is a
//! modal — while it is open it owns the keyboard, exactly like the running-session
//! and agent-pick overlays — opened by `Ctrl-R` on an idle session and closed by
//! sending (`Enter`) or cancelling (`Esc`).
//!
//! The pure DECISION is [`compose_key_to_action`], unit-tested like
//! [`super::update::key_to_action`] and free of any `TextArea` reference. The
//! impure edits (insert a newline, forward a keystroke to the widget) and the send
//! hand-off live in [`handle_compose_key`], a thin driver over that decision and
//! over the pure send core in [`crate::send`].

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_textarea::{TextArea, WrapMode};
use unicode_width::UnicodeWidthStr;

use crate::send::{self, SendPlan, SendRequest};

use super::app::App;
use super::update::Outcome;

/// Status shown when Send is pressed on an empty / whitespace-only buffer: the
/// compose zone stays open (nothing was sent), so this is a gentle nudge rather
/// than a refusal.
const COMPOSE_EMPTY_HINT: &str = "nothing to send — type a message first";

/// Status when the composed session vanished from the store between opening the
/// compose zone and pressing Send (e.g. its file was removed).
const COMPOSE_SESSION_GONE: &str = "that session is no longer loaded — nothing sent";

/// The open compose zone: which session it targets and the live editor buffer.
///
/// Modeled as explicit `App` state (a sibling of the other overlay states such
/// as `modal` and `pending_stop`) so the compose modal is a small, inspectable
/// piece of state. The target is a
/// stable `session_id` (STABLE-ID STATE), re-resolved to the authoritative
/// `(cwd, session_id)` from inside the file at Send time — never trusted as a
/// live path.
pub struct ComposeState {
    /// Stable `session_id` the reply is addressed to (the row that was selected
    /// when `Ctrl-R` opened compose).
    pub target_session_id: String,
    /// The multiline editor buffer. The ONLY `ratatui_textarea` value in the
    /// program outside this module's functions.
    pub textarea: TextArea<'static>,
    /// Short agent-view job id to `claude stop` before sending — set when the target
    /// is a held (`done`/`needs input`) background agent that must be deregistered
    /// first (see [`crate::send::reply_gate`]). `None` for a plain in-place reply.
    pub stop_job: Option<String>,
}

impl ComposeState {
    /// Open a fresh compose buffer for `session_id`, configured for plain multiline
    /// input. `stop_job` carries the job id to stop first (the stop-then-reply path).
    #[must_use]
    pub fn new(session_id: String, stop_job: Option<String>) -> Self {
        let mut textarea = TextArea::default();
        // No current-line underline: the compose box is a plain multiline field,
        // not a code editor. Styled via a ratatui `Style` (TERMINAL-SAFE STYLING).
        textarea.set_cursor_line_style(ratatui::style::Style::default());
        // Soft-wrap long lines at word boundaries (grapheme fallback for a word
        // wider than the box) so a long sentence stays visible instead of scrolling
        // off to the right.
        textarea.set_wrap_mode(WrapMode::WordOrGlyph);
        Self {
            target_session_id: session_id,
            textarea,
            stop_job,
        }
    }

    /// The display width (terminal columns) of each logical line of the draft.
    ///
    /// The renderer feeds these to its shared wrap model to size the auto-growing
    /// box, so the width math lives with the editor rather than reaching into the
    /// `TextArea` from the view. `unicode-width` so wide/CJK glyphs count correctly.
    #[must_use]
    pub fn content_line_widths(&self) -> Vec<usize> {
        self.textarea
            .lines()
            .iter()
            .map(|line| UnicodeWidthStr::width(line.as_str()))
            .collect()
    }
}

/// A decoded intent from one keypress while the compose zone owns the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeAction {
    /// Send the buffer (bare `Enter`).
    Send,
    /// Insert a newline (`Ctrl-J` primary, `Alt+Enter` guaranteed fallback,
    /// `Shift+Enter` opportunistic — see [`compose_key_to_action`]).
    Newline,
    /// Forward the keystroke to the text editor (ordinary typing / editing).
    Forward,
    /// Cancel compose and return to the board (`Esc`).
    Cancel,
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
/// * `Esc` → **Cancel** (dismiss compose, not the app).
/// * everything else → **Forward** to the editor.
#[must_use]
pub fn compose_key_to_action(key: KeyEvent) -> ComposeAction {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Char('j' | 'J') if ctrl => ComposeAction::Newline,
        KeyCode::Enter if alt || shift => ComposeAction::Newline,
        KeyCode::Enter => ComposeAction::Send,
        KeyCode::Esc => ComposeAction::Cancel,
        _ => ComposeAction::Forward,
    }
}

/// Open the compose zone for `session_id`, FORCE-SHOWING the preview (the compose
/// zone docks in the preview pane, or falls back to a full-width bottom bar on a
/// short terminal — the renderer decides). `stop_job` is the job id to `claude
/// stop` before sending, or `None` for a plain in-place reply. The reply gate (and,
/// for a waiting agent, the stop confirmation) has already run at the call site
/// (`Ctrl-R` in `update`).
pub fn open(app: &mut App, session_id: String, stop_job: Option<String>) {
    // Composing docks in the preview pane, so it must be visible; the renderer
    // falls back to a full-width bottom bar when the pane is too short.
    app.show_preview = true;
    app.compose = Some(ComposeState::new(session_id, stop_job));
}

/// Apply one keypress while the compose zone owns the keyboard.
///
/// Newline inserts into the editor; Forward hands the keystroke to the editor's
/// FULL key handler ([`TextArea::input`]); Cancel clears the compose state; Send
/// resolves the buffer into a [`SendRequest`] via the pure send core and returns
/// [`Outcome::Send`] for the driver to spawn — the board stays up throughout.
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
            app.compose = None;
            Outcome::Continue
        }
        ComposeAction::Send => submit_compose(app),
    }
}

/// Resolve the compose buffer into a driver [`Outcome`].
///
/// Guards an empty/whitespace buffer (keep composing, gentle status). Otherwise it
/// re-reads the AUTHORITATIVE `(cwd, session_id)` from inside the file
/// ([`send::plan_send`]) — never the stale in-memory copy — builds the argv, sets
/// the "sending…" status, clears the compose state, and hands a [`SendRequest`] to
/// the driver as [`Outcome::Send`]. A refusal (deleted worktree / unreadable file)
/// sets a board status and stays on the board. All borrows are cloned out before
/// the app is mutated.
fn submit_compose(app: &mut App) -> Outcome {
    let (message, session_id, stop_job) = match app.compose.as_ref() {
        Some(compose) => (
            compose.textarea.lines().join("\n"),
            compose.target_session_id.clone(),
            compose.stop_job.clone(),
        ),
        None => return Outcome::Continue,
    };

    if message.trim().is_empty() {
        // Nothing to send: keep the compose zone open so the user can type.
        app.set_status(COMPOSE_EMPTY_HINT);
        return Outcome::Continue;
    }

    let (file, baseline_msg_count) = match app.session_by_id(&session_id) {
        Some(session) => (session.file.clone(), session.msg_count),
        None => {
            app.compose = None;
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
            app.compose = None;
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
            app.compose = None;
            app.set_status(message);
            Outcome::Continue
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
