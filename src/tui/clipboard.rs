//! System-clipboard copy for `Ctrl-X y`: the OS clipboard TOOL first, a write-only
//! OSC 52 escape as the fallback.
//!
//! Why two routes. OSC 52 hands the id to the TERMINAL and hopes: some terminals
//! drop it silently (RustRover's JediTerm has no OSC 52 handler at all), and the
//! write-only discipline forbids asking whether it landed. A local clipboard tool
//! reports an exit code instead, so the board's status can say what actually
//! happened. Over SSH the tool would fill the REMOTE machine's clipboard, not the
//! user's, so there OSC 52 — which the user's own terminal receives — is the only
//! route that can reach them.
//!
//! The module splits the way the rest of the TUI does:
//!
//! - a PURE route decision ([`clipboard_route`]) over [`ClipboardEnv`] facts: the
//!   ORDERED clipboard-tool candidates to try, where an empty list means "go
//!   straight to OSC 52". Local macOS → `pbcopy`; local Linux → `wl-copy` under
//!   Wayland, else `xclip` then `xsel` under X11; SSH, no display, or any other OS
//!   → no tool;
//! - ONE thin environment edge ([`ClipboardEnv::from_env`]), the only place the
//!   route reads the environment;
//! - a THREADED tool copy ([`spawn_tool_copy`]): the `tui::run_inner` driver starts
//!   it for `update::Outcome::Copy`, it pipes the id into each candidate on its OWN
//!   thread, and reports exactly one `AppEvent::CopyFinished`. It never touches the
//!   terminal;
//! - the OSC 52 fallback: a PURE, unit-tested encoder core ([`base64_encode`] →
//!   [`osc52_clipboard_sequence`]) and a THIN `Write`-generic writer
//!   ([`copy_to_clipboard`]). Its one runtime caller is `update::finish_copy`,
//!   which the driver runs on the UI thread, between draws, with `io::stdout()` —
//!   at once when the route has no tool, or when a `CopyFinished` reports that no
//!   tool copied the id. The escape is never queried back.
//!
//! No new dependency: base64 is hand-inlined here (see [`base64_encode`]) rather
//! than pulling `base64` into `[dependencies]`, and the tools are plain
//! `std::process` children rather than an in-process clipboard crate, keeping the
//! binary self-contained and matching the crate's dependency restraint (the
//! hand-rolled markdown pass in `store::preview` and the hand-parsed agent
//! frontmatter make the same call).
//!
//! Every item here is reachable from the driver's `Ctrl-X y` path, so none of them
//! needs a `#[allow(dead_code)]`.

use std::ffi::OsStr;
use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;

use crate::watch::AppEvent;

/// Set by OpenSSH's `sshd` in every session it serves. Its presence means snapback
/// runs on the REMOTE end of an SSH login, where a local clipboard tool would fill
/// the remote machine's clipboard rather than the user's, so the route goes
/// straight to OSC 52, which the user's own terminal receives.
const SSH_CONNECTION_VAR: &str = "SSH_CONNECTION";

/// Set by `sshd` when the session has a pseudo-terminal. A second SSH witness
/// beside [`SSH_CONNECTION_VAR`], because a wrapper that filters the environment
/// can keep one and drop the other; either one means SSH.
const SSH_TTY_VAR: &str = "SSH_TTY";

/// The Wayland compositor's socket name, set in every Wayland session. It selects
/// `wl-copy`, the tool that speaks the Wayland clipboard protocol natively, and it
/// wins over [`DISPLAY_VAR`] because XWayland sets that one inside Wayland too.
const WAYLAND_DISPLAY_VAR: &str = "WAYLAND_DISPLAY";

/// The X11 display, set in every X session. It selects the X11 tools (`xclip`,
/// then `xsel`) when no Wayland display is present.
const DISPLAY_VAR: &str = "DISPLAY";

/// One clipboard-tool invocation: the program and its fixed argv. The id always
/// travels on the child's STDIN, never as an argument, so no id ever needs quoting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipboardTool {
    /// Program name, resolved on `PATH` by [`Command::new`].
    pub program: &'static str,
    /// Fixed arguments that make the program copy its stdin to the CLIPBOARD.
    pub args: &'static [&'static str],
}

