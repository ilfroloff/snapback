//! System-clipboard COPY for `Ctrl-X y` and for a preview drag-selection (the OS
//! clipboard TOOL first, a write-only OSC 52 escape as the fallback), and the
//! clipboard READ behind a compose box's `Ctrl-V` attach.
//!
//! The READ ([`clipboard_read_route`], [`spawn_clipboard_read`]) reuses
//! [`ClipboardEnv`] and the decision below, and adds no dependency: `osascript` on
//! macOS, `wl-paste` on Wayland, `xclip` on X11, each argv a named const. `xsel`
//! stays copy-only because it cannot read non-text targets. Every read child runs
//! with stdin and stderr nulled and stdout piped, never inheriting the TTY, on its
//! own thread, in a process group of its own, bounded by a timeout (which kills
//! the whole group), a bounded wait for its stdout to close, and a byte cap, and
//! there is no OSC 52 read (a query would need a terminal reply, which TERMINAL
//! SAFETY forbids).
//!
//! A pasted IMAGE is saved under `config::paste_cache_dir()`, and ONE prune
//! ([`prune_expired`]) deletes the cache's own files past [`PASTE_CACHE_TTL`] from
//! two places: the read worker, before each image write, and a detached thread
//! `tui::run_inner` starts at every board start ([`spawn_paste_cache_prune`]), so
//! a screenshot does not outlive the TTL just because no later paste came. An
//! image whose result the compose dropped (a read `Esc` cancelled, or a stale
//! token) is deleted sooner, on a detached thread the driver starts
//! ([`spawn_paste_image_discard`]).
//!
//! Both copies are ONE path. What is copied arrives as a
//! [`CopyPayload`](crate::watch::CopyPayload) — a session id or the selected
//! transcript text — and every part below is indifferent to which: it moves the
//! payload's text, and only the status line (`update::finish_copy`) names the kind.
//!
//! Why two routes. OSC 52 hands the text to the TERMINAL and hopes: some terminals
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
//!   it for `update::Outcome::Copy`, it pipes the text into each candidate on its
//!   OWN thread, and reports exactly one `AppEvent::CopyFinished`. It never touches
//!   the terminal;
//! - the OSC 52 fallback: a PURE, unit-tested encoder core ([`base64_encode`] →
//!   [`osc52_clipboard_sequence`]) and a THIN `Write`-generic writer
//!   ([`copy_to_clipboard`]) — the crate's ONLY OSC 52 encoder and its only
//!   writer. Its one runtime caller is `update::finish_copy`, which the driver runs
//!   on the UI thread, between draws, with `io::stdout()` — at once when the route
//!   has no tool, or when a `CopyFinished` reports that no tool copied the text.
//!   The escape is never queried back.
//!
//! No new dependency: base64 is hand-inlined here (see [`base64_encode`]) rather
//! than pulling `base64` into `[dependencies]`, and the tools are plain
//! `std::process` children rather than an in-process clipboard crate, keeping the
//! binary self-contained and matching the crate's dependency restraint (the
//! hand-rolled markdown pass in `store::preview` and the hand-parsed agent
//! frontmatter make the same call).
//!
//! The READ entry points are reached from the compose box: `compose::take_clipboard_read`
//! asks, and `tui::run_inner` starts [`spawn_clipboard_read`] on that answer.

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::watch::{AppEvent, ClipboardAttach, CopyPayload};

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

/// One clipboard-tool invocation: the program and its fixed argv. The text always
/// travels on the child's STDIN, never as an argument, so no text ever needs
/// quoting — a multi-line selection included.
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
/// a small server behind, so the text stays pasteable after snapback exits.
const WL_COPY: ClipboardTool = ClipboardTool {
    program: "wl-copy",
    args: &[],
};

/// `xclip` for X11 sessions. `-selection clipboard` targets the CLIPBOARD: xclip's
/// default is PRIMARY (middle-click), which `Ctrl-V` never reads. It reads stdin by
/// default and leaves a server behind that keeps serving the text after it exits.
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

/// Copy `payload`'s text with the first of `tools` that succeeds, on its OWN
/// detached thread, and deliver exactly one [`AppEvent::CopyFinished`] — carrying
/// the payload back — when it is done. The UI thread never waits on a clipboard
/// tool, even one that hangs.
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
pub fn spawn_tool_copy(
    tools: &'static [ClipboardTool],
    payload: CopyPayload,
    tx: Sender<AppEvent>,
) {
    std::thread::spawn(move || {
        let copied = copy_with_tools(tools, payload.text());
        // A send failure means the receiver (the board) has gone away; ignore it.
        let _ = tx.send(AppEvent::CopyFinished { payload, copied });
    });
}

/// Try each of `tools` in order and stop at the first that copies `payload`;
/// `true` once one has. A missing or failing tool moves on to the next one.
fn copy_with_tools(tools: &[ClipboardTool], payload: &str) -> bool {
    tools.iter().any(|tool| run_tool(*tool, payload))
}

/// Run ONE clipboard tool with `payload` on its stdin; `true` only if the whole
/// text reached its stdin AND it exited 0.
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
/// Inlined on purpose: `base64` is only a TRANSITIVE dependency, and the payloads
/// are small — a 36-byte session UUID, or a selection no bigger than one screen of
/// transcript — so promoting a crate to `[dependencies]` for ~20 lines of
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
/// best-effort (for `Ctrl-X y` the sticky status line carries the full id as the
/// visible fallback; a selection stays highlighted in the pane, where a native
/// Shift/Option selection can take it again), so we favor the terminator most
/// likely to land over matching the
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

// ---------------------------------------------------------------------------
// READ: what the clipboard holds, for a compose box's `Ctrl-V` attach.
// ---------------------------------------------------------------------------

/// The longest a clipboard READ child may run before its whole process group is
/// killed and it is reaped. A hung selection owner (`xclip` against an
/// unresponsive X client) would otherwise pin the worker thread forever; ten
/// seconds covers a large screenshot converted by `osascript` while still failing
/// visibly.
const CLIPBOARD_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the read watchdog polls a child for exit. Short enough that a
/// finished tool is noticed at once, long enough not to spin.
const READ_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How long a read waits, once its child has ended (exited, or killed with its
/// group at the timeout), for its stdout reader to hand the bytes back. The
/// child's own writes are then at most one pipe buffer behind, drained in well
/// under this. It expires only while a DESCENDANT still holds stdout: one that
/// left the group (its own `setpgid`/`setsid`), or one that outlived a child that
/// exited by itself, whose group is never signalled because the reap has freed its
/// id. The read then reports `Failed` instead of waiting on it, and that reader
/// thread ends with its writer. The same bound as `claude_catalog`'s reader grace.
const READ_STDOUT_GRACE: Duration = Duration::from_millis(500);

/// The most image bytes a read may hand back or write: 25 MiB. claude re-encodes
/// a large image itself before sending it, so a bigger one buys nothing, and the
/// cap bounds both the worker's memory and the disk a repeated paste can fill.
const PASTE_IMAGE_MAX_BYTES: usize = 25 * 1024 * 1024;

/// The most bytes a TYPE or FILE listing may carry: 1 MiB, far above any real
/// listing (a few hundred copied files) and small enough to buffer without care.
const LIST_MAX_BYTES: usize = 1024 * 1024;

/// How long a cached paste lives: 7 days. [`prune_expired`] deletes an older one
/// before the next image write AND at every board start
/// ([`spawn_paste_cache_prune`]). claude embeds an attached file's content at send
/// time, so a file only has to outlive a draft left open and a `--bg` worker's
/// first read; a week makes losing one negligible, and bounds what a screenshot
/// can leave on disk.
const PASTE_CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Placeholder in an image command's args replaced with the clipboard TYPE to
/// read: a MIME target on Linux, a pasteboard type (UTI) on macOS.
const TYPE_PLACEHOLDER: &str = "{type}";

/// Placeholder in an image command's args replaced with the file the tool must
/// write (the macOS route, whose tool writes a file instead of stdout).
const DEST_PLACEHOLDER: &str = "{dest}";

/// Prefix of every cached paste file name.
const PASTE_FILE_PREFIX: &str = "paste-";

/// Leads the TEMP name an image is written under before its rename
/// (`.paste-….tmp`), hiding it from a plain `ls`. [`is_paste_cache_name`] accepts
/// it, so a temp file a crash left behind is pruned like a finished one.
const PASTE_TEMP_LEAD: char = '.';

/// Ends the temp name an image is written under before its rename.
const PASTE_TEMP_SUFFIX: &str = ".tmp";

/// Octal mode of the paste cache dir on unix: owner only, because a screenshot can
/// carry secrets.
#[cfg(unix)]
const PASTE_DIR_MODE: u32 = 0o700;

/// One clipboard-READ invocation fixed at compile time: the program and its argv,
/// where a [`TYPE_PLACEHOLDER`] or [`DEST_PLACEHOLDER`] slot is filled per read
/// ([`ReadCmd::substituted`]). Every argv the read can run is one of these named
/// consts, the way the copy tools are, so a test pins each one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReadArgv {
    /// Program name, resolved on `PATH` by [`Command::new`].
    program: &'static str,
    /// Its arguments, slots included.
    args: &'static [&'static str],
}

/// One clipboard-read child: program and argv, owned so a test can build a
/// stand-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadCmd {
    program: String,
    args: Vec<String>,
}

impl ReadCmd {
    fn new(program: &str, args: &[&str]) -> Self {
        Self {
            program: program.to_owned(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
        }
    }

    /// This command with `{type}` and `{dest}` filled in.
    fn substituted(&self, target: &str, dest: &str) -> Self {
        Self {
            program: self.program.clone(),
            args: self
                .args
                .iter()
                .map(|a| {
                    a.replace(TYPE_PLACEHOLDER, target)
                        .replace(DEST_PLACEHOLDER, dest)
                })
                .collect(),
        }
    }
}

impl From<ReadArgv> for ReadCmd {
    fn from(argv: ReadArgv) -> Self {
        Self::new(argv.program, argv.args)
    }
}

/// How the LIST step's stdout is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListFormat {
    /// macOS: [`MACOS_TYPE_TAG`] and [`MACOS_FILE_TAG`] lines (types and ALL file
    /// URLs at once).
    MacOs,
    /// `wl-paste` / `xclip`: one type per line; file URLs need a second step.
    TypeLines,
}

/// Where the IMAGE step's bytes arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageSink {
    /// On the tool's stdout (`wl-paste`, `xclip`).
    Stdout,
    /// In the file named by `{dest}` (macOS `osascript`).
    File,
}

/// One platform's clipboard READ: the commands the worker runs. Built by
/// [`ReadBackendKind::backend`]; a test builds its own from `sh` stand-ins.
#[derive(Debug, Clone)]
pub struct ReadBackend {
    /// The tool this is, or stands in for: what its list step's exit means
    /// follows it ([`classify_list_exit`]).
    kind: ReadBackendKind,
    list: ReadCmd,
    format: ListFormat,
    /// Fetches the `text/uri-list` target (a `TypeLines` backend only).
    uri_list: Option<ReadCmd>,
    /// Reads ONE image type's own bytes, as published ([`ImageFetch::Raw`]).
    image: ReadCmd,
    /// Converts the clipboard image to PNG ([`ImageFetch::ConvertToPng`]): macOS
    /// only, `None` elsewhere.
    png_image: Option<ReadCmd>,
    sink: ImageSink,
    /// Bound on each child; a test shortens it.
    timeout: Duration,
}

