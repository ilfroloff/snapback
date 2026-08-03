//! Terminal UI shell (ratatui).
//!
//! The elm-style presentation layer: `app` holds the `App` model, `update`
//! runs the event loop (Input / SessionsChanged / Tick), and `view` renders the
//! two-pane grouped list + preview. This module also owns terminal
//! setup/teardown ([`init_terminal`] / [`restore_terminal`], including a panic
//! hook) so a crash never leaves the terminal broken, and drives the render +
//! event loop in [`run`].

pub mod app;
pub mod compose;
pub mod update;
pub mod view;

use std::io::{self, Write};
use std::time::Duration;

use anyhow::Result;
use crossterm::cursor::{SetCursorStyle, Show};
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableMouseCapture, PopKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::style::{Attribute, SetAttribute};
use crossterm::terminal::{
    enable_raw_mode, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::DefaultTerminal;

use crate::store::SessionStore;
use crate::watch::EventLoop;

pub use app::{App, Scope};
pub use update::Outcome;

/// Cadence of the periodic redraw tick that drives autorefresh visibility.
const TICK: Duration = crate::watch::TICK;

/// Enter the alternate screen, enable raw mode, turn on mouse capture (for
/// wheel/trackpad scrolling) and BRACKETED PASTE, and install a panic hook that
/// restores the terminal — both of those modes included — before unwinding.
///
/// Built on `ratatui::try_init`, which performs raw mode, the alternate screen,
/// and a panic hook that calls `ratatui::restore`. That restore disables NEITHER
/// mouse capture nor bracketed paste, so after `try_init` we WRAP the panic hook
/// (see [`install_mode_safe_panic_hook`]) and only then enable the two modes, so
/// EVERY exit — quit, error, resume hand-off, or panic — leaves both OFF.
/// The returned [`DefaultTerminal`] is a `CrosstermBackend` writing to stdout.
///
/// Bracketed paste (`CSI ?2004h`) is what makes crossterm deliver a clipboard drop
/// as ONE [`crossterm::event::Event::Paste`] instead of a stream of `KeyEvent`s.
/// Without it a multi-line paste into the quick-reply compose zone sent its first
/// line as the reply (a bare `Enter` is `Send`), typed the remainder into the
/// board's search query, and let a further newline reach the board's `Enter` =
/// resume binding — a truncated send plus an unintended `claude` hand-off. The
/// mode therefore belongs to snapback now, which is why it must also be disabled
/// on every teardown path (see [`restore_terminal`]) and RE-ARMED after every
/// child return (see [`reassert_board_screen`]).
pub fn init_terminal() -> Result<DefaultTerminal> {
    let terminal = ratatui::try_init()?;
    // Wrap ratatui's (restore-only) panic hook BEFORE enabling the two modes, so
    // even a panic between here and the first draw turns both back off.
    install_mode_safe_panic_hook();
    if let Err(err) = execute!(io::stdout(), EnableMouseCapture, EnableBracketedPaste) {
        // A failed enable must not leak the raw mode / alt screen try_init set up,
        // nor a half-applied mode pair — `restore_terminal` disables both.
        restore_terminal();
        return Err(err.into());
    }
    Ok(terminal)
}

/// Restore the terminal to its original state: disable mouse capture, disable
/// bracketed paste, disable raw mode, and leave the alternate screen.
///
/// This is the teardown seam for the resume round trip: [`run`] calls it before
/// returning an [`Outcome::Resume`], so `claude` is spawned onto a clean,
/// non-raw terminal with mouse mode and bracketed paste OFF, and the loop in
/// `main` re-initializes afterwards. Both modes are disabled FIRST (while still on
/// the alt screen), then `ratatui::restore` drops raw mode and the alt screen; every
/// step is idempotent, so re-initializing each loop iteration is safe. Errors are
/// ignored — restoring on the way out is best-effort by design.
///
/// Bracketed paste is disabled here for the same reason mouse capture is: snapback
/// ENABLES it in [`init_terminal`], so leaving it on would hand the user's shell —
/// or the spawned `claude` child — a mode it never asked for, wrapping its pastes
/// in `ESC[200~`/`ESC[201~` markers it may not consume.
pub fn restore_terminal() {
    let _ = disable_mouse(&mut io::stdout());
    let _ = disable_paste(&mut io::stdout());
    ratatui::restore();
}

/// Write crossterm's `DisableMouseCapture` sequence to `w` (flushed by
/// `execute!`). Factored out and generic over [`Write`] so the teardown escape
/// sequence can be asserted in a unit test without a real TTY.
fn disable_mouse<W: Write>(w: &mut W) -> io::Result<()> {
    execute!(w, DisableMouseCapture)
}

/// Write crossterm's `DisableBracketedPaste` sequence (`CSI ?2004l`) to `w`
/// (flushed by `execute!`). The teardown half of the mode [`init_terminal`]
/// enables; factored out and generic over [`Write`] for exactly the reason
/// [`disable_mouse`] is — so the escape can be asserted in a unit test without a
/// real TTY.
fn disable_paste<W: Write>(w: &mut W) -> io::Result<()> {
    execute!(w, DisableBracketedPaste)
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

/// Kitty keyboard protocol "set enhancement flags ABSOLUTELY to 0" — `CSI = 0 u`
/// (bytes `1b 5b 3d 30 75`), per the kitty progressive-enhancement spec's
/// `CSI = flags ; mode u` set form with `flags = 0`. Unlike a POP (which clears
/// one stack level) this forces the flags at the CURRENT stack level to none, so
/// pairing it with [`PopKeyboardEnhancementFlags`] disables any residue the pop
/// left at the base level (see [`reset_terminal_state`] for why both are needed).
/// crossterm has no typed command for the set form, so it is written as raw
/// bytes (mirrors [`CAN`] / [`ST`]). WRITE-ONLY and a harmless no-op on terminals
/// that do not implement the protocol.
const KITTY_DISABLE_KEYBOARD: [u8; 5] = [0x1b, b'[', b'=', b'0', b'u'];

/// `DECSTR` (Soft Terminal Reset) — `CSI ! p` (bytes `1b 5b 21 70`), DEC STD 070.
/// Sweeps the remaining minor DEC ANSI modes to their power-on defaults
/// (DECOM/DECAWM/DECCKM off, DECTCEM cursor shown, IRM replace, SGR normal,
/// keypad numeric, DECSTBM full-screen margins, saved cursor home) in ONE
/// escape, so a child's leftover minor-mode state cannot bleed into the board. It
/// is a soft reset over DEC-defined modes ONLY: it does NOT touch the xterm
/// PRIVATE modes `?1049` (alt screen) or `?100x` (mouse), which is why
/// [`reassert_board_screen`] can safely re-assert those AFTER this runs. No
/// crossterm typed command emits it, so it is written as raw bytes (mirrors
/// [`CAN`] / [`ST`]). WRITE-ONLY.
const DECSTR: [u8; 4] = [0x1b, b'[', b'!', b'p'];

/// `DECCKM` off (Normal Cursor Keys mode) — `CSI ? 1 l` (bytes `1b 5b 3f 31 6c`).
/// Forces the cursor/arrow keys back to the normal `CSI A/B/C/D` encoding rather
/// than the application `SS3 A/B/…` form a child (e.g. one in an editor mode) may
/// have set with `CSI ? 1 h`, so the board's arrow-key handling decodes as
/// expected. [`DECSTR`] already resets DECCKM, but this asserts it explicitly and
/// independently of soft-reset support. No crossterm typed command emits it, so
/// it is written as raw bytes (mirrors [`CAN`] / [`ST`]). WRITE-ONLY.
const DECCKM_OFF: [u8; 5] = [0x1b, b'[', b'?', b'1', b'l'];

/// Return the terminal's KEYBOARD, CURSOR, and minor-MODE state to a known-good
/// baseline after a spawned `claude` child handed control back — the state
/// [`hard_reset`]'s other seams do not cover. Every escape here is WRITE-ONLY (no
/// cursor-position / DSR `CSI 6n` query, so it can never block reading a reply
/// from a dirty child's stdin) and a harmless no-op on a terminal that lacks the
/// mode, so [`hard_reset`] runs it UNCONDITIONALLY on every board (re)entry.
/// Factored out and generic over [`Write`] so the emitted bytes can be asserted
/// in a unit test without a real TTY (mirrors [`recover_parser_state`] /
/// [`reset_child_modes`] / [`reassert_board_screen`]).
///
/// Emitted, in order:
/// 1. [`PopKeyboardEnhancementFlags`] — crossterm emits `CSI < 1 u`, popping ONE
///    level of the kitty keyboard progressive-enhancement stack. This is the
///    PRIORITY fix: a `claude` child that pushed the protocol (`CSI > … u`) and
///    exited on `Ctrl-Z` without popping leaves an enhancement level active, and
///    the leftover level re-encodes ordinary keys (adds release events, alternate
///    key reports), scrambling the board's input.
/// 2. [`KITTY_DISABLE_KEYBOARD`] — `CSI = 0 u`, set the flags ABSOLUTELY to 0.
///    A single pop clears only ONE stack entry, and we CANNOT query the stack
///    depth to know how many to pop — that needs `CSI ? u`, a DSR-class query
///    whose reply would block on a dirty child's stdin (forbidden on this path,
///    the same reason [`hard_reset`] issues no `CSI 6n`). So instead of popping
///    blindly we PAIR one pop with an absolute-off at the current level, the most
///    robust write-only way to reach "no enhancement" regardless of residual
///    depth.
/// 3. [`SetCursorStyle::DefaultUserShape`] — `CSI 0 SP q` (DECSCUSR default),
///    normalizing a blinking-bar / custom cursor shape the child set.
/// 4. [`Show`] — `CSI ?25h` (DECTCEM on), un-hiding a cursor the child hid.
///    ratatui re-hides/re-positions the cursor per frame anyway; this just avoids
///    an invisible or wrongly-shaped cursor flashing on return.
/// 5. [`DECSTR`] — `CSI ! p` soft reset, sweeping the remaining minor DEC modes.
/// 6. [`DECCKM_OFF`] — `CSI ? 1 l`, explicit normal cursor-keys encoding.
fn reset_terminal_state<W: Write>(w: &mut W) -> io::Result<()> {
    // Kitty keyboard protocol: pop one enhancement level, then hard-disable any
    // residue at the base level with an absolute `CSI = 0 u`. A single pop clears
    // only ONE stack level and the depth is unknowable without a forbidden query
    // (see the doc comment), so pop + absolute-off is the robust write-only path.
    execute!(w, PopKeyboardEnhancementFlags)?;
    w.write_all(&KITTY_DISABLE_KEYBOARD)?;
    // Normalize cursor shape + visibility the child may have changed (ratatui
    // re-hides/re-positions per frame regardless).
    execute!(w, SetCursorStyle::DefaultUserShape, Show)?;
    // Soft-reset the remaining minor DEC modes, then force normal cursor-key
    // encoding explicitly. Neither has a crossterm typed command — raw bytes.
    // DECSTR does NOT touch the xterm private modes `?1049` (alt screen) / `?100x`
    // (mouse) that `reassert_board_screen` re-asserts AFTER this, so it is safe
    // here.
    w.write_all(&DECSTR)?;
    w.write_all(&DECCKM_OFF)?;
    Ok(())
}

/// Turn OFF the input modes a spawned `claude` child may have left enabled, so a
/// child that exited on `Ctrl-Z` without restoring, or exited abnormally, cannot
/// leak WHATEVER IT SET into the board.
///
/// Bracketed paste (`CSI ?2004`) and focus reporting (`CSI ?1004`) both inject
/// synthetic bytes into stdin while active (`ESC[200~…ESC[201~` around pastes,
/// `ESC[I` / `ESC[O` on focus changes). Focus reporting snapback never uses at all,
/// so for that one this disable is the whole story.
///
/// Bracketed paste is different, and the difference is the point: snapback now OWNS
/// that mode — [`init_terminal`] enables it so a paste arrives as one
/// [`crossterm::event::Event::Paste`] rather than as a stream of keystrokes. This
/// disable therefore does NOT mean "snapback never wants it"; it CLEARS whatever
/// level the child left behind, and [`reassert_board_screen`] re-asserts the
/// board's OWN enable immediately afterwards. Clear-then-assert is deliberate and
/// must stay in that order: it is the "ONE complete return-to-known-state"
/// principle [`hard_reset`] is built on, applied to a mode both processes touch.
/// Do not delete this disable on the grounds that the board turns paste back on —
/// the re-assert is what makes the sequence safe, not redundant.
///
/// Factored out and generic over [`Write`] so the reset sequence can be asserted in
/// a unit test without a real TTY (mirrors [`disable_mouse`] / [`disable_paste`]).
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
/// 3. [`EnableBracketedPaste`] — re-arm the mode [`init_terminal`] owns, for the
///    same reason and on the same pattern as mouse capture. It MUST live here
///    rather than in [`init_terminal`] alone: [`hard_reset`] runs
///    [`reset_child_modes`] (which disables paste, clearing the child's level) and
///    `DECSTR` on the way back, so the board's own enable has to land AFTER both or
///    a returning child would silently leave paste off for the rest of the session.
/// 4. [`Clear`]`(`[`ClearType::All`]`)` — `CSI 2J`, erase the visible screen. A
///    WRITE-ONLY clear: unlike [`ratatui::Terminal::clear`] it emits `2J`
///    directly and never issues a cursor-position query, so it cannot block on a
///    stdin reply from a dirty child (see [`hard_reset`]). The matching
///    back-buffer reset is unnecessary here — [`init_terminal`] hands
///    [`run_inner`] a brand-new terminal whose first `draw` repaints every cell.
/// 5. [`Clear`]`(`[`ClearType::Purge`]`)` — `CSI 3J`, erase the emulator's saved
///    lines. This is load-bearing: without it the native scrollback bleeds
///    through the repainted board (the reported corruption).
fn reassert_board_screen<W: Write>(w: &mut W) -> io::Result<()> {
    execute!(
        w,
        LeaveAlternateScreen,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste,
        Clear(ClearType::All),
        Clear(ClearType::Purge),
    )
}

/// Clear the child's leaked input modes and THEN re-assert the board's own screen
/// and modes — the ordered pair [`hard_reset`] performs as its last step.
///
/// This exists as ONE helper, rather than two calls in [`hard_reset`], because the
/// ORDER between them is the contract and a contract that spans two call sites
/// cannot be pinned by a test: bracketed paste is turned OFF by
/// [`reset_child_modes`] (`CSI ?2004l`, clearing whatever the child left) and back
/// ON by [`reassert_board_screen`] (`CSI ?2004h`, the board's own enable). Run in
/// the other order the board ends up with paste DISABLED after every child return
/// and every paste silently reverts to the keystroke stream this whole seam exists
/// to avoid. Composing them here makes that reorder a one-line change in a
/// [`Write`]-generic function whose emitted bytes a unit test asserts, with no TTY.
fn reset_child_modes_and_reassert_board<W: Write>(w: &mut W) -> io::Result<()> {
    reset_child_modes(w)?;
    reassert_board_screen(w)
}

/// Assert ONE COMPLETE return-to-known-good-state reset after returning from a
/// spawned `claude` child, healing a terminal the child left DIRTY. Every escape
/// it emits is WRITE-ONLY — it NEVER issues a cursor-position (DSR `CSI 6n`)
/// query.
///
/// The guiding principle is to re-assert a whole known state, NOT to clear one
/// mode per reported bug: a child that exits dirty on `Ctrl-Z` can leave ANY of
/// the modes it touched set, so the seam sweeps the full set the board depends on
/// — escape parser, kitty keyboard protocol, cursor shape/visibility, minor DEC
/// modes, input modes, alt screen, mouse, bracketed paste — every time. The
/// kitty keyboard protocol is the load-bearing addition: a `claude` child that
/// pushed progressive enhancement and exited without popping leaves an
/// enhancement level active that re-encodes ordinary keys and scrambles the
/// board's input; it is NOT one of the modes `restore_terminal` / the older
/// seams ever cleared, so it persisted across the round trip and was the
/// primary cause of the reported still-unstable `Ctrl-Z`.
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
/// 2. [`reset_terminal_state`] — return keyboard/cursor/minor-mode state to a
///    known-good baseline: pop the kitty keyboard protocol + absolute-off
///    (`CSI < 1 u` + `CSI = 0 u`) so a child's leftover progressive-enhancement
///    level cannot scramble the board's key input, reset cursor shape + show it
///    (`CSI 0 SP q` + `CSI ?25h`), soft-reset the remaining minor DEC modes with
///    `DECSTR` (`CSI ! p`), and force normal cursor keys with `DECCKM`-off
///    (`CSI ? 1 l`). Placed BEFORE step 4: `DECSTR` is a soft reset over DEC ANSI
///    modes only and does NOT touch the xterm private modes `?1049` / `?100x`, so
///    the alt-screen + mouse re-assert in step 4 still wins.
/// 3. [`enable_raw_mode`] — cheap, idempotent confirmation of raw mode (already
///    re-applied by `try_init`), so this seam's post-condition — alt screen +
///    raw mode + mouse capture + bracketed paste — is self-contained rather than
///    assumed.
/// 4. [`reset_child_modes_and_reassert_board`] — the ordered pair that ends the
///    seam, kept in ONE helper because the order between its halves is the
///    contract (see its doc):
///    * [`reset_child_modes`] first — turn off input modes the child may have
///      leaked (bracketed paste `?2004l`, focus reporting `?1004l`).
///    * [`reassert_board_screen`] second — round-trip
///      `Leave`→`EnterAlternateScreen` to force a FRESH alt buffer (defeating the
///      no-op re-enter), re-arm mouse capture AND bracketed paste (`?2004h`, the
///      board's own enable, which must land after the disable above), clear the
///      visible screen with `Clear(ClearType::All)` (`CSI 2J`), and purge the
///      native SCROLLBACK with `Clear(ClearType::Purge)` (`CSI 3J`).
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
    // Return keyboard/cursor/minor-mode state to a known-good baseline (kitty
    // keyboard-protocol pop + absolute-off, cursor shape + show, DECSTR soft
    // reset, DECCKM-off). Runs AFTER parser recovery and BEFORE the alt-screen /
    // mouse re-assert, so DECSTR's soft reset cannot undo `?1049` / `?100x`.
    reset_terminal_state(&mut out)?;
    enable_raw_mode()?;
    // Clear the child's leaked input modes, THEN re-assert the board's own screen
    // and modes. One helper, because the order between the two is the contract:
    // bracketed paste is disabled by the first and re-enabled by the second.
    reset_child_modes_and_reassert_board(&mut out)?;
    Ok(())
}

/// Wrap the current panic hook so a panic disables mouse capture AND bracketed
/// paste before the existing hook runs.
///
/// Called right after [`ratatui::try_init`], so the hook it wraps is ratatui's,
/// which restores raw mode + the main screen but leaves both of those modes ON.
/// This closes the last teardown path — a panic — that would otherwise leak them
/// into the user's shell: a leftover mouse mode turns clicks into escape bytes, and
/// a leftover bracketed paste wraps the shell's own pastes in `ESC[200~`/`ESC[201~`
/// markers it may print literally. Both disables are best-effort (errors ignored),
/// and both mirror what [`restore_terminal`] does on the non-panicking paths.
fn install_mode_safe_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_mouse(&mut io::stdout());
        let _ = disable_paste(&mut io::stdout());
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
pub fn run(app: &mut App, store: &mut SessionStore) -> Result<Outcome> {
    let mut terminal = init_terminal()?;
    // `init_terminal()?` failing needs no restore — nothing was enabled yet.
    // From here on the terminal is live, so run the fallible body, capture its
    // result, ALWAYS restore, then surface the result. `restore_terminal` is
    // idempotent, so restoring here and re-initializing next loop is safe.
    let result = run_inner(&mut terminal, app, store);
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
fn run_inner(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    store: &mut SessionStore,
) -> Result<Outcome> {
    // A returning `claude` child can hand back a dirty terminal (alt screen
    // dropped, stale cells, leaked input modes) — notably a `Ctrl-Z` that exits
    // `claude` without restoring the terminal.
    // Re-assert a clean alt screen BEFORE the first draw; the fresh terminal's
    // first `draw` below fully repaints it. Runs inside `run_inner` so any failure
    // propagates through the captured result and [`run`] still restores the
    // terminal.
    hard_reset()?;
    // The watcher and the store read the same root by construction — it is the
    // store's own — so the two can never drift onto different trees.
    let root = store.root().to_path_buf();
    let events = EventLoop::new(&root, TICK)?;
    // Refresh the agent badges OFF the UI thread on their own cadence, so the
    // `claude agents --json --all` shell-out can never block rendering. Delivered
    // as `AppEvent::ReportedAgents` on the merged channel and applied in `update`.
    events.spawn_agents_poller(crate::watch::AGENTS_REFRESH);

    let outcome = loop {
        terminal.draw(|frame| view::render(frame, app))?;
        match events.recv() {
            Some(event) => match update::handle_event(app, event, store) {
                Outcome::Continue => {}
                // A confirmed quick-reply send: fire it on a detached thread and
                // KEEP drawing — the board never tears down (contrast
                // `Outcome::Resume`). The child reports back via
                // `AppEvent::SendFinished` on this same channel, so the completion
                // status and the reloaded reply both land on the live board.
                Outcome::Send(req) => {
                    crate::send::spawn_send(req, events.sender());
                }
                // A confirmed interrupt: fire `claude stop` on a detached thread and
                // KEEP drawing, exactly like `Outcome::Send`. The stop reports back
                // via `AppEvent::InterruptFinished` on this same channel.
                Outcome::Interrupt(req) => {
                    crate::send::spawn_interrupt(req, events.sender());
                }
                // A confirmed background-agent launch: fire `claude --bg` on a
                // detached thread and KEEP drawing, exactly like `Outcome::Send`.
                // `--bg` returns as soon as the agent is registered and needs no
                // TTY, so there is nothing here worth a teardown; the launch
                // reports back via `AppEvent::BgLaunchFinished` on this same
                // channel, and the agent itself appears on the board through the
                // watcher reload.
                Outcome::BgLaunch(req) => {
                    crate::send::spawn_bg_launch(req, events.sender());
                }
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

    /// The teardown sequence must also include `DisableBracketedPaste`: snapback
    /// ENABLES that mode in `init_terminal`, so quitting, erroring out, handing off
    /// to `claude`, or panicking must all hand the mode back off — otherwise the
    /// user's shell keeps wrapping its own pastes in `ESC[200~`/`ESC[201~`.
    /// Asserted via the `Write`-generic helper against a `Vec<u8>` (no TTY), exactly
    /// like its `disable_mouse` sibling above.
    #[test]
    fn teardown_emits_the_disable_bracketed_paste_sequence() {
        let mut buf: Vec<u8> = Vec::new();
        disable_paste(&mut buf).expect("write DisableBracketedPaste");
        assert!(
            buf.windows(6).any(|w| w == b"?2004l"),
            "teardown must emit DisableBracketedPaste (?2004l), got {:?}",
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

    /// Bracketed paste is turned OFF (clearing the child's level) and then back ON
    /// (the board's own enable) on every return from a spawned `claude` child, IN
    /// THAT ORDER. Reverse the two and the board comes back with paste disabled
    /// forever after the first resume, so every subsequent paste degrades to the
    /// keystroke stream that sends a reply's first line and resumes on its second.
    ///
    /// The order is pinned against the ONE production helper that owns it
    /// (`reset_child_modes_and_reassert_board`, which `hard_reset` calls) rather
    /// than by writing both halves into a buffer here. That distinction is the
    /// whole value of this test: a version that called `reset_child_modes` and
    /// `reassert_board_screen` itself would only assert the order the TEST chose
    /// and would stay green through a real reorder of the production calls.
    #[test]
    fn hard_reset_disables_then_re_enables_bracketed_paste_in_order() {
        let mut buf: Vec<u8> = Vec::new();
        reset_child_modes_and_reassert_board(&mut buf).expect("write the child-return sequence");
        let seq = String::from_utf8_lossy(&buf);

        // crossterm's DisableBracketedPaste is `CSI ?2004 l`, EnableBracketedPaste
        // `CSI ?2004 h`; the trailing letter is the only difference, so each match
        // is exact.
        let off = buf
            .windows(6)
            .position(|w| w == b"?2004l")
            .unwrap_or_else(|| {
                panic!("the child return must first disable bracketed paste (?2004l), got {seq:?}")
            });
        let on = buf
            .windows(6)
            .position(|w| w == b"?2004h")
            .unwrap_or_else(|| {
                panic!("the child return must re-enable bracketed paste (?2004h), got {seq:?}")
            });

        assert!(
            off < on,
            "the child's leaked paste level must be cleared BEFORE the board \
             re-asserts its own enable, or the board returns with paste off; got {seq:?}"
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

    /// The hard-reset seam must return keyboard/cursor/minor-mode state to a
    /// known-good baseline on return from a spawned `claude` child. Asserts
    /// [`reset_terminal_state`] emits, IN ORDER: the kitty keyboard-protocol POP
    /// (`CSI < 1 u`) and the absolute-off (`CSI = 0 u`) — the priority fix for a
    /// leftover progressive-enhancement level scrambling key input — then the
    /// cursor-style reset (`CSI 0 SP q`), cursor Show (`CSI ?25h`), DECSTR soft
    /// reset (`CSI ! p`), and DECCKM-off / normal cursor keys (`CSI ? 1 l`). Every
    /// one is WRITE-ONLY (no DSR `CSI 6n`), keeping the return leg non-blocking on
    /// a dirty child's stdin. Tests the PURE helper against a `Vec<u8>` (no TTY).
    ///
    /// NOTE: crossterm 0.29's `PopKeyboardEnhancementFlags` emits `CSI < 1 u`
    /// (pop ONE level, with an explicit count), not the bare `CSI < u` — both pop
    /// a single stack level, so the count is immaterial; the test matches the
    /// bytes the pinned crossterm actually writes.
    #[test]
    fn hard_reset_returns_keyboard_cursor_and_minor_modes_to_baseline() {
        let mut buf: Vec<u8> = Vec::new();
        reset_terminal_state(&mut buf).expect("write terminal-state reset sequence");
        let seq = String::from_utf8_lossy(&buf);

        // Kitty keyboard protocol POP (`CSI < 1 u`).
        let pop = buf
            .windows(4)
            .position(|w| w == b"[<1u")
            .unwrap_or_else(|| {
                panic!("reset must pop the kitty keyboard flags (CSI <1u), got {seq:?}")
            });
        // Kitty keyboard protocol absolute-off (`CSI = 0 u`).
        let koff = buf
            .windows(4)
            .position(|w| w == b"[=0u")
            .unwrap_or_else(|| {
                panic!("reset must absolute-disable the kitty flags (CSI =0u), got {seq:?}")
            });
        // Default cursor style (DECSCUSR): `CSI 0 SP q` — the space before `q`
        // distinguishes it from the SGR reset `[0m` in `recover_parser_state`.
        let cursor = buf
            .windows(4)
            .position(|w| w == b"[0 q")
            .unwrap_or_else(|| {
                panic!("reset must set the default cursor style (CSI 0 SP q), got {seq:?}")
            });
        // Cursor Show (DECTCEM on): `CSI ?25h`.
        let show = buf
            .windows(5)
            .position(|w| w == b"[?25h")
            .unwrap_or_else(|| panic!("reset must show the cursor (CSI ?25h), got {seq:?}"));
        // DECSTR soft reset: `CSI ! p`.
        let decstr = buf.windows(3).position(|w| w == b"[!p").unwrap_or_else(|| {
            panic!("reset must soft-reset the terminal (DECSTR, CSI !p), got {seq:?}")
        });
        // DECCKM off / normal cursor keys: `CSI ? 1 l`.
        let ckm = buf
            .windows(4)
            .position(|w| w == b"[?1l")
            .unwrap_or_else(|| {
                panic!("reset must set normal cursor keys (DECCKM off, CSI ?1l), got {seq:?}")
            });

        // Order matches the doc contract: pop → absolute-off → cursor style →
        // show → DECSTR → DECCKM-off.
        assert!(
            pop < koff,
            "kitty pop must precede absolute-off, got {seq:?}"
        );
        assert!(
            koff < cursor,
            "kitty absolute-off must precede the cursor-style reset, got {seq:?}"
        );
        assert!(
            cursor < show,
            "cursor-style reset must precede cursor show, got {seq:?}"
        );
        assert!(
            show < decstr,
            "cursor show must precede DECSTR, got {seq:?}"
        );
        assert!(decstr < ckm, "DECSTR must precede DECCKM-off, got {seq:?}");
    }

    /// Parser recovery must still come FIRST — before the new keyboard/cursor/mode
    /// resets — on the return path, so a child stuck MID control-string cannot
    /// swallow the mode-reset escapes as string content. Composes the two PURE
    /// helpers in the SAME order [`hard_reset`] calls them into one buffer and
    /// asserts the parser-recovery prefix (`CAN`) lands before the first
    /// mode-reset escape (the kitty pop `CSI < 1 u`). Tests the pure helpers, not
    /// the impure `hard_reset` driver.
    #[test]
    fn hard_reset_recovers_parser_before_the_mode_resets() {
        let mut buf: Vec<u8> = Vec::new();
        recover_parser_state(&mut buf).expect("write parser-recovery sequence");
        reset_terminal_state(&mut buf).expect("write terminal-state reset sequence");
        let seq = String::from_utf8_lossy(&buf);

        let can = buf
            .iter()
            .position(|&b| b == CAN)
            .unwrap_or_else(|| panic!("recovery must emit CAN (0x18) first, got {seq:?}"));
        let pop = buf
            .windows(4)
            .position(|w| w == b"[<1u")
            .unwrap_or_else(|| panic!("reset must emit the kitty pop (CSI <1u), got {seq:?}"));
        assert!(
            can < pop,
            "parser recovery (CAN) must precede the first mode reset (kitty pop), got {seq:?}"
        );
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