/// macOS's built-in pasteboard writer: copies its stdin to the general pasteboard
/// (what `Cmd-V` pastes) and exits 0. It ships with every macOS install and needs
/// no argument.
const PBCOPY: ClipboardTool = ClipboardTool {
    program: "pbcopy",
    args: &[],
};

/// wl-clipboard's writer for Wayland sessions. With no argument it copies stdin to
/// the regular CLIPBOARD (the primary selection would need `--primary`) and leaves
/// a small server behind, so the id stays pasteable after snapback exits.
const WL_COPY: ClipboardTool = ClipboardTool {
    program: "wl-copy",
    args: &[],
};

/// `xclip` for X11 sessions. `-selection clipboard` targets the CLIPBOARD: xclip's
/// default is PRIMARY (middle-click), which `Ctrl-V` never reads. It reads stdin by
/// default and leaves a server behind that keeps serving the id after it exits.
const XCLIP: ClipboardTool = ClipboardTool {
    program: "xclip",
    args: &["-selection", "clipboard"],
};

/// `xsel`, the X11 fallback when `xclip` is missing or fails. `--clipboard` picks
/// the CLIPBOARD (its default is PRIMARY too), and `--input` forces the
/// stdin-reading mode, which xsel would otherwise guess from whether stdin is a
/// terminal.
const XSEL: ClipboardTool = ClipboardTool {
    program: "xsel",
    args: &["--clipboard", "--input"],
};

/// The macOS route: `pbcopy` alone.
const MACOS_TOOLS: &[ClipboardTool] = &[PBCOPY];

/// The Wayland route: `wl-copy` alone.
const WAYLAND_TOOLS: &[ClipboardTool] = &[WL_COPY];

/// The X11 route, in order: `xclip`, then `xsel` when xclip is missing or fails.
const X11_TOOLS: &[ClipboardTool] = &[XCLIP, XSEL];

/// No tool to try: the copy goes straight to the OSC 52 fallback.
const NO_TOOLS: &[ClipboardTool] = &[];

/// The operating systems the route tells apart. snapback ships for `darwin` and
/// `linux` only (`npm/package.json`); any other target takes the OSC 52 route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetOs {
    /// macOS (`target_os = "macos"`).
    MacOs,
    /// Linux (`target_os = "linux"`).
    Linux,
    /// Any other target.
    Other,
}

impl TargetOs {
    /// The OS this binary was BUILT for — a compile-time fact, not an env read.
    fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Other
        }
    }
}

/// The environment facts [`clipboard_route`] decides from. Plain data, so the
/// route is a pure function a table test can walk row by row, and so the one place
/// that reads the environment ([`ClipboardEnv::from_env`]) stays a thin edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipboardEnv {
    /// snapback runs inside an SSH login ([`SSH_CONNECTION_VAR`] or
    /// [`SSH_TTY_VAR`] is set).
    ssh: bool,
    /// The OS the binary was built for.
    os: TargetOs,
    /// A Wayland display is present ([`WAYLAND_DISPLAY_VAR`] is set).
    wayland: bool,
    /// An X11 display is present ([`DISPLAY_VAR`] is set).
    x11: bool,
}

impl ClipboardEnv {
    /// Read the route's facts from the process environment and the build target.
    ///
    /// This is the ONLY place the copy route touches the environment; everything
    /// downstream decides from the returned value. It does not bypass the `config`
    /// module's rule ("the single place that reads the environment"): that rule
    /// covers snapback-OWNED PATHS, and these four variables are facts about the
    /// session snapback runs in, not paths snapback owns.
    #[must_use]
    pub fn from_env() -> Self {
        let set = |name: &str| is_set(std::env::var_os(name).as_deref());
        Self {
            ssh: set(SSH_CONNECTION_VAR) || set(SSH_TTY_VAR),
            os: TargetOs::current(),
            wayland: set(WAYLAND_DISPLAY_VAR),
            x11: set(DISPLAY_VAR),
        }
    }
}