/// The real read tools, one per platform route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadBackendKind {
    /// macOS: `osascript` (JavaScript for the listing, AppleScript for the image).
    Osascript,
    /// Wayland: `wl-paste`.
    WlPaste,
    /// X11: `xclip`. `xsel` cannot read non-text targets, so it is copy-only.
    Xclip,
}

/// Leads each pasteboard TYPE line [`MACOS_LIST_SCRIPT`] prints: `T\t<type>`.
const MACOS_TYPE_TAG: &str = "T\t";

/// Leads each copied FILE line [`MACOS_LIST_SCRIPT`] prints: `F\t<path>`.
const MACOS_FILE_TAG: &str = "F\t";

/// JXA that prints the general pasteboard's types ([`MACOS_TYPE_TAG`] lines) and
/// ALL file URLs ([`MACOS_FILE_TAG`] lines). A const cannot be spliced into this
/// const's text, so the tags are spelled out inside it, and
/// `the_macos_list_script_pushes_exactly_the_parsers_tags` holds the two spellings
/// together. AppleScript's `«class furl»` returns only the first file, so the
/// listing goes through `NSPasteboard` directly. A CR or LF inside a path becomes
/// U+0001 so the line-based list cannot truncate it: the path then carries a
/// control character, which `file_mention` refuses by name.
const MACOS_LIST_SCRIPT: &str = "ObjC.import('AppKit');\
var pb=$.NSPasteboard.generalPasteboard;var o=[];\
(ObjC.deepUnwrap(pb.types)||[]).forEach(function(t){o.push('T\\t'+t)});\
var opts=$.NSDictionary.dictionaryWithObjectForKey($.NSNumber.numberWithBool(true),$.NSPasteboardURLReadingFileURLsOnlyKey);\
var u=pb.readObjectsForClassesOptions($.NSArray.arrayWithObject($.NSURL),opts);\
if(u){for(var i=0;i<u.count;i++){o.push('F\\t'+ObjC.unwrap(u.objectAtIndex(i).path).replace(/[\\n\\r]/g,'\\u0001'))}}\
o.join('\\n')";

/// JXA that writes the general pasteboard's RAW bytes for the type named by
/// `argv[0]` to the file named by `argv[1]`: the image step for GIF and WebP, which
/// `«class PNGf»` is not assumed to convert. Both values arrive as `argv`, never
/// spliced into script text. A type the pasteboard lacks, or a failed write,
/// throws, so `osascript` exits non-zero. NSData's own atomic write is off
/// (`false`): the read's temp name and rename are the atomic step, and NSData's
/// would leave its helper file under a name the prune does not own. Checked on
/// macOS 27.0.1, 2026-10-11, against a private named pasteboard: GIF and WebP
/// bytes came back byte for byte, and a missing type wrote nothing.
const MACOS_RAW_IMAGE_SCRIPT: &str = "ObjC.import('AppKit');\
function run(argv){\
var d=$.NSPasteboard.generalPasteboard.dataForType(argv[0]);\
if(d.isNil()||!d.writeToFileAtomically(argv[1],false)){throw new Error('no '+argv[0])}\
}";

/// The program behind every macOS read step.
const OSASCRIPT: &str = "osascript";

/// wl-clipboard's reader, behind every Wayland read step.
const WL_PASTE: &str = "wl-paste";

/// macOS LIST step: [`MACOS_LIST_SCRIPT`], run as JavaScript for Automation.
const MACOS_LIST: ReadArgv = ReadArgv {
    program: OSASCRIPT,
    args: &["-l", "JavaScript", "-e", MACOS_LIST_SCRIPT],
};

/// macOS image step for a PNG, TIFF or JPEG pasteboard image: the AppleScript form
/// `claude 2.1.289` ships (`the clipboard as «class PNGf»`), which converts it to
/// PNG and writes the file passed as `argv`, so no path is ever spliced into
/// script text.
const MACOS_PNG_IMAGE: ReadArgv = ReadArgv {
    program: OSASCRIPT,
    args: &[
        "-e",
        "on run argv",
        "-e",
        "set png_data to (the clipboard as «class PNGf»)",
        "-e",
        "set fp to open for access POSIX file (item 1 of argv) with write permission",
        "-e",
        "set eof fp to 0",
        "-e",
        "write png_data to fp",
        "-e",
        "close access fp",
        "-e",
        "end run",
        DEST_PLACEHOLDER,
    ],
};

/// macOS image step for a GIF or WebP: [`MACOS_RAW_IMAGE_SCRIPT`], given the
/// pasteboard type and then the file as its `argv`.
const MACOS_RAW_IMAGE: ReadArgv = ReadArgv {
    program: OSASCRIPT,
    args: &[
        "-l",
        "JavaScript",
        "-e",
        MACOS_RAW_IMAGE_SCRIPT,
        TYPE_PLACEHOLDER,
        DEST_PLACEHOLDER,
    ],
};

/// Wayland LIST step: every MIME type the selection offers, one per line.
const WL_PASTE_LIST: ReadArgv = ReadArgv {
    program: WL_PASTE,
    args: &["--list-types"],
};

/// Wayland copied-FILES step: the `text/uri-list` target, without the trailing
/// newline wl-paste appends to a text type.
const WL_PASTE_URI_LIST: ReadArgv = ReadArgv {
    program: WL_PASTE,
    args: &["--no-newline", "--type", URI_LIST_TYPE],
};

/// Wayland image step: the chosen MIME target's bytes, on stdout.
const WL_PASTE_IMAGE: ReadArgv = ReadArgv {
    program: WL_PASTE,
    args: &["--type", TYPE_PLACEHOLDER],
};

/// X11 LIST step: the CLIPBOARD's `TARGETS`, one per line. `-selection clipboard`
/// as in [`XCLIP`], because xclip's default selection is PRIMARY.
const XCLIP_LIST: ReadArgv = ReadArgv {
    program: XCLIP.program,
    args: &["-selection", "clipboard", "-t", "TARGETS", "-o"],
};

/// X11 copied-FILES step: the CLIPBOARD's `text/uri-list` target.
const XCLIP_URI_LIST: ReadArgv = ReadArgv {
    program: XCLIP.program,
    args: &["-selection", "clipboard", "-t", URI_LIST_TYPE, "-o"],
};

/// X11 image step: the chosen MIME target's bytes, on stdout.
const XCLIP_IMAGE: ReadArgv = ReadArgv {
    program: XCLIP.program,
    args: &["-selection", "clipboard", "-t", TYPE_PLACEHOLDER, "-o"],
};

impl ReadBackendKind {
    /// The commands for this tool, each one of the named argv consts above.
    #[must_use]
    pub fn backend(self) -> ReadBackend {
        match self {
            Self::Osascript => ReadBackend {
                kind: self,
                list: MACOS_LIST.into(),
                format: ListFormat::MacOs,
                uri_list: None,
                image: MACOS_RAW_IMAGE.into(),
                png_image: Some(MACOS_PNG_IMAGE.into()),
                sink: ImageSink::File,
                timeout: CLIPBOARD_READ_TIMEOUT,
            },
            Self::WlPaste => ReadBackend {
                kind: self,
                list: WL_PASTE_LIST.into(),
                format: ListFormat::TypeLines,
                uri_list: Some(WL_PASTE_URI_LIST.into()),
                image: WL_PASTE_IMAGE.into(),
                png_image: None,
                sink: ImageSink::Stdout,
                timeout: CLIPBOARD_READ_TIMEOUT,
            },
            Self::Xclip => ReadBackend {
                kind: self,
                list: XCLIP_LIST.into(),
                format: ListFormat::TypeLines,
                uri_list: Some(XCLIP_URI_LIST.into()),
                image: XCLIP_IMAGE.into(),
                png_image: None,
                sink: ImageSink::Stdout,
                timeout: CLIPBOARD_READ_TIMEOUT,
            },
        }
    }
}

/// The clipboard target Linux file managers publish for copied files (RFC 2483).
const URI_LIST_TYPE: &str = "text/uri-list";

/// Leads every `file:` URI written with an authority (RFC 8089 §2), the only form
/// a `text/uri-list` or a terminal's URI paste names a local file in. Shared with
/// `compose::paste_names_files`, which matches that same rendering of a paste, and
/// `compose::paste_may_name_files`, which asks the clipboard about one.
pub(super) const FILE_URI_SCHEME: &str = "file://";

/// The one host a `file://` URI may name and still mean THIS machine (RFC 8089
/// §2, beside an empty host). Any other host names a file a local mention cannot
/// reach, so the URI is skipped.
const FILE_URI_LOCAL_HOST: &str = "localhost";

/// The MIME top-level type of every image target (RFC 2046 §4.2): a Linux
/// selection offering one outside [`IMAGE_TYPES`] holds an image claude cannot
/// read, which is a refusal of its own rather than an empty clipboard.
const MIME_IMAGE_PREFIX: &str = "image/";

/// The MIME top-level type of every text target (RFC 2046 §4.1), so a selection
/// offering any of them reads as text beside the X11 / macOS names in
/// [`TEXT_TYPES`].
const MIME_TEXT_PREFIX: &str = "text/";

/// Where a clipboard read can go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadRoute {
    /// SSH: a local tool would read the REMOTE clipboard.
    RemoteSession,
    /// No display (or another OS): nothing to ask.
    NoDisplay,
    /// Ask this tool.
    Backend(ReadBackendKind),
}

/// Decide where a clipboard read goes, from the SAME facts the copy route uses.
/// PURE, table-tested. Unlike the copy there is no OSC 52 fallback: reading it
/// back would be a terminal query, which TERMINAL SAFETY forbids.
#[must_use]
pub fn clipboard_read_route(env: ClipboardEnv) -> ReadRoute {
    if env.ssh {
        return ReadRoute::RemoteSession;
    }
    match env.os {
        TargetOs::MacOs => ReadRoute::Backend(ReadBackendKind::Osascript),
        TargetOs::Linux if env.wayland => ReadRoute::Backend(ReadBackendKind::WlPaste),
        TargetOs::Linux if env.x11 => ReadRoute::Backend(ReadBackendKind::Xclip),
        TargetOs::Linux | TargetOs::Other => ReadRoute::NoDisplay,
    }
}

/// What to do with a clipboard, decided from its types and file list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadPlan {
    /// Reference the copied files.
    Files,
    /// Save this image.
    Image(ImageType),
    /// Image data claude cannot read.
    UnsupportedImage,
    /// Text only.
    TextOnly,
    /// Nothing usable.
    Nothing,
}

/// How the image step fetches a chosen type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageFetch {
    /// The type's own bytes, as published: a Linux MIME target, or a macOS GIF or
    /// WebP read raw ([`MACOS_RAW_IMAGE`]).
    Raw,
    /// macOS `«class PNGf»` ([`MACOS_PNG_IMAGE`]): the pasteboard image converted
    /// to PNG, the form claude ships for a PNG, TIFF or JPEG pasteboard image.
    ConvertToPng,
}

