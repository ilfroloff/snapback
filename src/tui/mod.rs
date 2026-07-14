//! Terminal UI shell (ratatui).
//!
//! The elm-style presentation layer: `app` holds the `App` model, `update`
//! runs the event loop (Input / SessionsChanged / Tick), and `view` renders the
//! two-pane grouped list + preview. This module also owns terminal
//! setup/teardown ([`init_terminal`] / [`restore_terminal`], including a panic
//! hook) so a crash never leaves the terminal broken, and drives the render +
//! event loop in [`run`].

pub mod app;
pub mod update;
pub mod view;

use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::style::{Attribute, SetAttribute};
use crossterm::terminal::{
    enable_raw_mode, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::DefaultTerminal;

use crate::watch::EventLoop;

pub use app::{App, Scope};
pub use update::Outcome;

/// Cadence of the periodic redraw tick that drives autorefresh visibility.
const TICK: Duration = crate::watch::TICK;

/// Enter the alternate screen, enable raw mode, turn on mouse capture (for
/// wheel/trackpad scrolling), and install a panic hook that restores the
/// terminal — mouse mode included — before unwinding.
///
/// Built on `ratatui::try_init`, which performs raw mode, the alternate screen,
/// and a panic hook that calls `ratatui::restore`. That restore does NOT disable
/// mouse capture, so after `try_init` we WRAP the panic hook (see
/// [`install_mouse_safe_panic_hook`]) and only then enable mouse capture, so
/// EVERY exit — quit, error, resume hand-off, or panic — leaves mouse mode off.
/// The returned [`DefaultTerminal`] is a `CrosstermBackend` writing to stdout.
pub fn init_terminal() -> Result<DefaultTerminal> {
    let terminal = ratatui::try_init()?;
    // Wrap ratatui's (restore-only) panic hook BEFORE enabling mouse capture so
    // even a panic between here and the first draw disables mouse mode.
    install_mouse_safe_panic_hook();
    if let Err(err) = execute!(io::stdout(), EnableMouseCapture) {
        // A failed enable must not leak the raw mode / alt screen try_init set up.
        restore_terminal();
        return Err(err.into());
    }
    Ok(terminal)
}

/// Restore the terminal to its original state: disable mouse capture, disable
/// raw mode, and leave the alternate screen.
///
/// This is the teardown seam for the resume round trip: [`run`] calls it before
/// returning an [`Outcome::Resume`], so `claude` is spawned onto a clean,
/// non-raw terminal with mouse mode OFF, and the loop in `main` re-initializes
/// afterwards. Mouse capture is disabled FIRST (while still on the alt screen),
/// then `ratatui::restore` drops raw mode and the alt screen; both steps are
/// idempotent, so re-initializing each loop iteration is safe. Errors are
/// ignored — restoring on the way out is best-effort by design.
pub fn restore_terminal() {
    let _ = disable_mouse(&mut io::stdout());
    ratatui::restore();
}

/// Write crossterm's `DisableMouseCapture` sequence to `w` (flushed by
/// `execute!`). Factored out and generic over [`Write`] so the teardown escape
/// sequence can be asserted in a unit test without a real TTY.
fn disable_mouse<W: Write>(w: &mut W) -> io::Result<()> {
    execute!(w, DisableMouseCapture)
}

/// `CAN` (Cancel, byte `0x18`) — ECMA-48 §8.3.5. Received mid-sequence it
/// ABORTS the control sequence the terminal's escape parser is currently
/// assembling and returns the parser to ground state. A `claude` child that
/// exited mid-CSI (a `Ctrl-Z` dirty exit) can leave the parser expecting more
/// bytes; `CAN` discards that partial sequence. No crossterm typed command emits
/// it, so it is written as a raw control byte (see [`recover_parser_state`]).
const CAN: u8 = 0x18;

/// `ST` (String Terminator, `ESC \` — bytes `0x1b 0x5c`) — ECMA-48 §8.3.143.
/// Closes any open control STRING — OSC / DCS / SOS / PM / APC — that the child
/// opened but never terminated. A dangling DCS/OSC otherwise swallows subsequent
/// output as string content (the reported `[39m`-renders-as-literal-text
/// corruption). No crossterm typed command emits it, so it is written as raw
/// control bytes (see [`recover_parser_state`]).
const ST: [u8; 2] = [0x1b, 0x5c];

/// Recover the terminal's escape-sequence parser to ground state BEFORE any
/// board escape is written, healing a `claude` child that exited with the parser
/// stuck MID control-string. Emitted, in order:
///
/// 1. [`CAN`] (`0x18`) — abort an in-flight CSI/escape sequence.
/// 2. [`ST`] (`ESC \`) — terminate any pending OSC/DCS/SOS/PM/APC string.
/// 3. SGR reset — crossterm [`SetAttribute`]`(`[`Attribute::Reset`]`)` emits
///    `CSI 0 m` (`\x1b[0m`, verified against the pinned crossterm 0.29), clearing
///    leftover attributes/color the child set.
///
/// This raw-byte write is a JUSTIFIED, NARROW exception to AGENTS.md's
/// "TERMINAL-SAFE STYLING: never embed ANSI escapes" rule. That rule governs
/// presentation styling in `view.rs` / `preview.rs`, where a ratatui `Style` is
/// the right tool. Here we are in the terminal-MANAGEMENT layer doing parser
/// RECOVERY, and `CAN` / `ST` have NO crossterm typed-command equivalent — the
/// only way to emit them is the raw control byte. The SGR reset, which DOES have
/// a typed command, uses it rather than a literal escape. All three writes are
/// WRITE-ONLY (no cursor-position / DSR `CSI 6n` query), so the return leg still
/// never blocks reading a reply from a dirty child's stdin. They are also
/// harmless no-ops on a clean (non-stuck) terminal — `CAN` / `ST` in ground state
/// do nothing, and an SGR reset with nothing set is a no-op — so [`hard_reset`]
/// can prepend them UNCONDITIONALLY on every board (re)entry. Factored out and
/// generic over [`Write`] so the emitted bytes can be asserted in a unit test
/// without a real TTY (mirrors [`disable_mouse`] / [`reset_child_modes`] /
/// [`reassert_board_screen`]).
fn recover_parser_state<W: Write>(w: &mut W) -> io::Result<()> {
    // `CAN` and `ST` have no crossterm typed command — write the raw control
    // bytes. `CAN` aborts an in-flight CSI/escape; `ST` closes a dangling string.
    w.write_all(&[CAN])?;
    w.write_all(&ST)?;
    // SGR reset via the typed command (`CSI 0 m`).
    execute!(w, SetAttribute(Attribute::Reset))
}

/// Turn OFF the terminal modes a spawned `claude` child may have enabled but
/// that `snapback` never uses, so a child that exited on `Ctrl-Z` without
/// restoring, or exited abnormally, cannot leak them into the board.
///
/// Bracketed paste (`CSI ?2004`) and focus reporting (`CSI ?1004`) both inject
/// synthetic bytes into stdin while active (`ESC[200~…ESC[201~` around pastes,
/// `ESC[I` / `ESC[O` on focus changes); leaked into the board those corrupt its
/// input. [`init_terminal`] enables NEITHER, so disabling them is a safe no-op
/// when the child already cleaned up. Factored out and generic over [`Write`] so
/// the reset sequence can be asserted in a unit test without a real TTY (mirrors
/// [`disable_mouse`]).
fn reset_child_modes<W: Write>(w: &mut W) -> io::Result<()> {
    execute!(w, DisableBracketedPaste, DisableFocusChange)
}

/// Force the board back onto a FRESH alternate screen and clear stale content —
/// the visible screen AND the terminal's native SCROLLBACK — after a spawned
/// `claude` child returned. Every escape written here is WRITE-ONLY: none of them
/// query the cursor position (DSR `CSI 6n`), so this seam can never block reading
/// a reply from a dirty child's stdin — the reason the visible-screen clear lives
/// here rather than in [`ratatui::Terminal::clear`] (see [`hard_reset`]). Factored
/// out and generic over [`Write`] so the emitted escape sequence can be asserted
/// in a unit test without a real TTY (mirrors [`disable_mouse`] /
/// [`reset_child_modes`]).
///
/// Emitted, in order:
/// 1. [`LeaveAlternateScreen`] then [`EnterAlternateScreen`] — a bare
///    `EnterAlternateScreen` is a NO-OP when the emulator already believes it is
///    on the alt screen (a dirty child that entered `?1049h` and exited without
///    leaving it), so the buffer switch is FORCED to round-trip; landing on a
///    fresh alt buffer is what actually clears it. This is the minimal sequence
///    that reliably re-enters the alt screen — one brief switch to the main
///    screen and back, paid only on child return, never per frame.
/// 2. [`EnableMouseCapture`] — re-arm wheel/trackpad scrolling that the screen
///    round-trip (or the child) may have dropped.
/// 3. [`Clear`]`(`[`ClearType::All`]`)` — `CSI 2J`, erase the visible screen. A
///    WRITE-ONLY clear: unlike [`ratatui::Terminal::clear`] it emits `2J`
///    directly and never issues a cursor-position query, so it cannot block on a
///    stdin reply from a dirty child (see [`hard_reset`]). The matching
///    back-buffer reset is unnecessary here — [`init_terminal`] hands
///    [`run_inner`] a brand-new terminal whose first `draw` repaints every cell.
/// 4. [`Clear`]`(`[`ClearType::Purge`]`)` — `CSI 3J`, erase the emulator's saved
///    lines. This is load-bearing: without it the native scrollback bleeds
///    through the repainted board (the reported corruption).
fn reassert_board_screen<W: Write>(w: &mut W) -> io::Result<()> {
    execute!(
        w,
        LeaveAlternateScreen,
        EnterAlternateScreen,
        EnableMouseCapture,
        Clear(ClearType::All),
        Clear(ClearType::Purge),
    )
}

/// Re-assert a known-good board screen after returning from a spawned `claude`
/// child, healing a terminal the child left DIRTY. Every escape it emits is
/// WRITE-ONLY — it NEVER issues a cursor-position (DSR `CSI 6n`) query.
///
/// [`init_terminal`] re-runs on every board (re)entry and its
/// [`ratatui::try_init`] re-enables raw mode — [`restore_terminal`] cleared
/// crossterm's saved-mode global before the hand-off, so the next
/// [`enable_raw_mode`] re-applies it. What `try_init` CANNOT do is recover a dirty
/// hand-back: its `EnterAlternateScreen` is a NO-OP when the emulator already
/// believes it is on the alt screen, and ratatui's diff renderer only repaints
/// cells that differ from its (freshly built) in-memory buffer. The reported
/// failure is `Ctrl-Z` inside a resumed `claude` that exits without restoring the
/// terminal: the board returns CORRUPTED, with stale cells and the emulator's
/// native scrollback showing through.
///
/// The write-only discipline is deliberate and load-bearing. In ratatui 0.30
/// (`ratatui-core` 0.1.2) [`ratatui::Terminal::clear`] first calls
/// `backend.get_cursor_position()`, which emits DSR `CSI 6n` and BLOCKS reading
/// the reply from stdin. On a dirty child hand-back (a `claude` that exited on
/// `Ctrl-Z` without restoring the terminal) that reply is delayed or lost;
/// crossterm times out (~2s) and returns an error that propagated out of [`run`]
/// and CRASHED snapback with "The cursor position could not be read within a
/// normal duration". So the visible-screen clear is emitted as a WRITE-ONLY
/// `Clear(ClearType::All)` (`CSI 2J`) inside [`reassert_board_screen`] instead of
/// via `terminal.clear()`; `terminal.draw` with a `Viewport::Fullscreen` never
/// queries the cursor position, so this seam is the ONLY DSR source on the return
/// path — and it is now gone. The load-bearing steps here, in order:
///
/// 1. [`recover_parser_state`] — FIRST, before ANY board escape: emit `CAN` +
///    `ST` + an SGR reset to un-stick the terminal's escape parser if the child
///    exited MID control-string (a dangling DCS/OSC/CSI with no terminator). It
///    MUST come first: a stuck parser swallows the FIRST escapes it sees as
///    string content, so if the re-init escapes below ran ahead of it they would
///    be eaten and every downstream SGR code would render as literal text (the
///    reported leaked `[39m` that cascades over the whole board). The recovery
///    bytes are harmless no-ops on a clean terminal, so prepending them on every
///    board (re)entry is safe.
/// 2. [`enable_raw_mode`] — cheap, idempotent confirmation of raw mode (already
///    re-applied by `try_init`), so this seam's post-condition — alt screen +
///    raw mode + mouse capture — is self-contained rather than assumed.
/// 3. [`reset_child_modes`] — turn off input modes the child may have leaked
///    (bracketed paste `?2004l`, focus reporting `?1004l`).
/// 4. [`reassert_board_screen`] — round-trip `Leave`→`EnterAlternateScreen` to
///    force a FRESH alt buffer (defeating the no-op re-enter), re-arm mouse
///    capture, clear the visible screen with `Clear(ClearType::All)` (`CSI 2J`),
///    and purge the native SCROLLBACK with `Clear(ClearType::Purge)` (`CSI 3J`).
///
/// A write-only `2J` is sufficient — no back-buffer reset needed — because
/// [`init_terminal`] builds a BRAND-NEW [`DefaultTerminal`] with empty buffers on
/// EVERY board (re)entry, before [`run_inner`]: the first
/// [`ratatui::Terminal::draw`] after the physical `CSI 2J` already repaints every
/// non-default cell onto the cleared screen, so the back-buffer reset
/// `terminal.clear()` used to perform is REDUNDANT here (and was the sole reason
/// to pass the terminal in — hence this fn takes no arguments).
///
/// The `?1049` round-trip and the `3J` scrollback purge are the additions that
/// remove the "native scrollback showing through" corruption a bare
/// `terminal.clear()` (visible-screen `2J` only) left behind. Called at the top
/// of [`run_inner`] so every board (re)entry — in particular every return from
/// `resume::launch`, whatever the child's exit — starts from a clean screen.
/// Cheap and idempotent, so running it on the first board show too is harmless;
/// it does not touch [`restore_terminal`]'s teardown invariant.
fn hard_reset() -> Result<()> {
    let mut out = io::stdout();
    // FIRST, before any board escape is written: un-stick the parser if the
    // child left it mid control-string, so the re-init escapes below are parsed
    // as commands rather than swallowed as string content.
    recover_parser_state(&mut out)?;
    enable_raw_mode()?;
    reset_child_modes(&mut out)?;
    reassert_board_screen(&mut out)?;
    Ok(())
}

/// Wrap the current panic hook so a panic disables mouse capture before the
/// existing hook runs.
///
/// Called right after [`ratatui::try_init`], so the hook it wraps is ratatui's,
/// which restores raw mode + the main screen but leaves mouse capture ON. This
/// closes the last teardown path — a panic — that would otherwise leak mouse
/// mode into the user's shell. Disabling mouse is best-effort (errors ignored).
fn install_mouse_safe_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_mouse(&mut io::stdout());
        previous(info);
    }));
}