/// Whether an environment variable counts as SET: present AND non-empty. An empty
/// `DISPLAY=` names no display, and an empty `SSH_TTY=` names no tty, so both read
/// as unset. Pure, so the edge above stays a one-liner per variable.
fn is_set(value: Option<&OsStr>) -> bool {
    value.is_some_and(|v| !v.is_empty())
}

/// Decide which clipboard tools to try, in order, for `env`. PURE: it never reads
/// the environment (the caller passes the facts in), so every row is table-tested.
///
/// - SSH, on any OS → none: a local tool would fill the remote machine's
///   clipboard, and OSC 52 reaches the user's own terminal;
/// - local macOS → `pbcopy`;
/// - local Linux with a Wayland display → `wl-copy`;
/// - local Linux with only an X11 display → `xclip`, then `xsel`;
/// - anything else (no display, another OS) → none.
///
/// An empty slice sends the copy straight to the OSC 52 fallback.
#[must_use]
pub fn clipboard_route(env: ClipboardEnv) -> &'static [ClipboardTool] {
    if env.ssh {
        return NO_TOOLS;
    }
    match env.os {
        TargetOs::MacOs => MACOS_TOOLS,
        TargetOs::Linux if env.wayland => WAYLAND_TOOLS,
        TargetOs::Linux if env.x11 => X11_TOOLS,
        TargetOs::Linux | TargetOs::Other => NO_TOOLS,
    }
}

/// Copy `session_id` with the first of `tools` that succeeds, on its OWN detached
/// thread, and deliver exactly one [`AppEvent::CopyFinished`] when it is done — the
/// UI thread never waits on a clipboard tool, even one that hangs.
///
/// The one-shot shape of `send::spawn_send` (a thread per request, one completion
/// event, a failed send ignored because the board went away), started from the
/// same place: the `tui::run_inner` driver, for `update::Outcome::Copy`. The worker
/// NEVER touches the terminal — the OSC 52 fallback a `copied: false` result calls
/// for is written by the driver on the UI thread, where it cannot interleave with a
/// ratatui frame.
///
/// `tools` is a parameter (the route [`clipboard_route`] picked) so a test can hand
/// in a harmless stand-in instead of a real clipboard tool.
pub fn spawn_tool_copy(tools: &'static [ClipboardTool], session_id: String, tx: Sender<AppEvent>) {
    std::thread::spawn(move || {
        let copied = copy_with_tools(tools, &session_id);
        // A send failure means the receiver (the board) has gone away; ignore it.
        let _ = tx.send(AppEvent::CopyFinished { session_id, copied });
    });
}

/// Try each of `tools` in order and stop at the first that copies `payload`;
/// `true` once one has. A missing or failing tool moves on to the next one.
fn copy_with_tools(tools: &[ClipboardTool], payload: &str) -> bool {
    tools.iter().any(|tool| run_tool(*tool, payload))
}

/// Run ONE clipboard tool with `payload` on its stdin; `true` only if the whole id
/// reached its stdin AND it exited 0.
///
/// stdout and stderr are nulled, the way `resume::open_url` nulls its opener's: a
/// tool can then never paint over the board, and a tool that leaves a server
/// behind to keep serving the clipboard (`xclip`, `xsel`, `wl-copy`) cannot hold a
/// pipe open that anything here waits to drain. A spawn error (the tool is not
/// installed), a failed stdin write, or a non-zero exit all return `false`.
fn run_tool(tool: ClipboardTool, payload: &str) -> bool {
    let Ok(mut child) = Command::new(tool.program)
        .args(tool.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    // The closure takes the stdin handle BY VALUE and drops it on return, so the
    // tool sees EOF BEFORE the wait below. Every one of these tools reads stdin to
    // EOF before it exits, so waiting with stdin still open would hang forever.
    let wrote = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(payload.as_bytes()).is_ok());
    // Reap the child whatever happened above, so a failed write leaves no zombie.
    let exited_clean = child.wait().is_ok_and(|status| status.success());
    wrote && exited_clean
}

/// Standard (RFC 4648 §4) base64 alphabet: `A–Z a–z 0–9 + /`, indexed 0–63 by
/// each 6-bit group. Every terminal's OSC 52 handler decodes this alphabet with
/// `=` padding, so it is the encoding the clipboard write must produce.
const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Pad byte (`=`) appended so the encoded length is always a multiple of 4, per
/// RFC 4648 §4: one `=` for a 2-byte final chunk, two for a 1-byte final chunk.
const BASE64_PAD: u8 = b'=';