/// One clipboard image type claude reads: its name as the list step prints it,
/// the extension it is saved under, and how its bytes are fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ImageType {
    /// The MIME target (Linux) or pasteboard type (macOS) to read.
    name: &'static str,
    /// The saved file's extension.
    ext: &'static str,
    /// How the image step fetches it.
    fetch: ImageFetch,
}

impl ImageType {
    /// A type saved as its own bytes, under `ext`.
    const fn raw(name: &'static str, ext: &'static str) -> Self {
        Self {
            name,
            ext,
            fetch: ImageFetch::Raw,
        }
    }

    /// A macOS type `«class PNGf»` converts, so it is saved as PNG.
    const fn converted_to_png(name: &'static str) -> Self {
        Self {
            name,
            ext: "png",
            fetch: ImageFetch::ConvertToPng,
        }
    }
}

/// Image types claude reads, best first: PNG, then JPEG, then GIF, then WebP.
/// Linux names are MIME targets; macOS names are the pasteboard types (UTIs)
/// [`MACOS_LIST_SCRIPT`] prints. A macOS PNG or JPEG converts through
/// `«class PNGf»` and saves as PNG. A GIF or WebP is never assumed convertible: it
/// is read RAW and keeps its own format. `public.tiff` ranks LAST: macOS lists a
/// synthesized TIFF beside an image of another type (a pasteboard holding only
/// `com.compuserve.gif` or only `org.webmproject.webp` also listed `public.tiff`,
/// measured on macOS 27.0.1, 2026-10-11), so ranked any higher it would shadow
/// the GIF or WebP.
const IMAGE_TYPES: &[ImageType] = &[
    ImageType::raw("image/png", "png"),
    ImageType::converted_to_png("public.png"),
    ImageType::raw("image/jpeg", "jpg"),
    ImageType::converted_to_png("public.jpeg"),
    ImageType::raw("image/gif", "gif"),
    ImageType::raw("com.compuserve.gif", "gif"),
    ImageType::raw("image/webp", "webp"),
    ImageType::raw("org.webmproject.webp", "webp"),
    ImageType::converted_to_png("public.tiff"),
];

/// macOS pasteboard types (UTIs) that name an image claude cannot read: each
/// conforms to `public.image` and is outside [`IMAGE_TYPES`]. A UTI carries no
/// shared prefix the way a MIME `image/` target does, so without this list such a
/// pasteboard would read as empty. Each one checked declared and conforming to
/// `public.image` through `UTType` on macOS 27.0.1, 2026-10-11, reading no
/// pasteboard. A `public.tiff` beside any of them still converts first
/// ([`IMAGE_TYPES`] is asked before this list).
const MACOS_UNREADABLE_IMAGE_TYPES: &[&str] = &[
    "public.image",
    "com.microsoft.bmp",
    "com.microsoft.ico",
    "com.apple.icns",
    "public.heic",
    "public.heics",
    "public.heif",
    "public.avif",
    "public.jpeg-xl",
    "public.jpeg-2000",
    "public.svg-image",
    "public.camera-raw-image",
    "com.adobe.raw-image",
    "com.apple.pict",
    "com.adobe.photoshop-image",
    "com.truevision.tga-image",
    "com.ilm.openexr-image",
    "public.radiance",
    "com.sgi.sgi-image",
    "public.pbm",
    "public.pvr",
    "com.apple.quicktime-image",
    "com.apple.macpaint-image",
    "com.kodak.flashpix-image",
    "public.xbitmap-image",
];

/// Pasteboard / selection type names that mean plain text.
const TEXT_TYPES: &[&str] = &[
    "UTF8_STRING",
    "STRING",
    "TEXT",
    "NSStringPboardType",
    "public.utf8-plain-text",
    "public.plain-text",
];

/// Decide what a read does. PURE. Precedence: copied FILES, then a readable
/// image, then an unreadable image, then text, then nothing. Files come first
/// because a Finder copy can carry the file's ICON as an image beside its URL, and
/// attaching the icon instead of the file would be wrong.
fn choose_read(types: &[String], files: &[PathBuf]) -> ReadPlan {
    if !files.is_empty() {
        return ReadPlan::Files;
    }
    let has = |name: &str| types.iter().any(|t| t == name);
    if let Some(&image) = IMAGE_TYPES.iter().find(|image| has(image.name)) {
        return ReadPlan::Image(image);
    }
    if types.iter().any(|t| {
        t.starts_with(MIME_IMAGE_PREFIX) || MACOS_UNREADABLE_IMAGE_TYPES.contains(&t.as_str())
    }) {
        return ReadPlan::UnsupportedImage;
    }
    if types
        .iter()
        .any(|t| t.starts_with(MIME_TEXT_PREFIX) || TEXT_TYPES.contains(&t.as_str()))
    {
        return ReadPlan::TextOnly;
    }
    ReadPlan::Nothing
}

/// Parse the macOS list step: [`MACOS_TYPE_TAG`] and [`MACOS_FILE_TAG`] lines.
/// Anything else is skipped (fail-soft).
fn parse_macos_list(out: &str) -> (Vec<String>, Vec<PathBuf>) {
    let mut types = Vec::new();
    let mut files = Vec::new();
    for line in out.lines() {
        if let Some(t) = line.strip_prefix(MACOS_TYPE_TAG) {
            types.push(t.to_owned());
        } else if let Some(p) = line.strip_prefix(MACOS_FILE_TAG) {
            if !p.is_empty() {
                files.push(PathBuf::from(p));
            }
        }
    }
    (types, files)
}

/// Parse a newline-separated type listing (`wl-paste --list-types`, `xclip -t
/// TARGETS -o`): trimmed, blank lines dropped.
fn parse_type_lines(out: &str) -> Vec<String> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Parse a `text/uri-list` (RFC 2483): `#` lines are comments, lines end in CRLF
/// or LF, only `file://` URIs (empty host or `localhost`) are kept, and their
/// paths are percent-decoded. A URI with a malformed escape, or one that decodes
/// to non-UTF-8, is skipped ([`percent_decode`]).
fn parse_uri_list(out: &str) -> Vec<PathBuf> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let rest = l.strip_prefix(FILE_URI_SCHEME)?;
            let path = rest.strip_prefix(FILE_URI_LOCAL_HOST).unwrap_or(rest);
            path.starts_with('/').then(|| percent_decode(path))?
        })
        .map(PathBuf::from)
        .collect()
}

/// Decode `%XX` escapes, `XX` being exactly two hex digits. `None` for a malformed
/// escape (a `%` not followed by two hex digits: `%+5`, `%-1`, `%4`, a lone `%`)
/// and for a result that is not UTF-8. Refused, never kept literal: a conforming
/// producer escapes a `%` itself (RFC 3986 §2.4), so a bare one marks a corrupt or
/// non-conforming URI, and guessing at it could name a different file than the
/// one copied. Refusing attaches nothing for that URI, the direction AGENTS.md
/// FAIL-SOFT sets for clipboard output. Hand-rolled like [`base64_encode`].
pub(super) fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            // Checked digit by digit first: `from_str_radix` alone takes a sign
            // (`+5`), which is no hex escape.
            let hex = s
                .get(i + 1..i + 3)
                .filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Why a read child produced no output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunError {
    /// The program could not be spawned.
    NotInstalled,
    /// It outlived its timeout and was killed with its group, or its stdout stayed
    /// open [`READ_STDOUT_GRACE`] past its end.
    TimedOut,
    /// It exited non-zero: `Some(code)`, or `None` when a signal ended it.
    Exited(Option<i32>),
    /// It wrote more than the byte cap.
    TooLarge,
}

/// Run a read child with stdin and stderr nulled (never the TTY) and stdout
/// piped, bounded by `timeout` and `max` bytes. The child leads a process group of
/// its own, and on expiry that whole group AND the child are killed before the
/// child is reaped, so a grandchild holding stdout dies with it. The wait for the
/// stdout reader is bounded too ([`READ_STDOUT_GRACE`]), so nothing a child leaves
/// behind can pin the worker.
fn run_capture(cmd: &ReadCmd, max: usize, timeout: Duration) -> Result<Vec<u8>, RunError> {
    let mut command = Command::new(&cmd.program);
    command
        .args(&cmd.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Applied in the child before `exec`, so the group exists before anything below
    // can signal it. No read child reads the TTY, so leaving the board's foreground
    // group costs nothing.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(crate::claude_catalog::NEW_PROCESS_GROUP);
    }
    let mut child = command.spawn().map_err(|_| RunError::NotInstalled)?;
    let stdout = child.stdout.take();
    // Read one byte past the cap so "exactly max" and "over" tell apart; the
    // reader dropping the pipe also ends a child that would write more. The bytes
    // come back over a channel, so the wait for them below has a bound.
    let (bytes_tx, bytes_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(out) = stdout {
            let _ = out.take(max as u64 + 1).read_to_end(&mut buf);
        }
        let _ = bytes_tx.send(buf);
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => break None,
            Ok(None) => std::thread::sleep(READ_POLL_INTERVAL),
            Err(_) => break None,
        }
    };
    if status.is_none() {
        // Both kills precede the reap, so the group id is still this child's pid
        // (`kill_group`'s contract). The direct kill takes a leader that moved
        // itself into another group, which the group kill misses.
        crate::claude_catalog::kill_group(&mut child);
        let _ = child.kill();
        let _ = child.wait();
    }
    let Ok(buf) = bytes_rx.recv_timeout(READ_STDOUT_GRACE) else {
        return Err(RunError::TimedOut);
    };
    if buf.len() > max {
        return Err(RunError::TooLarge);
    }
    match status {
        None => Err(RunError::TimedOut),
        Some(s) if s.success() => Ok(buf),
        Some(s) => Err(RunError::Exited(s.code())),
    }
}

/// `wl-paste`'s exit when nothing is copied: `bail("Nothing is copied")`, which is
/// `exit(1)` (wl-clipboard 2.3.0, `src/wl-paste.c` and `src/util/misc.h`). Every
/// other `bail` exits 1 too (no compositor to connect to, for one), so this code
/// reads as an empty clipboard and only another one as a failure.
const WL_PASTE_NOTHING_COPIED_EXIT: i32 = 1;

/// `xclip -o`'s exit when the CLIPBOARD has no owner, or its owner cannot convert
/// the asked target: `errconvsel`, which is `exit(EXIT_FAILURE)`, so 1 (xclip
/// 0.13, `xclip.c` and `xcprint.c`). `Can't open display` exits 1 too.
const XCLIP_NO_OWNER_EXIT: i32 = 1;

/// What a LIST step's exit means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListExit {
    /// Exit 0: its stdout is the listing.
    Listed,
    /// The tool's own exit for an empty clipboard.
    Empty,
    /// Any other exit, or a signal: the read failed.
    Failed,
}