/// Run one board session: initialize the terminal, merge input+watcher+tick via
/// [`EventLoop`], and drive the elm-style draw/handle loop until the user quits
/// or confirms a resume. Restores the terminal before returning on EVERY exit,
/// whatever the outcome — success, or an error propagated out of the loop body.
///
/// Takes `&mut App` (rather than owning it) so the model — selection, query,
/// scope, scroll — survives across resume round trips: `main` calls [`run`] in a
/// loop, and on [`Outcome::Resume`] spawns `claude` as a child, waits, reloads
/// the store, and calls [`run`] again on the SAME `App`. [`Outcome::Quit`] ends
/// the loop; a refused resume never reaches `main` — it is surfaced as a board
/// status and the loop keeps drawing.
///
/// Restore is guaranteed by structure: once [`init_terminal`] has enabled raw
/// mode + the alternate screen, all further fallible work happens in
/// [`run_inner`], whose result is captured so [`restore_terminal`] ALWAYS runs
/// before returning. A bare `?` in the body would otherwise leave the terminal
/// in raw mode on the error path — `DefaultTerminal` does not restore on drop
/// and the panic hook only fires on panic, not on `Err`.
pub fn run(app: &mut App, root: &Path) -> Result<Outcome> {
    let mut terminal = init_terminal()?;
    // `init_terminal()?` failing needs no restore — nothing was enabled yet.
    // From here on the terminal is live, so run the fallible body, capture its
    // result, ALWAYS restore, then surface the result. `restore_terminal` is
    // idempotent, so restoring here and re-initializing next loop is safe.
    let result = run_inner(&mut terminal, app, root);
    restore_terminal();
    result
}