/// Low-6-bits mask (`0b11_1111` = 63). base64 slices each 24-bit input group
/// into four 6-bit indices, and this mask selects one such index.
const SIX_BIT_MASK: u32 = 0b11_1111;

/// Encode `bytes` as standard-alphabet base64 with `=` padding.
///
/// Inlined on purpose: `base64` is only a TRANSITIVE dependency, and a session
/// id is a 36-byte UUID — promoting a crate to `[dependencies]` for ~20 lines of
/// well-specified, fully-tested arithmetic fails YAGNI and the self-contained
/// binary principle. So this is hand-rolled and pinned by RFC 4648 test vectors,
/// exactly like the crate's other hand-rolled parsers.
fn base64_encode(bytes: &[u8]) -> String {
    // base64 packs each run of 3 input bytes (24 bits) into 4 output chars
    // (6 bits each); a short final chunk is zero-extended and `=`-padded.
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let group = (b0 << 16) | (b1 << 8) | b2;

        // First two sextets are always real; the last two depend on how many
        // input bytes the chunk actually carried.
        out.push(BASE64_ALPHABET[((group >> 18) & SIX_BIT_MASK) as usize] as char);
        out.push(BASE64_ALPHABET[((group >> 12) & SIX_BIT_MASK) as usize] as char);
        out.push(match chunk.len() {
            1 => BASE64_PAD as char,
            _ => BASE64_ALPHABET[((group >> 6) & SIX_BIT_MASK) as usize] as char,
        });
        out.push(match chunk.len() {
            3 => BASE64_ALPHABET[(group & SIX_BIT_MASK) as usize] as char,
            _ => BASE64_PAD as char,
        });
    }
    out
}

/// OSC (Operating System Command) introducer — `ESC ]` (bytes `0x1b 0x5d`),
/// ECMA-48 §8.3.89. Opens the control string that carries the clipboard command.
const OSC_INTRODUCER: [u8; 2] = [0x1b, b']'];

/// OSC command `52` — xterm's "manipulate selection data" (clipboard) command.
/// The bytes are the ASCII digits `5` `2`; a `;` separates it from the selector.
const OSC52_COMMAND: &[u8] = b"52";

/// Selection selector `c` — the system CLIPBOARD selection. The primary
/// selection (`p`) is deliberately out of scope, so `c` is the only selector
/// this module ever writes.
const OSC52_CLIPBOARD_SELECTOR: u8 = b'c';

/// OSC parameter separator (`;`), placed between the command, the selector, and
/// the base64 payload per the `OSC 52 ; c ; <data>` grammar.
const OSC_PARAM_SEPARATOR: u8 = b';';

/// OSC string terminator: `BEL` (`0x07`).
///
/// Two terminators are legal for an OSC string — `BEL` and `ST` (`ESC \`, the
/// form `tui::mod`'s parser-recovery seam emits). `BEL` is chosen here for the
/// BROADEST terminal compatibility: it is the single-byte terminator xterm and
/// virtually every emulator accept for OSC 52, and some simpler terminals and
/// multiplexers handle it more reliably than the two-byte `ST`. That trade-off
/// matters because OSC 52 support is already uneven and this write is
/// best-effort (the sticky status line carries the full id as the visible
/// fallback), so we favor the terminator most likely to land over matching the
/// module's `ST` idiom.
const OSC_STRING_TERMINATOR: u8 = 0x07;