/// Classify a list step's exit for `kind`: `code` is its exit code, `None` when a
/// signal ended it. PURE, table-tested. Only `wl-paste` and `xclip` have an exit
/// that means an EMPTY clipboard. `osascript` has none: on an empty pasteboard
/// [`MACOS_LIST_SCRIPT`] prints an empty listing and exits 0, so any non-zero exit
/// of it is a failure.
fn classify_list_exit(kind: ReadBackendKind, code: Option<i32>) -> ListExit {
    let empty = match kind {
        ReadBackendKind::Osascript => None,
        ReadBackendKind::WlPaste => Some(WL_PASTE_NOTHING_COPIED_EXIT),
        ReadBackendKind::Xclip => Some(XCLIP_NO_OWNER_EXIT),
    };
    match code {
        Some(0) => ListExit::Listed,
        Some(_) if code == empty => ListExit::Empty,
        _ => ListExit::Failed,
    }
}

/// Whether a cached file of `age` is past `ttl`. Pure, so the prune's one
/// decision is table-tested.
fn is_expired(age: Duration, ttl: Duration) -> bool {
    age > ttl
}

/// Whether `name` is one the paste cache writes: a final name (starting with
/// [`PASTE_FILE_PREFIX`]) or the temp form of one ([`PASTE_TEMP_LEAD`], then that
/// prefix). PURE, table-tested: [`prune_expired`] deletes nothing else.
fn is_paste_cache_name(name: &str) -> bool {
    name.strip_prefix(PASTE_TEMP_LEAD)
        .unwrap_or(name)
        .starts_with(PASTE_FILE_PREFIX)
}

/// Whether [`spawn_paste_image_discard`] may delete `path`: a name
/// [`is_paste_cache_name`] accepts, DIRECTLY in `cache_dir` made absolute the way
/// [`save_image`] makes it, so every `Image` path it returns qualifies. PURE,
/// table-tested, and decided on path components alone (nothing is resolved on
/// disk), so a `..`, a sub-dir, another dir, a relative path or a path with no
/// file name is never discardable.
fn is_discardable_paste_image(path: &Path, cache_dir: &Path) -> bool {
    let named_ours = path
        .file_name()
        .is_some_and(|name| is_paste_cache_name(&name.to_string_lossy()));
    named_ours
        && std::path::absolute(cache_dir).is_ok_and(|dir| path.parent() == Some(dir.as_path()))
}

/// Delete the regular files directly in `dir` older than `ttl` whose names
/// [`is_paste_cache_name`] accepts. The ONE prune: the read worker runs it before
/// each image write, and [`spawn_paste_cache_prune`] at every board start.
/// Fail-soft (a missing or unreadable dir or entry is skipped, and nothing panics)
/// and confined to `dir`: it never recurses, never follows a symlink, and never
/// touches a foreign file a misconfigured config dir might hold.
fn prune_expired(dir: &Path, ttl: Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        // `DirEntry::metadata` does not follow a symlink, so a link is not a file.
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() || !is_paste_cache_name(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let age = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok());
        if age.is_some_and(|a| is_expired(a, ttl)) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Prune the paste cache in `dir` ([`prune_expired`] under [`PASTE_CACHE_TTL`]) on
/// a detached thread of its own: the board-start half of the prune, so a
/// screenshot cannot outlive the TTL for want of a later image write. It reports
/// no event and nothing waits on it. The handle is returned only so a test can
/// join it; `tui::run_inner` drops it, which detaches the thread. `dir` is a
/// parameter so no test ever prunes the real cache.
pub fn spawn_paste_cache_prune(dir: PathBuf) -> JoinHandle<()> {
    std::thread::spawn(move || prune_expired(&dir, PASTE_CACHE_TTL))
}

/// Delete the cached images of clipboard results the compose DROPPED
/// (`compose::take_stale_paste_images`), on a detached thread of its own: the
/// shape of [`spawn_paste_cache_prune`], reporting no event, nothing waiting on
/// it. Only a path [`is_discardable_paste_image`] accepts that is a REGULAR file
/// is deleted, and every error is ignored, so whatever it cannot delete is left
/// to the prune. The handle is returned only so a test can join it;
/// `tui::run_inner` drops it. `cache_dir` is a parameter so no test ever deletes
/// from the real cache.
pub fn spawn_paste_image_discard(paths: Vec<PathBuf>, cache_dir: PathBuf) -> JoinHandle<()> {
    std::thread::spawn(move || {
        for path in paths
            .iter()
            .filter(|p| is_discardable_paste_image(p, &cache_dir))
        {
            // `symlink_metadata` does not follow a symlink, so a link is not a file.
            if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file()) {
                let _ = std::fs::remove_file(path);
            }
        }
    })
}

/// Create the paste dir if absent, owner-only on unix.
fn ensure_paste_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(PASTE_DIR_MODE))?;
    }
    Ok(())
}

/// Fetch the clipboard `image` and write it ATOMICALLY (temp name, then rename, the
/// way `hidden::save_hidden` does) to `dir/paste-<pid>-<nanos>.<ext>`. A final
/// path no mention can carry is [`ClipboardAttach::Unmentionable`], and nothing
/// is written for it.
fn save_image(backend: &ReadBackend, image: ImageType, dir: &Path) -> ClipboardAttach {
    // A backend with no PNG conversion (Linux) never lists a type that needs one.
    let template = match image.fetch {
        ImageFetch::Raw => Some(&backend.image),
        ImageFetch::ConvertToPng => backend.png_image.as_ref(),
    };
    let Some(template) = template else {
        return ClipboardAttach::Failed;
    };
    let Ok(dir) = std::path::absolute(dir) else {
        return ClipboardAttach::Failed;
    };
    let dir = dir.as_path();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let name = format!(
        "{PASTE_FILE_PREFIX}{}-{nanos}.{}",
        std::process::id(),
        image.ext
    );
    let target = dir.join(&name);
    // Asked BEFORE the dir, the temp file or the image is written: DOMAIN.md,
    // "The paste cache", owns why.
    if crate::tui::complete::file_mention(&target).is_err() {
        return ClipboardAttach::Unmentionable(target);
    }
    if ensure_paste_dir(dir).is_err() {
        return ClipboardAttach::Failed;
    }
    prune_expired(dir, PASTE_CACHE_TTL);
    let tmp = dir.join(format!("{PASTE_TEMP_LEAD}{name}{PASTE_TEMP_SUFFIX}"));
    let cmd = template.substituted(image.name, &tmp.to_string_lossy());
    let result = match run_capture(&cmd, PASTE_IMAGE_MAX_BYTES, backend.timeout) {
        Ok(bytes) => match backend.sink {
            ImageSink::Stdout if bytes.is_empty() => Err(ClipboardAttach::Failed),
            ImageSink::Stdout => std::fs::write(&tmp, bytes).map_err(|_| ClipboardAttach::Failed),
            ImageSink::File => Ok(()),
        },
        Err(RunError::TooLarge) => Err(ClipboardAttach::TooLarge),
        Err(RunError::NotInstalled) => Err(ClipboardAttach::NoTool),
        Err(RunError::TimedOut | RunError::Exited(_)) => Err(ClipboardAttach::Failed),
    };
    let outcome = result.and_then(|()| match std::fs::metadata(&tmp) {
        Ok(m) if m.len() > PASTE_IMAGE_MAX_BYTES as u64 => Err(ClipboardAttach::TooLarge),
        Ok(m) if m.len() == 0 => Err(ClipboardAttach::Failed),
        Ok(_) => std::fs::rename(&tmp, &target).map_err(|_| ClipboardAttach::Failed),
        Err(_) => Err(ClipboardAttach::Failed),
    });
    match outcome {
        Ok(()) => ClipboardAttach::Image(target),
        Err(attach) => {
            let _ = std::fs::remove_file(&tmp);
            attach
        }
    }
}

/// How far a clipboard read goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadMode {
    /// List, then save an image if that is what the clipboard holds.
    Attach,
    /// List only: report copied files, and anything else as
    /// [`ClipboardAttach::Nothing`]. Never writes to disk, so a silent file-paste
    /// check cannot leave a screenshot behind.
    ListOnly,
}

/// The whole read: list, choose, then (for an image, in [`ReadMode::Attach`]) save.
/// Every failure maps to a [`ClipboardAttach`] variant, never a panic.
fn read_clipboard(backend: &ReadBackend, dir: &Path, mode: ReadMode) -> ClipboardAttach {
    let listing = match run_capture(&backend.list, LIST_MAX_BYTES, backend.timeout) {
        Ok(bytes) => bytes,
        Err(RunError::NotInstalled) => return ClipboardAttach::NoTool,
        Err(RunError::Exited(code))
            if classify_list_exit(backend.kind, code) == ListExit::Empty =>
        {
            return ClipboardAttach::Nothing;
        }
        Err(RunError::Exited(_) | RunError::TimedOut | RunError::TooLarge) => {
            return ClipboardAttach::Failed;
        }
    };
    let text = String::from_utf8_lossy(&listing);
    let (types, files) = match backend.format {
        ListFormat::MacOs => parse_macos_list(&text),
        ListFormat::TypeLines => {
            let types = parse_type_lines(&text);
            let files = match &backend.uri_list {
                Some(cmd) if types.iter().any(|t| t == URI_LIST_TYPE) => {
                    run_capture(cmd, LIST_MAX_BYTES, backend.timeout)
                        .map(|b| parse_uri_list(&String::from_utf8_lossy(&b)))
                        .unwrap_or_default()
                }
                _ => Vec::new(),
            };
            (types, files)
        }
    };
    match choose_read(&types, &files) {
        ReadPlan::Files => ClipboardAttach::Files(files),
        _ if mode == ReadMode::ListOnly => ClipboardAttach::Nothing,
        ReadPlan::Image(image) => save_image(backend, image, dir),
        ReadPlan::UnsupportedImage => ClipboardAttach::UnsupportedImage,
        ReadPlan::TextOnly => ClipboardAttach::TextOnly,
        ReadPlan::Nothing => ClipboardAttach::Nothing,
    }
}