/// The fallible body of [`run`], entered only after [`init_terminal`] succeeds.
///
/// Split out so [`run`] can guarantee [`restore_terminal`] runs on every exit:
/// the `?`-propagated errors from [`EventLoop::new`] and `terminal.draw` leave
/// through this function's return value rather than bypassing the caller's
/// restore. Dropping the local `events` here also joins the input reader before
/// [`run`] returns (see [`EventLoop`]'s `Drop`), so the reader has released
/// stdin before `main` spawns `claude`.
fn run_inner(terminal: &mut DefaultTerminal, app: &mut App, root: &Path) -> Result<Outcome> {
    // A returning `claude` child can hand back a dirty terminal (alt screen
    // dropped, stale cells, leaked input modes) — notably a `Ctrl-Z` that exits
    // `claude` without restoring the terminal.
    // Re-assert a clean alt screen BEFORE the first draw; the fresh terminal's
    // first `draw` below fully repaints it. Runs inside `run_inner` so any failure
    // propagates through the captured result and [`run`] still restores the
    // terminal.
    hard_reset()?;
    let events = EventLoop::new(root, TICK)?;
    // Refresh the live-session badges OFF the UI thread on their own cadence, so
    // the `claude agents --json` shell-out can never block rendering. Delivered
    // as `AppEvent::LiveAgents` on the merged channel and applied in `update`.
    events.spawn_agents_poller(crate::watch::AGENTS_REFRESH);

    let outcome = loop {
        terminal.draw(|frame| view::render(frame, app))?;
        match events.recv() {
            Some(event) => match update::handle_event(app, event, root) {
                Outcome::Continue => {}
                done => break done,
            },
            // All senders dropped (input + watcher + tick gone): exit cleanly.
            None => break Outcome::Quit,
        }
    };

    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Task VERIFY-4: the teardown sequence must include `DisableMouseCapture`,
    /// so a spawned `claude` / a clean quit / an error exit all leave mouse mode
    /// off. Asserted via the `Write`-generic helper against a `Vec<u8>` (no TTY).
    #[test]
    fn teardown_emits_the_disable_mouse_capture_sequence() {
        let mut buf: Vec<u8> = Vec::new();
        disable_mouse(&mut buf).expect("write DisableMouseCapture");
        // crossterm's DisableMouseCapture resets mouse tracking; the normal-
        // tracking reset is `CSI ?1000 l`. Assert that byte sequence is present.
        assert!(
            buf.windows(6).any(|w| w == b"?1000l"),
            "teardown must emit DisableMouseCapture (?1000l), got {:?}",
            String::from_utf8_lossy(&buf)
        );
    }

    /// The hard-reset seam must turn OFF the input modes a spawned `claude` child
    /// could leave enabled — bracketed paste (`CSI ?2004`) and focus reporting
    /// (`CSI ?1004`) — so a `Ctrl-Z` that exits `claude` dirty, or an abnormal
    /// child exit, cannot leak stray bytes into the board. Asserted via the
    /// `Write`-generic helper against a `Vec<u8>` (no TTY), so the decision logic
    /// stays pure and testable without a real terminal.
    #[test]
    fn hard_reset_disables_bracketed_paste_and_focus_reporting() {
        let mut buf: Vec<u8> = Vec::new();
        reset_child_modes(&mut buf).expect("write child-mode resets");
        let seq = String::from_utf8_lossy(&buf);
        // crossterm's DisableBracketedPaste is `CSI ?2004 l`; DisableFocusChange
        // is `CSI ?1004 l`. Assert both byte sequences are present.
        assert!(
            buf.windows(6).any(|w| w == b"?2004l"),
            "hard reset must disable bracketed paste (?2004l), got {seq:?}"
        );
        assert!(
            buf.windows(6).any(|w| w == b"?1004l"),
            "hard reset must disable focus reporting (?1004l), got {seq:?}"
        );
    }

    /// The hard-reset seam must FIRST recover the terminal's escape parser to
    /// ground state — before any re-init escape — so a `claude` child that exited
    /// MID control-string cannot swallow the board's own escapes and cascade into
    /// SGR-as-literal-text corruption (the reported leaked `[39m`). Asserts the
    /// recovery emits, IN ORDER, `CAN` (`0x18`), then `ST` (`ESC \` = `0x1b 0x5c`),
    /// then the SGR reset (`CSI 0 m`, i.e. `[0m`). Tests the PURE helper (not the
    /// impure `hard_reset` driver) against a `Vec<u8>`, so no real TTY is needed;
    /// `hard_reset` calling this helper first is what puts it before the re-init
    /// escapes emitted by [`reset_child_modes`] / [`reassert_board_screen`].
    #[test]
    fn hard_reset_recovers_parser_state_first_in_order() {
        let mut buf: Vec<u8> = Vec::new();
        recover_parser_state(&mut buf).expect("write parser-recovery sequence");
        let seq = String::from_utf8_lossy(&buf);

        // 1. CAN (0x18) aborts an in-flight CSI/escape sequence.
        let can = buf
            .iter()
            .position(|&b| b == CAN)
            .unwrap_or_else(|| panic!("recovery must emit CAN (0x18), got {seq:?}"));
        // 2. ST (ESC \ = 0x1b 0x5c) closes a pending OSC/DCS/SOS/PM/APC string.
        // ST's ESC is 0x1b 0x5c; the SGR reset's ESC is 0x1b 0x5b ('['), so this
        // matches the ST pair specifically and not the SGR escape.
        let st = buf
            .windows(2)
            .position(|w| w == ST)
            .unwrap_or_else(|| panic!("recovery must emit ST (ESC \\ = 1b 5c), got {seq:?}"));
        // 3. SGR reset (CSI 0 m); match `[0m` so the ST bytes cannot satisfy it.
        let sgr = buf
            .windows(3)
            .position(|w| w == b"[0m")
            .unwrap_or_else(|| panic!("recovery must emit SGR reset (CSI 0m), got {seq:?}"));

        assert!(can < st, "CAN must precede ST, got {seq:?}");
        assert!(st < sgr, "ST must precede the SGR reset, got {seq:?}");
    }

    /// The hard-reset seam must re-enter a FRESH alternate screen (`CSI ?1049h`),
    /// CLEAR the visible screen (`CSI 2J`, crossterm `Clear(ClearType::All)`), and
    /// PURGE the terminal's native scrollback (`CSI 3J`, crossterm
    /// `Clear(ClearType::Purge)`) on return from a spawned `claude` child, so a
    /// dirty hand-back cannot leave the emulator's saved lines bleeding through
    /// the repainted board. All three escapes are WRITE-ONLY — no cursor-position
    /// (DSR) query — which is what keeps the return leg from blocking on a dirty
    /// child's stdin (the regression [`hard_reset`] documents). Asserted via the
    /// `Write`-generic helper against a `Vec<u8>` (no TTY), so the decision logic
    /// stays pure and testable without a real terminal.
    #[test]
    fn hard_reset_reenters_alt_screen_clears_and_purges_scrollback() {
        let mut buf: Vec<u8> = Vec::new();
        reassert_board_screen(&mut buf).expect("write board re-assert sequence");
        let seq = String::from_utf8_lossy(&buf);
        // crossterm's EnterAlternateScreen is `CSI ?1049 h` (LeaveAlternateScreen
        // is the `?1049l` counterpart, so match the `h` form to prove re-entry).
        assert!(
            buf.windows(6).any(|w| w == b"?1049h"),
            "hard reset must re-enter a fresh alternate screen (?1049h), got {seq:?}"
        );
        // crossterm's Clear(ClearType::All) is `CSI 2 J`; match `[2J` so a bare
        // `2J` substring elsewhere cannot satisfy the assertion.
        assert!(
            buf.windows(3).any(|w| w == b"[2J"),
            "hard reset must clear the visible screen (CSI 2J), got {seq:?}"
        );
        // crossterm's Clear(ClearType::Purge) is `CSI 3 J`; match `[3J` so a bare
        // `3J` substring elsewhere cannot satisfy the assertion.
        assert!(
            buf.windows(3).any(|w| w == b"[3J"),
            "hard reset must purge native scrollback (CSI 3J), got {seq:?}"
        );
    }
}