/// Build the write-only OSC 52 SET-clipboard escape for `payload`:
/// `ESC ] 52 ; c ; <base64(payload)> BEL`.
///
/// Pure — it performs NO I/O. It only ever emits the SET form; it NEVER emits
/// the OSC 52 `?` query form, honoring the write-only / no-DSR discipline in
/// AGENTS.md TERMINAL SAFETY (a query would block reading a reply). The wire
/// grammar is assembled entirely from the named `const`s above so there are no
/// magic bytes. Public so the update loop's tests can state the exact bytes the
/// fallback must write.
#[must_use]
pub fn osc52_clipboard_sequence(payload: &str) -> Vec<u8> {
    let encoded = base64_encode(payload.as_bytes());
    // + 4: the two `;` separators, the `c` selector, and the BEL terminator.
    let mut seq =
        Vec::with_capacity(OSC_INTRODUCER.len() + OSC52_COMMAND.len() + encoded.len() + 4);
    seq.extend_from_slice(&OSC_INTRODUCER);
    seq.extend_from_slice(OSC52_COMMAND);
    seq.push(OSC_PARAM_SEPARATOR);
    seq.push(OSC52_CLIPBOARD_SELECTOR);
    seq.push(OSC_PARAM_SEPARATOR);
    seq.extend_from_slice(encoded.as_bytes());
    seq.push(OSC_STRING_TERMINATOR);
    seq
}