/// Read the clipboard on its OWN thread and deliver exactly one
/// [`AppEvent::ClipboardRead`] for `token`. The shape of [`spawn_tool_copy`]: the
/// `tui::run_inner` driver starts it, the worker never touches the terminal, and a
/// failed send is ignored because the board went away. `backend` is a parameter so
/// a test hands in `sh` stand-ins instead of a real clipboard tool.
pub fn spawn_clipboard_read(
    backend: ReadBackend,
    dest_dir: PathBuf,
    token: u64,
    mode: ReadMode,
    tx: Sender<AppEvent>,
) {
    std::thread::spawn(move || {
        let result = read_clipboard(&backend, &dest_dir, mode);
        let _ = tx.send(AppEvent::ClipboardRead { token, result });
    });
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
            spawn_tool_copy(tools, CopyPayload::SessionId(ID.to_string()), tx);
            match rx.recv_timeout(WORKER_TIMEOUT) {
                Ok(AppEvent::CopyFinished {
                    payload,
                    copied: got,
                }) => {
                    assert_eq!(payload, CopyPayload::SessionId(ID.to_string()));
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

    /// A drag-SELECTION rides the same worker: its text — several rows and a
    /// non-ASCII character — reaches the tool's stdin byte for byte, and the event
    /// hands the SAME payload back, still a `Selection`, so the status can say what
    /// was copied. The stand-in exits 0 only on the exact bytes, so a pass proves
    /// the whole multi-line text travelled and nothing was trimmed or re-encoded.
    #[test]
    fn a_selection_travels_the_worker_whole_and_comes_back_as_a_selection() {
        const ACCEPTS_SELECTION: ClipboardTool = ClipboardTool {
            program: "sh",
            args: &[
                "-c",
                "[ \"$(cat)\" = \"$(printf 'caf\\303\\251 row\\n\\nlast')\" ]",
            ],
        };
        let text = "café row\n\nlast";
        assert!(
            run_tool(ACCEPTS_SELECTION, text),
            "the stand-in must accept the exact selection bytes"
        );
        assert!(
            !run_tool(ACCEPTS_SELECTION, "cafe row\n\nlast"),
            "and must actually check them"
        );

        let (tx, rx) = mpsc::channel();
        spawn_tool_copy(
            &[ACCEPTS_SELECTION],
            CopyPayload::Selection(text.to_string()),
            tx,
        );
        match rx.recv_timeout(WORKER_TIMEOUT) {
            Ok(AppEvent::CopyFinished { payload, copied }) => {
                assert_eq!(payload, CopyPayload::Selection(text.to_string()));
                assert!(copied, "the selection's text reached the tool whole");
            }
            other => panic!("expected one CopyFinished, got {other:?}"),
        }
    }

    /// A selection is arbitrary UTF-8, not an ASCII UUID: the encoder takes its RAW
    /// bytes (`é` is `0xC3 0xA9`), which is what a terminal decodes back.
    #[test]
    fn base64_encode_takes_the_raw_bytes_of_non_ascii_text() {
        assert_eq!(base64_encode("é".as_bytes()), "w6k=");
        assert_eq!(base64_encode("a\nb".as_bytes()), "YQpi");
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

    // --- clipboard READ ----------------------------------------------------

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn the_read_route_picks_each_tool_by_session_os_and_display() {
        use super::TargetOs::{Linux, MacOs, Other};
        use ReadBackendKind::{Osascript, WlPaste, Xclip};
        let rows = [
            (
                env(true, MacOs, false, false),
                ReadRoute::RemoteSession,
                "SSH on a Mac",
            ),
            (
                env(true, Linux, true, true),
                ReadRoute::RemoteSession,
                "SSH on Linux",
            ),
            (
                env(false, MacOs, false, false),
                ReadRoute::Backend(Osascript),
                "local macOS",
            ),
            (
                env(false, Linux, true, true),
                ReadRoute::Backend(WlPaste),
                "Wayland wins",
            ),
            (
                env(false, Linux, false, true),
                ReadRoute::Backend(Xclip),
                "X11 only",
            ),
            (
                env(false, Linux, false, false),
                ReadRoute::NoDisplay,
                "headless Linux",
            ),
            (
                env(false, Other, false, true),
                ReadRoute::NoDisplay,
                "another OS",
            ),
        ];
        for (facts, expected, case) in rows {
            assert_eq!(clipboard_read_route(facts), expected, "{case}");
        }
    }

    #[test]
    fn choose_read_orders_files_then_image_then_unsupported_then_text() {
        use ImageFetch::{ConvertToPng, Raw};
        let image = |name, ext, fetch| ReadPlan::Image(ImageType { name, ext, fetch });
        let png = image("image/png", "png", Raw);
        let jpeg = image("image/jpeg", "jpg", Raw);
        let some_file = vec![PathBuf::from("/tmp/a.pdf")];
        let rows: [(&[&str], &[PathBuf], ReadPlan, &str); 24] = [
            (
                &["text/uri-list", "text/plain"],
                &some_file,
                ReadPlan::Files,
                "files win over text",
            ),
            (
                &["public.file-url", "public.png"],
                &some_file,
                ReadPlan::Files,
                "Finder file URL plus an icon image",
            ),
            (&["image/jpeg", "image/png"], &[], png, "png beats jpeg"),
            (&["image/webp", "image/jpeg"], &[], jpeg, "jpeg beats webp"),
            (&["image/gif", "image/png"], &[], png, "Linux png beats gif"),
            (
                &["image/gif"],
                &[],
                image("image/gif", "gif", Raw),
                "Linux gif only",
            ),
            (
                &["image/webp"],
                &[],
                image("image/webp", "webp", Raw),
                "Linux webp only",
            ),
            (
                &["public.tiff"],
                &[],
                image("public.tiff", "png", ConvertToPng),
                "macOS tiff converts to png",
            ),
            (
                &["com.compuserve.gif"],
                &[],
                image("com.compuserve.gif", "gif", Raw),
                "macOS gif only is read raw",
            ),
            (
                &["org.webmproject.webp"],
                &[],
                image("org.webmproject.webp", "webp", Raw),
                "macOS webp only is read raw",
            ),
            (
                &["com.compuserve.gif", "public.png"],
                &[],
                image("public.png", "png", ConvertToPng),
                "macOS png beats gif",
            ),
            (
                &["org.webmproject.webp", "public.jpeg"],
                &[],
                image("public.jpeg", "png", ConvertToPng),
                "macOS jpeg beats webp",
            ),
            (
                &["com.compuserve.gif", "public.tiff"],
                &[],
                image("com.compuserve.gif", "gif", Raw),
                "macOS gif beats the tiff macOS synthesizes beside it",
            ),
            (
                &["org.webmproject.webp", "public.tiff"],
                &[],
                image("org.webmproject.webp", "webp", Raw),
                "macOS webp beats the tiff macOS synthesizes beside it",
            ),
            (&["image/bmp"], &[], ReadPlan::UnsupportedImage, "only bmp"),
            (
                &["image/bmp", "text/plain"],
                &[],
                ReadPlan::UnsupportedImage,
                "bmp beats text",
            ),
            (
                &["com.microsoft.bmp"],
                &[],
                ReadPlan::UnsupportedImage,
                "macOS bmp with no tiff beside it is unsupported, never empty",
            ),
            (
                &["public.heic", "public.utf8-plain-text"],
                &[],
                ReadPlan::UnsupportedImage,
                "macOS heic beats text",
            ),
            (
                &["com.microsoft.bmp", "public.tiff"],
                &[],
                image("public.tiff", "png", ConvertToPng),
                "macOS bmp with a tiff beside it converts the tiff",
            ),
            (
                &["com.adobe.pdf"],
                &[],
                ReadPlan::Nothing,
                "macOS pdf data is no image",
            ),
            (
                &["text/html"],
                &[],
                ReadPlan::TextOnly,
                "any text/* target is text",
            ),
            (
                &["UTF8_STRING", "TARGETS"],
                &[],
                ReadPlan::TextOnly,
                "x11 text",
            ),
            (
                &["public.utf8-plain-text"],
                &[],
                ReadPlan::TextOnly,
                "macOS text",
            ),
            (
                &["application/x-weird"],
                &[],
                ReadPlan::Nothing,
                "unknown only",
            ),
        ];
        for (types, files, expected, case) in rows {
            assert_eq!(choose_read(&strings(types), files), expected, "{case}");
        }
        assert_eq!(choose_read(&[], &[]), ReadPlan::Nothing, "empty clipboard");
    }

    #[test]
    fn the_listing_parsers_read_each_tools_output() {
        assert_eq!(
            parse_macos_list("T\tpublic.png\nF\t/a b/c.txt\nF\t\njunk\nF\t/d\n"),
            (
                strings(&["public.png"]),
                vec![PathBuf::from("/a b/c.txt"), PathBuf::from("/d")]
            )
        );
        assert_eq!(
            parse_type_lines("image/png\n\n  text/plain \n"),
            strings(&["image/png", "text/plain"])
        );
        let uris = "# comment\r\nfile:///tmp/a%20b.png\r\nhttp://x/y\r\nfile://localhost/tmp/%E2%82%AC\r\nfile://remote/x\r\nfile:///bad%FF\r\nfile:///pct%zz\r\nfile:///sign%+5\r\nfile:///tmp/ok%2epng\r\n";
        assert_eq!(
            parse_uri_list(uris),
            vec![
                PathBuf::from("/tmp/a b.png"),
                PathBuf::from("/tmp/\u{20ac}"),
                PathBuf::from("/tmp/ok.png"),
            ],
            "a URI with a malformed escape is skipped, never kept literal"
        );
    }

    /// A `%` escape is EXACTLY two hex digits, of either case; anything else after
    /// a `%` (a sign `from_str_radix` would take, one digit, none) refuses the whole
    /// string rather than decoding to a different path.
    #[test]
    fn percent_decode_takes_exactly_two_hex_digits_and_refuses_the_rest() {
        let rows: [(&str, Option<&str>, &str); 14] = [
            ("%41", Some("A"), "an upper-case escape"),
            ("%4a", Some("J"), "a lower-case escape"),
            ("%4A", Some("J"), "the same escape upper-case"),
            (
                "%e2%82%AC",
                Some("\u{20ac}"),
                "mixed case across a UTF-8 sequence",
            ),
            ("a%20b", Some("a b"), "an escape between text"),
            ("%2541", Some("%41"), "decoded once, never twice"),
            ("plain", Some("plain"), "no escape at all"),
            ("%+5", None, "a plus sign is no hex digit"),
            ("%-1", None, "a minus sign is no hex digit"),
            ("%", None, "a lone percent"),
            ("a%", None, "a trailing percent"),
            ("%4", None, "one digit"),
            ("%zz", None, "no hex digit"),
            ("%FF", None, "a byte that is not UTF-8"),
        ];
        // Collected, so a loosened rule names every row it flips.
        let wrong: Vec<String> = rows
            .into_iter()
            .filter_map(|(input, want, case)| {
                let got = percent_decode(input);
                (got.as_deref() != want)
                    .then(|| format!("{case}: {input:?} gave {got:?}, want {want:?}"))
            })
            .collect();
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[test]
    fn is_expired_is_strictly_older_than_the_ttl() {
        let ttl = Duration::from_secs(10);
        assert!(!is_expired(Duration::from_secs(10), ttl));
        assert!(is_expired(Duration::from_secs(11), ttl));
    }

    /// A unique scratch dir, never the real cache.
    fn scratch(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "snapback-clip-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sh(script: &str) -> ReadCmd {
        ReadCmd::new("sh", &["-c", script])
    }

    /// A stand-in backend: `list` and `uri` print fixed text, `image` writes bytes.
    /// It is `wl-paste`-shaped, so its list exit reads as `wl-paste`'s.
    fn stand_in(list: &str, uri: Option<&str>, image: &str) -> ReadBackend {
        ReadBackend {
            kind: ReadBackendKind::WlPaste,
            list: sh(list),
            format: ListFormat::TypeLines,
            uri_list: uri.map(sh),
            image: sh(image),
            png_image: None,
            sink: ImageSink::Stdout,
            timeout: Duration::from_secs(5),
        }
    }

    /// The Linux PNG target, the image a stand-in saves.
    const LINUX_PNG: ImageType = ImageType::raw("image/png", "png");

    fn read_once(backend: ReadBackend, dir: &Path) -> ClipboardAttach {
        read_once_as(backend, dir, ReadMode::Attach)
    }

    fn read_once_as(backend: ReadBackend, dir: &Path, mode: ReadMode) -> ClipboardAttach {
        let (tx, rx) = mpsc::channel();
        spawn_clipboard_read(backend, dir.to_path_buf(), 7, mode, tx);
        let AppEvent::ClipboardRead { token, result } =
            rx.recv_timeout(WORKER_TIMEOUT).expect("one event")
        else {
            panic!("expected ClipboardRead");
        };
        assert_eq!(token, 7);
        assert!(
            rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "exactly one event per read"
        );
        result
    }

    #[test]
    fn the_worker_reports_files_an_image_nothing_and_a_hang_once_each() {
        let dir = scratch("worker");
        let files = read_once(
            stand_in(
                "printf 'text/uri-list\\ntext/plain\\n'",
                Some("printf 'file:///tmp/x%%20y.pdf\\r\\n'"),
                "exit 9",
            ),
            &dir,
        );
        assert_eq!(
            files,
            ClipboardAttach::Files(vec![PathBuf::from("/tmp/x y.pdf")])
        );

        let ClipboardAttach::Image(path) =
            read_once(stand_in("echo image/png", None, "printf PNGBYTES"), &dir)
        else {
            panic!("expected an image");
        };
        assert_eq!(path.parent(), Some(dir.as_path()));
        assert_eq!(path.extension().unwrap(), "png");
        assert_eq!(std::fs::read(&path).unwrap(), b"PNGBYTES");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "no temp file left behind"
        );

        assert_eq!(
            read_once(stand_in("exit 1", None, "true"), &dir),
            ClipboardAttach::Nothing
        );
        assert_eq!(
            read_once(stand_in("echo text/plain", None, "true"), &dir),
            ClipboardAttach::TextOnly
        );
        assert_eq!(
            read_once(stand_in("echo image/bmp", None, "true"), &dir),
            ClipboardAttach::UnsupportedImage
        );
        assert_eq!(
            read_once(stand_in("echo image/png", None, "exit 1"), &dir),
            ClipboardAttach::Failed
        );

        let mut hang = stand_in("sleep 30", None, "true");
        hang.timeout = Duration::from_millis(200);
        assert_eq!(
            read_once(hang, &dir),
            ClipboardAttach::Failed,
            "a hung child is killed"
        );

        let mut missing = stand_in("true", None, "true");
        missing.list = ReadCmd::new("snapback-test-no-such-clipboard-tool", &[]);
        assert_eq!(read_once(missing, &dir), ClipboardAttach::NoTool);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_image_over_the_cap_is_too_large_and_leaves_nothing() {
        assert_eq!(
            run_capture(&sh("printf 12345678"), 4, Duration::from_secs(5)),
            Err(RunError::TooLarge)
        );
        assert_eq!(
            run_capture(&sh("printf 1234"), 4, Duration::from_secs(5)),
            Ok(b"1234".to_vec())
        );
        // File sink: a tool that writes past the cap to `{dest}`.
        let dir = scratch("cap");
        let mut backend = stand_in("true", None, "true");
        backend.sink = ImageSink::File;
        backend.image = ReadCmd::new(
            "sh",
            &[
                "-c",
                &format!("head -c {} /dev/zero > \"$0\"", PASTE_IMAGE_MAX_BYTES + 1),
                DEST_PLACEHOLDER,
            ],
        );
        assert_eq!(
            save_image(&backend, LINUX_PNG, &dir),
            ClipboardAttach::TooLarge
        );
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "the oversized file is deleted"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_deletes_only_expired_regular_files() {
        let dir = scratch("prune");
        let old = dir.join("paste-old.png");
        let fresh = dir.join("paste-fresh.png");
        std::fs::write(&old, b"x").unwrap();
        std::fs::write(&fresh, b"x").unwrap();
        let foreign = dir.join("notes.txt");
        let stale_tmp = dir.join(".paste-1-2.png.tmp");
        std::fs::write(&foreign, b"x").unwrap();
        std::fs::write(&stale_tmp, b"x").unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        let long_ago = SystemTime::now() - PASTE_CACHE_TTL - Duration::from_secs(60);
        for stale in [&old, &foreign, &stale_tmp] {
            std::fs::File::options()
                .write(true)
                .open(stale)
                .unwrap()
                .set_modified(long_ago)
                .unwrap();
        }
        prune_expired(&dir, PASTE_CACHE_TTL);
        assert!(!old.exists(), "expired file pruned");
        assert!(fresh.exists(), "fresh file kept");
        assert!(dir.join("sub").exists(), "subdirectories are never touched");
        assert!(
            foreign.exists(),
            "a file without the paste prefix is never ours to delete"
        );
        assert!(
            !stale_tmp.exists(),
            "an expired temp file of ours is pruned"
        );
        prune_expired(&dir.join("missing"), PASTE_CACHE_TTL);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A silent file-paste check (`ListOnly`) must never save an image: an
    /// image-only clipboard writes nothing and reports `Nothing`, while copied
    /// files are still reported.
    #[test]
    fn a_list_only_read_never_writes_an_image() {
        let dir = scratch("listonly");
        let image = stand_in("echo image/png", None, "printf PNGBYTES");
        assert_eq!(
            read_once_as(image, &dir, ReadMode::ListOnly),
            ClipboardAttach::Nothing
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "nothing saved");
        let files = stand_in(
            "echo text/uri-list",
            Some("printf 'file:///tmp/a.pdf\\r\\n'"),
            "exit 9",
        );
        assert_eq!(
            read_once_as(files, &dir, ReadMode::ListOnly),
            ClipboardAttach::Files(vec![PathBuf::from("/tmp/a.pdf")])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A relative cache dir is resolved before the write, so the saved image is
    /// absolute (a mention would refuse a relative path after the write).
    #[test]
    fn a_relative_cache_dir_saves_an_absolute_path() {
        let rel = PathBuf::from(format!("target/snapback-rel-{}", std::process::id()));
        let backend = stand_in("true", None, "printf PNGBYTES");
        let ClipboardAttach::Image(path) = save_image(&backend, LINUX_PNG, &rel) else {
            panic!("expected an image");
        };
        assert!(path.is_absolute());
        let _ = std::fs::remove_dir_all(&rel);
    }

    /// A cache path no mention can carry (here a `#` in the config dir) is refused
    /// BEFORE anything is created: not the cache dir, no temp file and no final
    /// file, so a refused image never sits in the cache and a retry never adds
    /// another. The cache dir does not exist beforehand, as on a first paste, so
    /// creating it first would show.
    #[test]
    fn a_cache_path_no_mention_can_carry_writes_nothing_and_names_the_path() {
        let root = scratch("unmentionable");
        let dir = root.join("hash#dir");
        let mut backend = stand_in("true", None, "true");
        backend.sink = ImageSink::File;
        backend.image = ReadCmd::new("sh", &["-c", "printf PNGBYTES > \"$0\"", DEST_PLACEHOLDER]);
        let saved = save_image(&backend, LINUX_PNG, &dir);
        let created = dir.exists();
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            !created,
            "the refusal must come before the cache dir is created"
        );
        let ClipboardAttach::Unmentionable(path) = &saved else {
            panic!("expected the refusal, got {saved:?}");
        };
        assert_eq!(path.parent(), Some(dir.as_path()), "the path it would take");
        assert_eq!(
            crate::tui::complete::file_mention(path),
            Err(crate::tui::complete::MentionRefusal::Hash)
        );
    }

    /// The JXA list turns a newline in a path into a control character, so the
    /// parser keeps the whole path and `file_mention` refuses it by name.
    #[test]
    fn a_macos_path_with_a_newline_is_kept_whole_and_refused_as_control() {
        assert!(MACOS_LIST_SCRIPT.contains("replace(/[\\n\\r]/g,'\\u0001')"));
        let (_, files) = parse_macos_list("F\t/tmp/a\u{1}b.png\nF\t/tmp/ok.png");
        assert_eq!(files.len(), 2);
        assert_eq!(
            crate::tui::complete::file_mention(&files[0]),
            Err(crate::tui::complete::MentionRefusal::Control)
        );
    }

    // --- board-start prune ------------------------------------------------

    /// The prune's name filter: the cache's final names and their temp form, and
    /// nothing else a misconfigured config dir might hold.
    #[test]
    fn only_the_paste_caches_own_names_are_prunable() {
        let rows = [
            ("paste-1-2.png", true, "a saved image"),
            (".paste-1-2.png.tmp", true, "its temp file"),
            (".paste-tmp", true, "a temp name"),
            ("paste-", true, "the bare prefix"),
            ("notes.txt", false, "a foreign file"),
            ("paste", false, "a `paste` entry without the dash"),
            (".paste", false, "the temp lead without the dash"),
            ("..paste-1.png", false, "two leads"),
            ("Paste-1.png", false, "case matters"),
            ("my-paste-1.png", false, "the prefix must lead"),
            ("", false, "an empty name"),
        ];
        for (name, ours, case) in rows {
            assert_eq!(is_paste_cache_name(name), ours, "{case}: {name:?}");
        }
    }

    /// Back-date `path`'s mtime past [`PASTE_CACHE_TTL`].
    fn expire(path: &Path) {
        let long_ago = SystemTime::now() - PASTE_CACHE_TTL - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
    }

    /// The board-start prune runs on a thread of its own and deletes only the
    /// cache's OWN expired files, directly in the dir: a fresh paste, a foreign
    /// file, a symlink and anything in a subdirectory all stay. A missing dir is
    /// skipped without a panic.
    #[test]
    fn the_board_start_prune_deletes_only_the_caches_expired_files() {
        let dir = scratch("board-start");
        let elsewhere = scratch("board-start-elsewhere");
        let expired = dir.join("paste-1-2.png");
        let expired_tmp = dir.join(".paste-1-3.gif.tmp");
        let fresh = dir.join("paste-1-4.png");
        let foreign = dir.join("notes.txt");
        let nested = dir.join("paste-dir").join("paste-1-5.png");
        let link_target = elsewhere.join("paste-1-6.png");
        std::fs::create_dir(dir.join("paste-dir")).unwrap();
        for file in [
            &expired,
            &expired_tmp,
            &fresh,
            &foreign,
            &nested,
            &link_target,
        ] {
            std::fs::write(file, b"x").unwrap();
        }
        for stale in [&expired, &expired_tmp, &foreign, &nested, &link_target] {
            expire(stale);
        }
        let link = dir.join("paste-link.png");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&link_target, &link).unwrap();

        spawn_paste_cache_prune(dir.clone())
            .join()
            .expect("the prune never panics");

        assert!(!expired.exists(), "an expired paste is pruned");
        assert!(!expired_tmp.exists(), "an expired temp file is pruned");
        assert!(fresh.exists(), "a fresh paste stays");
        assert!(foreign.exists(), "a foreign file is never ours to delete");
        assert!(nested.exists(), "the prune never recurses");
        #[cfg(unix)]
        assert!(
            std::fs::symlink_metadata(&link).is_ok(),
            "a symlink is never followed to an expired target"
        );
        assert!(
            spawn_paste_cache_prune(dir.join("missing")).join().is_ok(),
            "a missing dir is skipped, never a panic"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    // --- a dropped result's image -------------------------------------------

    /// Only a cache-named file DIRECTLY in the cache dir is ever discardable,
    /// compared by path components. Literal paths: nothing here touches a disk.
    #[test]
    fn only_a_cache_named_file_directly_in_the_cache_dir_is_discardable() {
        let cache = Path::new("/home/u/.config/snapback/cache/paste");
        let rows = [
            (
                "/home/u/.config/snapback/cache/paste/paste-1-2.png",
                true,
                "a saved image",
            ),
            (
                "/home/u/.config/snapback/cache/paste/.paste-1-2.png.tmp",
                true,
                "its temp form",
            ),
            (
                "/home/u/.config/snapback/cache/paste/notes.txt",
                false,
                "a foreign name in the dir",
            ),
            (
                "/home/u/.config/snapback/cache/paste/sub/paste-1-2.png",
                false,
                "a cache name in a sub-dir",
            ),
            (
                "/home/u/Desktop/paste-1-2.png",
                false,
                "a cache name in another dir",
            ),
            (
                "/home/u/.config/snapback/cache/paste/../paste/paste-1-2.png",
                false,
                "a `..` path",
            ),
            (
                "/home/u/.config/snapback/cache/paste/sub/../paste-1-2.png",
                false,
                "a `..` back into the dir",
            ),
            ("paste-1-2.png", false, "a bare relative name"),
            (
                "home/u/.config/snapback/cache/paste/paste-1-2.png",
                false,
                "a relative path",
            ),
            (
                "/home/u/.config/snapback/cache/paste/..",
                false,
                "no file name",
            ),
            ("/", false, "the root, no file name"),
            ("", false, "an empty path"),
        ];
        for (path, discardable, case) in rows {
            assert_eq!(
                is_discardable_paste_image(Path::new(path), cache),
                discardable,
                "{case}: {path:?}"
            );
        }
        let saved = Path::new("/home/u/.config/snapback/cache/paste/paste-1-2.png");
        assert!(
            is_discardable_paste_image(saved, Path::new("/home/u/.config/snapback/cache/paste/")),
            "a trailing slash on the dir changes nothing"
        );
        assert!(
            !is_discardable_paste_image(saved, Path::new("")),
            "a dir that cannot be made absolute discards nothing"
        );
        let cwd = std::env::current_dir().expect("a current dir");
        let relative_dir = Path::new("cache/paste");
        assert!(
            is_discardable_paste_image(&cwd.join("cache/paste/paste-1-2.png"), relative_dir),
            "a relative dir is made absolute the way the save makes it"
        );
        assert!(
            !is_discardable_paste_image(Path::new("cache/paste/paste-1-2.png"), relative_dir),
            "a relative path stays relative"
        );
    }

    /// The stale-result discard runs on a thread of its own and deletes only a
    /// cache-named REGULAR file directly in the dir: a foreign file, a cache-named
    /// file in another dir or a sub-dir, and a symlink (and so its target) all
    /// stay. A path that does not exist is skipped without a panic.
    #[test]
    fn the_stale_image_discard_deletes_only_a_cache_named_regular_file_in_the_dir() {
        let dir = scratch("discard");
        let elsewhere = scratch("discard-elsewhere");
        let stale = dir.join("paste-1-2.png");
        let foreign = dir.join("notes.txt");
        let other_dir = elsewhere.join("paste-1-3.png");
        let nested = dir.join("sub").join("paste-1-4.png");
        let link_target = elsewhere.join("paste-1-5.png");
        std::fs::create_dir(dir.join("sub")).unwrap();
        for file in [&stale, &foreign, &other_dir, &nested, &link_target] {
            std::fs::write(file, b"x").unwrap();
        }
        let link = dir.join("paste-link.png");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&link_target, &link).unwrap();
        let missing = dir.join("paste-1-6.png");

        spawn_paste_image_discard(
            vec![
                stale.clone(),
                foreign.clone(),
                other_dir.clone(),
                nested.clone(),
                link.clone(),
                missing,
            ],
            dir.clone(),
        )
        .join()
        .expect("the discard never panics");

        assert!(!stale.exists(), "a cache-named file in the dir is deleted");
        assert!(foreign.exists(), "a foreign file is never ours to delete");
        assert!(other_dir.exists(), "another dir is never touched");
        assert!(nested.exists(), "a sub-dir is never touched");
        #[cfg(unix)]
        assert!(
            std::fs::symlink_metadata(&link).is_ok(),
            "a symlink is not a regular file, so it stays"
        );
        assert!(link_target.exists(), "a symlink is never followed");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    // --- list-step exit, per backend ----------------------------------------

    /// Each backend × {exit 0, the code `wl-paste` and `xclip` document for an
    /// empty clipboard, another non-zero code, a signal}. Only that documented
    /// code from those two tools is an empty clipboard; `osascript` has no such
    /// code, so its exit 1 is a failure like any other.
    #[test]
    fn each_backend_reads_only_its_own_empty_clipboard_exit_as_empty() {
        use ListExit::{Empty, Failed, Listed};
        use ReadBackendKind::{Osascript, WlPaste, Xclip};
        let rows = [
            (Osascript, Some(0), Listed),
            (Osascript, Some(1), Failed),
            (Osascript, Some(2), Failed),
            (Osascript, None, Failed),
            (WlPaste, Some(0), Listed),
            (WlPaste, Some(1), Empty),
            (WlPaste, Some(2), Failed),
            (WlPaste, None, Failed),
            (Xclip, Some(0), Listed),
            (Xclip, Some(1), Empty),
            (Xclip, Some(2), Failed),
            (Xclip, None, Failed),
        ];
        for (kind, code, expected) in rows {
            assert_eq!(
                classify_list_exit(kind, code),
                expected,
                "{kind:?} exiting {code:?}"
            );
        }
    }

    /// The worker classifies a list exit by its backend's OWN kind: exit 1 from a
    /// `wl-paste`-shaped list is an empty clipboard, the same exit from an
    /// `osascript` one is a failure, and another code or a signal fails either way.
    #[test]
    fn the_worker_classifies_a_list_exit_by_its_backend() {
        let dir = scratch("list-exit");
        assert_eq!(
            read_once(stand_in("exit 1", None, "true"), &dir),
            ClipboardAttach::Nothing
        );
        let mut mac = stand_in("exit 1", None, "true");
        mac.kind = ReadBackendKind::Osascript;
        assert_eq!(read_once(mac, &dir), ClipboardAttach::Failed);
        assert_eq!(
            read_once(stand_in("exit 2", None, "true"), &dir),
            ClipboardAttach::Failed
        );
        assert_eq!(
            read_once(stand_in("kill -9 $$", None, "true"), &dir),
            ClipboardAttach::Failed,
            "a list step a signal ended"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- a grandchild cannot hang the worker --------------------------------

    /// Run one read on the worker and take its event within [`WORKER_TIMEOUT`]:
    /// the result (`None` when no event came) and how long it took. Unlike
    /// [`read_once`] it never panics, so a caller can clean up before asserting.
    fn read_timed(backend: ReadBackend, dir: &Path) -> (Option<ClipboardAttach>, Duration) {
        let started = Instant::now();
        let (tx, rx) = mpsc::channel();
        spawn_clipboard_read(backend, dir.to_path_buf(), 7, ReadMode::Attach, tx);
        let first = rx.recv_timeout(WORKER_TIMEOUT);
        let took = started.elapsed();
        let result = match first {
            Ok(AppEvent::ClipboardRead { token: 7, result }) => Some(result),
            _ => None,
        };
        let only_one = rx.recv_timeout(Duration::from_millis(50)).is_err();
        (result.filter(|_| only_one), took)
    }

    /// The pid a stand-in recorded in `file`, if it got that far.
    #[cfg(unix)]
    fn recorded_pid(file: &Path) -> Option<libc::pid_t> {
        std::fs::read_to_string(file).ok()?.trim().parse().ok()
    }

    /// Whether `pid` is gone within `within`. A killed process can linger briefly
    /// as a zombie until its new parent reaps it, so this polls.
    #[cfg(unix)]
    fn gone_within(pid: libc::pid_t, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        loop {
            // SAFETY: signal 0 delivers nothing; it only asks whether `pid` exists.
            let gone = unsafe { libc::kill(pid, 0) } != 0
                && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            if gone || Instant::now() >= deadline {
                return gone;
            }
            std::thread::sleep(READ_POLL_INTERVAL);
        }
    }

    /// SIGKILL a stand-in's leftover `pid`, so no failed run leaves it behind.
    #[cfg(unix)]
    fn kill_leftover(pid: libc::pid_t) {
        // SAFETY: two scalars; `pid` is a `sleep` this test started.
        let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
    }

    /// A timed-out read kills its child's WHOLE process group. The stand-in's `sh`
    /// starts `sleep` in the background, records its pid and waits on it, so the
    /// `sleep` is a GRANDCHILD holding stdout. The worker must report exactly one
    /// `Failed` at the timeout, and the grandchild must be gone: killing the child
    /// alone leaves it alive, which the pid check sees.
    #[cfg(unix)]
    #[test]
    fn a_timed_out_read_kills_the_grandchild_holding_its_stdout() {
        const TIMEOUT: Duration = Duration::from_millis(300);
        /// The timeout, plus [`READ_STDOUT_GRACE`], plus slack for a loaded
        /// machine, and far below the grandchild's own 30 s.
        const RETURNS_WITHIN: Duration = Duration::from_secs(3);
        /// How long a killed grandchild may stay a zombie before it is reaped.
        const REAPED_WITHIN: Duration = Duration::from_secs(2);

        let dir = scratch("group-kill");
        let pid_file = dir.join("grandchild.pid");
        let mut hang = stand_in("true", None, "true");
        hang.list = ReadCmd::new(
            "sh",
            &[
                "-c",
                "sleep 30 & echo $! > \"$0\"; wait",
                pid_file.to_str().expect("a UTF-8 temp path"),
            ],
        );
        hang.timeout = TIMEOUT;

        let (result, took) = read_timed(hang, &dir);
        let grandchild = recorded_pid(&pid_file);
        let gone = grandchild.is_some_and(|pid| gone_within(pid, REAPED_WITHIN));
        if let Some(pid) = grandchild.filter(|_| !gone) {
            kill_leftover(pid);
        }

        assert_eq!(result, Some(ClipboardAttach::Failed), "exactly one Failed");
        assert!(
            took < RETURNS_WITHIN,
            "the read must end at its timeout: {took:?}"
        );
        assert!(
            grandchild.is_some(),
            "the stand-in must have started its grandchild before the timeout"
        );
        assert!(
            gone,
            "the grandchild holding stdout must die with its process group"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The wait for a read's stdout is BOUNDED. The stand-in's `sh` exits 0 at
    /// once but leaves a background `sleep` holding stdout. Nothing is killed (the
    /// child ended by itself, and its group is never signalled once it is reaped),
    /// so only [`READ_STDOUT_GRACE`] ends the wait; without it the worker would
    /// hang for the `sleep`'s 30 s, and the compose's pending attach with it.
    #[cfg(unix)]
    #[test]
    fn a_descendant_holding_stdout_cannot_hang_the_read() {
        /// [`READ_STDOUT_GRACE`] plus slack for a loaded machine, far below 30 s.
        const RETURNS_WITHIN: Duration = Duration::from_secs(3);

        let dir = scratch("stdout-held");
        let pid_file = dir.join("holder.pid");
        let mut held = stand_in("true", None, "true");
        held.list = ReadCmd::new(
            "sh",
            &[
                "-c",
                "sleep 30 & echo $! > \"$0\"",
                pid_file.to_str().expect("a UTF-8 temp path"),
            ],
        );

        let (result, took) = read_timed(held, &dir);
        let holder = recorded_pid(&pid_file);
        if let Some(pid) = holder {
            kill_leftover(pid);
        }

        assert!(
            holder.is_some(),
            "the stand-in must have started its holder"
        );
        assert_eq!(
            result,
            Some(ClipboardAttach::Failed),
            "exactly one Failed: an unfinished listing is never read as a partial one"
        );
        assert!(
            took < RETURNS_WITHIN,
            "the wait for stdout must be bounded: {took:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- pinned argv and the owner-only cache -------------------------------

    /// Every read argv, pinned the way the copy tools' are: a typo here would read
    /// PRIMARY instead of CLIPBOARD, the wrong type, or the wrong file. Each script
    /// body is pinned against its own literal below, never against its const.
    #[test]
    fn each_read_step_carries_its_pinned_argv() {
        /// [`MACOS_LIST_SCRIPT`], spelled independently as raw strings, so a slip
        /// in either spelling's escapes shows as a mismatch.
        const LIST_SCRIPT: &str = concat!(
            r"ObjC.import('AppKit');",
            r"var pb=$.NSPasteboard.generalPasteboard;var o=[];",
            r"(ObjC.deepUnwrap(pb.types)||[]).forEach(function(t){o.push('T\t'+t)});",
            r"var opts=$.NSDictionary.dictionaryWithObjectForKey(",
            r"$.NSNumber.numberWithBool(true),$.NSPasteboardURLReadingFileURLsOnlyKey);",
            r"var u=pb.readObjectsForClassesOptions($.NSArray.arrayWithObject($.NSURL),opts);",
            r"if(u){for(var i=0;i<u.count;i++){o.push('F\t'+",
            r"ObjC.unwrap(u.objectAtIndex(i).path).replace(/[\n\r]/g,'\u0001'))}}",
            r"o.join('\n')",
        );
        /// [`MACOS_RAW_IMAGE_SCRIPT`], spelled independently.
        const RAW_IMAGE_SCRIPT: &str = concat!(
            r"ObjC.import('AppKit');",
            r"function run(argv){",
            r"var d=$.NSPasteboard.generalPasteboard.dataForType(argv[0]);",
            r"if(d.isNil()||!d.writeToFileAtomically(argv[1],false)){",
            r"throw new Error('no '+argv[0])}",
            r"}",
        );
        let pinned: [(ReadArgv, &str, &[&str]); 9] = [
            (
                MACOS_LIST,
                "osascript",
                &["-l", "JavaScript", "-e", LIST_SCRIPT],
            ),
            (
                MACOS_PNG_IMAGE,
                "osascript",
                &[
                    "-e",
                    "on run argv",
                    "-e",
                    "set png_data to (the clipboard as «class PNGf»)",
                    "-e",
                    "set fp to open for access POSIX file (item 1 of argv) with write permission",
                    "-e",
                    "set eof fp to 0",
                    "-e",
                    "write png_data to fp",
                    "-e",
                    "close access fp",
                    "-e",
                    "end run",
                    "{dest}",
                ],
            ),
            (
                MACOS_RAW_IMAGE,
                "osascript",
                &[
                    "-l",
                    "JavaScript",
                    "-e",
                    RAW_IMAGE_SCRIPT,
                    "{type}",
                    "{dest}",
                ],
            ),
            (WL_PASTE_LIST, "wl-paste", &["--list-types"]),
            (
                WL_PASTE_URI_LIST,
                "wl-paste",
                &["--no-newline", "--type", "text/uri-list"],
            ),
            (WL_PASTE_IMAGE, "wl-paste", &["--type", "{type}"]),
            (
                XCLIP_LIST,
                "xclip",
                &["-selection", "clipboard", "-t", "TARGETS", "-o"],
            ),
            (
                XCLIP_URI_LIST,
                "xclip",
                &["-selection", "clipboard", "-t", "text/uri-list", "-o"],
            ),
            (
                XCLIP_IMAGE,
                "xclip",
                &["-selection", "clipboard", "-t", "{type}", "-o"],
            ),
        ];
        for (argv, program, args) in pinned {
            assert_eq!((argv.program, argv.args), (program, args), "{argv:?}");
        }
    }

    /// The tags [`MACOS_LIST_SCRIPT`] pushes are exactly the two
    /// [`parse_macos_list`] strips, in order. The script spells them inside
    /// JavaScript string literals, so a slip on either side would make the parser
    /// skip every line, and a full clipboard would read as empty.
    #[test]
    fn the_macos_list_script_pushes_exactly_the_parsers_tags() {
        /// Opens each line the script pushes: a JavaScript string literal.
        const PUSH: &str = "o.push('";
        /// Closes that literal; the line's value is appended after it.
        const THEN_VALUE: &str = "'+";
        let pushed: Vec<String> = MACOS_LIST_SCRIPT
            .split(PUSH)
            .skip(1)
            .map(|rest| {
                let (tag, _) = rest
                    .split_once(THEN_VALUE)
                    .expect("a tag literal, then its value");
                // JavaScript reads `\t` inside a string literal as a tab.
                tag.replace(r"\t", "\t")
            })
            .collect();
        assert_eq!(pushed, [MACOS_TYPE_TAG, MACOS_FILE_TAG]);
    }

    /// Each backend is built from exactly its named argv consts.
    #[test]
    fn each_backend_runs_its_named_argv() {
        let cmd = ReadCmd::from;
        let mac = ReadBackendKind::Osascript.backend();
        assert_eq!(mac.kind, ReadBackendKind::Osascript);
        assert_eq!(mac.list, cmd(MACOS_LIST));
        assert_eq!(mac.uri_list, None);
        assert_eq!(mac.image, cmd(MACOS_RAW_IMAGE));
        assert_eq!(mac.png_image, Some(cmd(MACOS_PNG_IMAGE)));
        assert_eq!((mac.format, mac.sink), (ListFormat::MacOs, ImageSink::File));

        let wl = ReadBackendKind::WlPaste.backend();
        assert_eq!(wl.kind, ReadBackendKind::WlPaste);
        assert_eq!(wl.list, cmd(WL_PASTE_LIST));
        assert_eq!(wl.uri_list, Some(cmd(WL_PASTE_URI_LIST)));
        assert_eq!(wl.image, cmd(WL_PASTE_IMAGE));
        assert_eq!(wl.png_image, None);
        assert_eq!(
            (wl.format, wl.sink),
            (ListFormat::TypeLines, ImageSink::Stdout)
        );

        let x = ReadBackendKind::Xclip.backend();
        assert_eq!(x.kind, ReadBackendKind::Xclip);
        assert_eq!(x.list, cmd(XCLIP_LIST));
        assert_eq!(x.uri_list, Some(cmd(XCLIP_URI_LIST)));
        assert_eq!(x.image, cmd(XCLIP_IMAGE));
        assert_eq!(x.png_image, None);
        assert_eq!(
            (x.format, x.sink),
            (ListFormat::TypeLines, ImageSink::Stdout)
        );

        for backend in [mac, wl, x] {
            assert_eq!(backend.timeout, CLIPBOARD_READ_TIMEOUT);
        }
    }

    /// A slot is filled per read: the type and the temp file land where each argv
    /// puts them, and the PNG step names no type (`«class PNGf»` converts whatever
    /// image the pasteboard holds).
    #[test]
    fn a_read_argv_slot_takes_the_type_and_the_file() {
        let dest = "/cache/.paste-1-2.gif.tmp";
        let filled =
            |argv: ReadArgv, target: &str| ReadCmd::from(argv).substituted(target, dest).args;
        assert_eq!(
            filled(XCLIP_IMAGE, "image/png"),
            ["-selection", "clipboard", "-t", "image/png", "-o"]
        );
        assert_eq!(
            filled(WL_PASTE_IMAGE, "image/webp"),
            ["--type", "image/webp"]
        );
        assert_eq!(
            filled(MACOS_RAW_IMAGE, "com.compuserve.gif")[4..],
            ["com.compuserve.gif", dest]
        );
        let png = filled(MACOS_PNG_IMAGE, "public.png");
        assert_eq!(png.last().map(String::as_str), Some(dest));
        assert!(!png.iter().any(|a| a.contains("public.png")));
    }

    /// The image step follows the chosen type's fetch: a raw type runs `image` with
    /// the type in its slot and keeps its extension, a converted one runs
    /// `png_image`, and a backend with no PNG conversion refuses one, writing
    /// nothing.
    #[test]
    fn save_image_runs_the_step_the_types_fetch_names() {
        let dir = scratch("fetch");
        let mut backend = stand_in("true", None, "true");
        backend.image = ReadCmd::new("sh", &["-c", "printf %s \"$0\"", TYPE_PLACEHOLDER]);
        backend.png_image = Some(sh("printf CONVERTED"));

        let ClipboardAttach::Image(gif) =
            save_image(&backend, ImageType::raw("com.compuserve.gif", "gif"), &dir)
        else {
            panic!("expected the raw gif");
        };
        assert_eq!(gif.extension().unwrap(), "gif");
        assert_eq!(std::fs::read(&gif).unwrap(), b"com.compuserve.gif");

        let ClipboardAttach::Image(png) =
            save_image(&backend, ImageType::converted_to_png("public.tiff"), &dir)
        else {
            panic!("expected the converted png");
        };
        assert_eq!(png.extension().unwrap(), "png");
        assert_eq!(std::fs::read(&png).unwrap(), b"CONVERTED");

        backend.png_image = None;
        assert_eq!(
            save_image(&backend, ImageType::converted_to_png("public.png"), &dir),
            ClipboardAttach::Failed
        );
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            2,
            "the refusal wrote nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The paste cache dir is created owner-only (`0700`): a screenshot can carry
    /// secrets. Created under a unique temp dir, never the real cache.
    #[cfg(unix)]
    #[test]
    fn the_paste_cache_dir_is_created_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        /// The permission bits of a mode.
        const PERMISSION_BITS: u32 = 0o777;
        let root = scratch("mode");
        let dir = root.join("cache").join("paste");
        let saved = save_image(&stand_in("true", None, "printf PNGBYTES"), LINUX_PNG, &dir);
        assert!(matches!(saved, ClipboardAttach::Image(_)), "{saved:?}");
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & PERMISSION_BITS;
        assert_eq!(mode, 0o700, "the cache dir's mode is {mode:o}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