/// Write the OSC 52 set-clipboard escape for `payload` to `w` and flush.
///
/// This is a WRITE-ONLY escape: like `tui::mod`'s `hard_reset` parser-recovery
/// seam (the `CAN`/`ST` raw-escape exception to the "never embed ANSI escapes"
/// rule), it moves no cursor, mutates no screen cells, and issues no
/// cursor-position (DSR `CSI 6n`) query — it only hands the terminal a clipboard
/// payload. It is therefore safe to emit MID-SESSION between ratatui draws (the
/// same reasoning that lets `hard_reset` run between board entries): the next
/// `terminal.draw` diffs against an unchanged screen and repaints nothing, and
/// the return leg can never block reading a reply because there is no reply. It
/// extends that same narrow, justified raw-escape exception to a second,
/// equally write-only site.
///
/// Generic over [`Write`] (mirroring `disable_mouse` in `tui::mod`) so the exact
/// emitted bytes can be asserted against a `Vec<u8>` in a unit test without a
/// real terminal.
pub fn copy_to_clipboard<W: Write>(w: &mut W, payload: &str) -> io::Result<()> {
    w.write_all(&osc52_clipboard_sequence(payload))?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::mpsc;
    use std::time::Duration;

    /// A real 36-char session UUID — the exact payload this module exists to copy.
    const ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    /// How long a test waits for a stand-in child to report back. Generous, so a
    /// loaded CI box cannot turn a slow spawn into a false failure.
    const WORKER_TIMEOUT: Duration = Duration::from_secs(10);

    /// A harmless stand-in for a clipboard tool that exits 0 ONLY when its stdin
    /// is exactly [`ID`] — so a pass proves the id travelled on stdin, whole, and
    /// that stdin was closed (the `$(cat)` would never return otherwise). `sh` is
    /// not a clipboard tool, so no test touches the real clipboard.
    const ACCEPTS_ID: ClipboardTool = ClipboardTool {
        program: "sh",
        args: &[
            "-c",
            "[ \"$(cat)\" = 550e8400-e29b-41d4-a716-446655440000 ]",
        ],
    };

    /// A stand-in that always exits non-zero.
    const FAILS: ClipboardTool = ClipboardTool {
        program: "false",
        args: &[],
    };

    /// A stand-in that cannot be spawned at all, like a tool that is not installed.
    const MISSING: ClipboardTool = ClipboardTool {
        program: "snapback-test-no-such-clipboard-tool",
        args: &[],
    };

    /// Build route facts for one table row.
    fn env(ssh: bool, os: TargetOs, wayland: bool, x11: bool) -> ClipboardEnv {
        ClipboardEnv {
            ssh,
            os,
            wayland,
            x11,
        }
    }

    /// The route table, row by row: SSH always goes to OSC 52 (on macOS AND on a
    /// Linux desktop that has a display); a local session gets its OS's tool;
    /// Wayland wins over X11 on a desktop that has both; and no display, or any
    /// other OS, goes to OSC 52.
    #[test]
    fn the_route_picks_each_clipboard_tool_by_session_os_and_display() {
        use super::TargetOs::{Linux, MacOs, Other};
        let rows: [(ClipboardEnv, &[ClipboardTool], &str); 11] = [
            (env(true, MacOs, false, false), &[], "SSH into a Mac"),
            (
                env(true, Linux, true, true),
                &[],
                "SSH into a Linux desktop",
            ),
            (env(true, Linux, false, true), &[], "SSH with X forwarding"),
            (env(false, MacOs, false, false), &[PBCOPY], "local macOS"),
            (
                env(false, MacOs, false, true),
                &[PBCOPY],
                "local macOS with XQuartz's DISPLAY",
            ),
            (env(false, Linux, true, false), &[WL_COPY], "local Wayland"),
            (
                env(false, Linux, true, true),
                &[WL_COPY],
                "Wayland wins over XWayland's DISPLAY",
            ),
            (
                env(false, Linux, false, true),
                &[XCLIP, XSEL],
                "local X11: xclip, then xsel",
            ),
            (
                env(false, Linux, false, false),
                &[],
                "Linux with no display",
            ),
            (env(false, Other, false, true), &[], "another OS"),
            (env(false, Other, false, false), &[], "another OS, headless"),
        ];
        for (facts, expected, case) in rows {
            assert_eq!(clipboard_route(facts), expected, "{case}: {facts:?}");
        }
    }

    /// The X11 candidates carry the argv that targets the CLIPBOARD (not PRIMARY)
    /// from stdin, and the other tools take none — a typo here would copy into a
    /// selection `Ctrl-V` never reads.
    #[test]
    fn each_tool_carries_the_argv_that_copies_stdin_to_the_clipboard() {
        assert_eq!((PBCOPY.program, PBCOPY.args), ("pbcopy", &[][..]));
        assert_eq!((WL_COPY.program, WL_COPY.args), ("wl-copy", &[][..]));
        assert_eq!(
            (XCLIP.program, XCLIP.args),
            ("xclip", &["-selection", "clipboard"][..])
        );
        assert_eq!(
            (XSEL.program, XSEL.args),
            ("xsel", &["--clipboard", "--input"][..])
        );
    }

    /// An empty variable names nothing, so it reads as unset; any value counts.
    #[test]
    fn an_env_var_is_set_only_when_present_and_non_empty() {
        assert!(!is_set(None));
        assert!(!is_set(Some(OsStr::new(""))));
        assert!(is_set(Some(OsStr::new(":0"))));
    }

    /// One tool: a clean exit with the id on stdin copies; a non-zero exit or a
    /// tool that is not installed does not.
    #[test]
    fn a_tool_copies_only_on_a_clean_exit_with_the_id_on_its_stdin() {
        assert!(run_tool(ACCEPTS_ID, ID), "exit 0 with the id on stdin");
        assert!(
            !run_tool(ACCEPTS_ID, "a-different-id"),
            "the stand-in must actually check what arrived on stdin"
        );
        assert!(!run_tool(FAILS, ID), "a non-zero exit is not a copy");
        assert!(!run_tool(MISSING, ID), "a missing tool is not a copy");
    }

    /// The candidates are tried IN ORDER: a missing or failing tool falls through
    /// to the next one, and every candidate failing (or none at all) is `false`.
    #[test]
    fn a_missing_or_failing_tool_falls_through_to_the_next_candidate() {
        assert!(copy_with_tools(&[MISSING, FAILS, ACCEPTS_ID], ID));
        assert!(!copy_with_tools(&[MISSING, FAILS], ID));
        assert!(!copy_with_tools(&[], ID));
    }

    /// The first success STOPS the walk: a later candidate never runs. The later
    /// one here would leave a marker file behind, so its absence is the proof.
    #[test]
    fn the_first_successful_tool_stops_the_walk() {
        let marker = std::env::temp_dir().join(format!(
            "snapback-clipboard-marker-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("the clock is past the epoch")
                .as_nanos()
        ));
        // Leaked so the stand-in can carry the path in its `&'static` argv; one
        // short string per test run.
        let path: &'static str = Box::leak(marker.display().to_string().into_boxed_str());
        let leaves_marker = ClipboardTool {
            program: "sh",
            args: Box::leak(vec!["-c", "cat >/dev/null; touch \"$0\"", path].into_boxed_slice()),
        };

        assert!(copy_with_tools(&[ACCEPTS_ID, leaves_marker], ID));
        assert!(
            !marker.exists(),
            "a candidate after the first success must never run"
        );
        // Control: the marker tool DOES leave the file when it is reached, so the
        // absence above is evidence rather than a stand-in that never works.
        assert!(copy_with_tools(&[FAILS, leaves_marker], ID));
        assert!(marker.exists(), "the marker stand-in works when it runs");
        let _ = std::fs::remove_file(&marker);
    }

    /// The worker reports EXACTLY ONE `CopyFinished` per copy, carrying the id it
    /// was given and the result, and then lets go of the channel.
    #[test]
    fn the_worker_reports_exactly_one_copy_finished() {
        const COPIES: &[ClipboardTool] = &[ACCEPTS_ID];
        const NOTHING_COPIES: &[ClipboardTool] = &[MISSING, FAILS];
        for (tools, copied) in [(COPIES, true), (NOTHING_COPIES, false)] {
            let (tx, rx) = mpsc::channel();
            spawn_tool_copy(tools, ID.to_string(), tx);
            match rx.recv_timeout(WORKER_TIMEOUT) {
                Ok(AppEvent::CopyFinished {
                    session_id,
                    copied: got,
                }) => {
                    assert_eq!(session_id, ID);
                    assert_eq!(got, copied, "the result for {tools:?}");
                }
                other => panic!("expected one CopyFinished, got {other:?}"),
            }
            assert!(
                rx.recv_timeout(WORKER_TIMEOUT).is_err(),
                "exactly one event: the worker drops its sender once it reports"
            );
        }
    }

    /// RFC 4648 §10 test vectors — the canonical base64 conformance set, so this
    /// pins the encoder against the spec rather than against itself.
    #[test]
    fn base64_encode_matches_rfc4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    /// A real 36-char session UUID — the exact payload this module exists to
    /// copy — encodes to a known, evenly-padded base64 string (36 bytes is a
    /// multiple of 3, so there is no trailing `=`).
    #[test]
    fn base64_encode_handles_a_full_session_uuid() {
        let id = "550e8400-e29b-41d4-a716-446655440000";
        assert_eq!(id.len(), 36);
        assert_eq!(
            base64_encode(id.as_bytes()),
            "NTUwZTg0MDAtZTI5Yi00MWQ0LWE3MTYtNDQ2NjU1NDQwMDAw"
        );
    }

    /// The full OSC 52 sequence for a known id is the exact bytes `ESC ] 52 ; c ;
    /// <base64> BEL`. Expected is assembled from the wire parts so the test pins
    /// the introducer, the `52;c;` prefix, the payload, AND the BEL terminator
    /// independently of the code under test.
    #[test]
    fn osc52_sequence_is_exact_bytes_for_a_known_id() {
        let id = "550e8400-e29b-41d4-a716-446655440000";
        let mut expected = Vec::new();
        expected.extend_from_slice(b"\x1b]52;c;");
        expected.extend_from_slice(b"NTUwZTg0MDAtZTI5Yi00MWQ0LWE3MTYtNDQ2NjU1NDQwMDAw");
        expected.push(0x07);
        assert_eq!(osc52_clipboard_sequence(id), expected);
    }

    /// The thin driver writes EXACTLY the pure sequence into a `Vec<u8>` (no TTY
    /// needed, mirroring `disable_mouse`'s test), and the literal prefix +
    /// terminator are pinned so a regression in either is caught here too.
    #[test]
    fn copy_to_clipboard_writes_the_exact_sequence_to_the_writer() {
        let id = "550e8400-e29b-41d4-a716-446655440000";
        let mut buf: Vec<u8> = Vec::new();
        copy_to_clipboard(&mut buf, id).expect("write OSC 52 sequence to buffer");
        assert_eq!(buf, osc52_clipboard_sequence(id));
        assert!(
            buf.starts_with(b"\x1b]52;c;"),
            "must start with the OSC 52 clipboard prefix, got {:?}",
            String::from_utf8_lossy(&buf)
        );
        assert_eq!(
            *buf.last().expect("sequence is non-empty"),
            0x07,
            "must end with the BEL terminator"
        );
    }
}
