//! claude's own list of a folder's slash commands (skills and built-ins) and
//! agents, asked for over the `initialize` control handshake: the compose pick
//! list's PRIMARY source.
//!
//! One `claude -p` child per folder reads a single control request on stdin and
//! answers with one `control_response` line carrying `commands` and `agents`. No
//! user message is ever written, so claude makes no model call. The wire shape,
//! the flags and what the child leaves behind were probed against
//! `claude 2.1.284` on 2026-09-30; those facts live in
//! `docs/agents/CLAUDE_CLI.md` ("The `initialize` control handshake"), not here.
//!
//! It SHELLS OUT, so it lives outside `src/store/` (AGENTS.md PURE, GIT-FREE
//! STORE CORE) and runs only on its own thread: [`spawn_fetch`] delivers exactly
//! one [`AppEvent::CatalogFetched`], the OFF-UI-THREAD rule's ordinary
//! own-thread case, like `ModelAliases` and `SettingsModel`. The child's stdin
//! and stdout are pipes and its stderr is discarded — it never sees the terminal
//! (AGENTS.md TERMINAL SAFETY) — and it leads a process group of its own, so a
//! timed-out fetch ends everything the child started. The reply is parsed FAIL-SOFT over
//! `serde_json::Value`, and only its `commands` and `agents` keys are read: the
//! same body carries an `account` key with the user's account identity, which is
//! never touched.
//!
//! Which of the folder's settings the child loads is decided at FETCH time, on
//! that same worker thread: [`fetch_in`] asks `crate::claude_trust` for claude's
//! own workspace-trust verdict and picks [`build_catalog_argv`]'s form from it.
//! The untrusted form also turns off claude's own git prefetch in the folder,
//! through the child's environment alone ([`build_catalog_env`]).
//! Reading the verdict is blocking FS work, so no key handler, no render path
//! and not `compose::take_catalog_fetch` ever reads it (AGENTS.md OFF-UI-THREAD).
//!
//! The answer is a [`Listing`], the shape `store::skills::read_listing` gives a
//! reply's `@` agents too, so the pick list turns either into rows the same way.
//!
//! The child machinery ([`exchange_reaped`]: spawn, one request, timeout, group
//! kill, reap) and the untrusted form ([`trust_flags`], [`build_catalog_env`])
//! are shared with `crate::claude_move`, whose one `set_cwd` request rides the
//! same exchange with a reply parser of its own.

use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::claude_trust::{self, FolderTrust};
use crate::store::skills::{normalize_description, Listing, ListingEntry};
use crate::watch::AppEvent;

/// The `request_id` snapback stamps on its one control request. claude echoes it
/// in the reply, so a `control_response` carrying any other id answers some
/// other request and is not the catalog.
const CATALOG_REQUEST_ID: &str = "snapback-catalog";

/// The program the real fetch runs, and the first word of
/// [`build_catalog_argv`] (and of `claude_move::build_set_cwd_argv`).
/// [`spawn_fetch`] and `claude_move::spawn_move` are the sites that run it; the
/// suite runs a stand-in in its place.
pub(crate) const CLAUDE_PROGRAM: &str = "claude";

/// The `--settings` JSON the fetch runs under, in BOTH argv forms: the fetch is
/// not a session the user started, so no hook runs just because a compose box
/// opened. Without it the user's own `SessionStart` hooks fired on every fetch;
/// with it the command and agent lists were identical (claude 2.1.284, probed
/// 2026-09-30). Whether the folder's PROJECT settings load at all is the argv
/// form's business ([`USER_SETTING_SOURCES`]), not this JSON's.
const CATALOG_SETTINGS: &str = r#"{"disableAllHooks":true}"#;

/// The `--setting-sources` value of the UNTRUSTED form. `user` loads neither the
/// project's settings files (their hooks, `env` block and helper commands such
/// as `apiKeyHelper`) nor its `.mcp.json`, and keeps the user's own, bundled and
/// built-in skills, commands and agents: the lists matched a plain folder's name
/// for name, and no project helper ran (claude 2.1.284, probed 2026-09-30).
const USER_SETTING_SOURCES: &str = "user";

/// The environment variable the UNTRUSTED form sets on its child, to
/// [`DISABLE_GIT_INSTRUCTIONS_ON`] (claude 2.1.284, probed 2026-10-01). A `-p`
/// run starts claude's own git-status prefetch with no trust check: `git
/// status`, `git log` and `git config user.name` in the folder, and a filter
/// driver from the folder's `.git/config` runs through that `git status`. No
/// setting source stops it, so `--setting-sources user` leaves it on. claude
/// reads this variable BEFORE the `includeGitInstructions` setting, and `1`
/// turns the prefetch off. The command and agent lists stayed byte-identical
/// with it. The probe and what it leaves running are CLAUDE_CLI.md's
/// ("Workspace trust").
const DISABLE_GIT_INSTRUCTIONS_ENV: &str = "CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS";

/// [`DISABLE_GIT_INSTRUCTIONS_ENV`]'s value that turns claude's git prefetch
/// off: claude reads it as a tri-state boolean, and `1` is its "true".
const DISABLE_GIT_INSTRUCTIONS_ON: &str = "1";

/// How long one fetch may take, the reply, the close of stdout AND the child's
/// exit together. Past it the child's whole process group and the child itself
/// are killed, and the child reaped. Measured (claude 2.1.284, 2026-09-30): the
/// reply lands in 0.21–0.22 s with [`build_catalog_argv`]'s flags and
/// 0.39–0.69 s without them, and stdout closes by 0.24 s, so 10 s only ever cuts
/// off a child that is stuck. Only the fetch's own worker thread waits on it,
/// for at most this plus
/// [`CATALOG_READER_GRACE`].
const CATALOG_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a timed-out fetch waits, after [`kill_group`], for its stdout
/// reader to see EOF before joining it. Once the whole group and the child are
/// SIGKILLed the pipe's last writer goes with the kernel's teardown, well inside
/// this. It expires only for a DESCENDANT that LEFT the group (its own
/// `setpgid`/`setsid`), which neither kill reaches: that reader is then left to
/// end with its writer, the one thread a fetch can outlive, and the worker still
/// delivers its event.
const CATALOG_READER_GRACE: Duration = Duration::from_millis(500);

/// `process_group`'s argument for "a NEW group whose id is the child's own pid"
/// (`setpgid(0, 0)` in the child), which is what lets [`kill_group`] name the
/// group by the pid `Child::id` reports.
#[cfg(unix)]
const NEW_PROCESS_GROUP: i32 = 0;

/// How often the reaper asks whether the child has exited. claude exits 25–50 ms
/// after its stdin reaches EOF (measured, claude 2.1.284, 2026-09-30), so a
/// clean exit is seen within a poll or three.
const CATALOG_EXIT_POLL: Duration = Duration::from_millis(20);

/// Cheap prefilter: a stdout line without it cannot be the reply, so it is never
/// JSON-parsed (hook and system lines are skipped unparsed). Plain
/// `str::contains`: `memchr` stays in `search.rs`. Also the reply's `type`, and
/// `claude_move`'s parser reads it the same way.
pub(crate) const CONTROL_RESPONSE_MARKER: &str = "control_response";

/// claude's hidden built-ins that can reach the `initialize` reply: the commands
/// claude 2.1.284 declares with a literal `isHidden: true` AND that pass its
/// print-mode command gate (read from its bundle on 2026-10-02). claude's own `/`
/// menu skips them, but the reply drops that flag, so they arrive as plain
/// `builtin: true` entries and only their names set them apart. A hidden command
/// the gate refuses never reaches the reply, so it is not listed. The gate, the
/// drift between releases and the recipe that re-derives the set are
/// CLAUDE_CLI.md's ("The `initialize` control handshake": its hidden built-ins
/// and re-verify steps).
const CLAUDE_HIDDEN_BUILTINS: [&str; 7] = [
    "__remote-workflow",
    "agents",
    "design-consent",
    "design-revoke",
    "extra-usage",
    "heapdump",
    "workflow-launch-exec",
];

/// The fetch's argv for a folder `trust` describes, exactly:
/// `claude -p --input-format stream-json --output-format stream-json --verbose
/// --no-session-persistence --strict-mcp-config --settings {"disableAllHooks":true}`,
/// followed, unless the folder is [`FolderTrust::Trusted`], by
/// `--setting-sources user`.
///
/// - `--input-format` / `--output-format stream-json` carry the control
///   protocol; `--verbose` is REQUIRED with them (claude exits 1 without it).
/// - `--no-session-persistence` is belt and braces: the probe saw no transcript
///   written without it either, so no stub row can reach the board.
/// - `--strict-mcp-config` starts no MCP server; their prompts are not in the
///   reply anyway, and the fetch is faster without them.
/// - `--settings` [`CATALOG_SETTINGS`] runs no hook.
/// - `--setting-sources` [`USER_SETTING_SOURCES`]: `-p` never shows claude's
///   trust dialog, so in a folder claude does not trust, the repository's own
///   settings would otherwise run its helper commands and apply its `env` block
///   just because a compose box opened. A trusted folder keeps them, and with
///   them its own skills, commands and agents. Which folders claude trusts is
///   CLAUDE_CLI.md's. The form's environment half is [`build_catalog_env`]'s.
#[must_use]
pub fn build_catalog_argv(trust: FolderTrust) -> Vec<String> {
    [
        CLAUDE_PROGRAM,
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--no-session-persistence",
        "--strict-mcp-config",
        "--settings",
        CATALOG_SETTINGS,
    ]
    .iter()
    .chain(trust_flags(trust))
    .map(|word| (*word).to_owned())
    .collect()
}

/// The argv words the UNTRUSTED form appends for a folder `trust` describes:
/// none for [`FolderTrust::Trusted`], `--setting-sources`
/// [`USER_SETTING_SOURCES`] otherwise. The ONE copy of that choice: both
/// [`build_catalog_argv`] and `claude_move::build_set_cwd_argv` end with it, and
/// [`build_catalog_env`] is its environment half for both.
#[must_use]
pub fn trust_flags(trust: FolderTrust) -> &'static [&'static str] {
    match trust {
        FolderTrust::Trusted => &[],
        FolderTrust::Untrusted => &["--setting-sources", USER_SETTING_SOURCES],
    }
}

/// The environment half of [`build_catalog_argv`]'s form for a folder `trust`
/// describes: the variables set on the fetch's CHILD alone, on top of what it
/// inherits. Never snapback's own environment. The move's child
/// (`claude_move`) takes the same environment for the same verdict.
///
/// - [`FolderTrust::Trusted`]: none, so the child inherits snapback's
///   environment untouched.
/// - [`FolderTrust::Untrusted`]: [`DISABLE_GIT_INSTRUCTIONS_ENV`] set to
///   [`DISABLE_GIT_INSTRUCTIONS_ON`], so claude's own git prefetch never runs
///   the repository's git configuration.
#[must_use]
pub fn build_catalog_env(trust: FolderTrust) -> &'static [(&'static str, &'static str)] {
    match trust {
        FolderTrust::Trusted => &[],
        FolderTrust::Untrusted => &[(DISABLE_GIT_INSTRUCTIONS_ENV, DISABLE_GIT_INSTRUCTIONS_ON)],
    }
}

/// The one stdin line of a fetch: an `initialize` control request stamped with
/// [`CATALOG_REQUEST_ID`], newline-terminated.
#[must_use]
pub fn initialize_request_line() -> String {
    let request = json!({
        "type": "control_request",
        "request_id": CATALOG_REQUEST_ID,
        "request": { "subtype": "initialize" },
    });
    format!("{request}\n")
}

/// The catalog carried by ONE stdout line, or `None` when the line is not the
/// successful reply to [`initialize_request_line`].
///
/// The line must be a `control_response` whose `response.subtype` is `success`
/// and whose `response.request_id` is [`CATALOG_REQUEST_ID`]; its body
/// (`response.response`) must be an object holding a `commands` or an `agents`
/// array. Entries are objects with a non-empty string `name` (trimmed) and an
/// optional string `description` ([`normalize_description`]). A command's
/// `builtin` is read only to drop claude's hidden built-ins
/// ([`is_hidden_builtin`]), ahead of the dedup so a same-named skill listed
/// after one survives; every other key is ignored, a malformed entry is
/// skipped, and each list is sorted by name with later duplicates dropped.
#[must_use]
pub fn parse_initialize_response(line: &str) -> Option<Listing> {
    if !line.contains(CONTROL_RESPONSE_MARKER) {
        return None;
    }
    let value: Value = serde_json::from_str(line).ok()?;
    if value.get("type").and_then(Value::as_str) != Some(CONTROL_RESPONSE_MARKER) {
        return None;
    }
    let response = value.get("response")?;
    if response.get("subtype").and_then(Value::as_str) != Some("success")
        || response.get("request_id").and_then(Value::as_str) != Some(CATALOG_REQUEST_ID)
    {
        return None;
    }
    let body = response.get("response")?.as_object()?;
    let commands = body.get("commands").and_then(Value::as_array);
    let agents = body.get("agents").and_then(Value::as_array);
    if commands.is_none() && agents.is_none() {
        return None;
    }
    Some(Listing {
        commands: commands.map_or_else(Vec::new, |items| {
            catalog_entries(items.iter().filter(|item| !is_hidden_builtin(item)))
        }),
        agents: agents.map_or_else(Vec::new, catalog_entries),
    })
}

/// Whether a reply `commands` entry is one of [`CLAUDE_HIDDEN_BUILTINS`]: its
/// `builtin` is the JSON boolean `true` AND its trimmed `name` is listed. A
/// user or project skill of the same name carries no `builtin`, so it is still
/// offered.
fn is_hidden_builtin(item: &Value) -> bool {
    item.get("builtin") == Some(&Value::Bool(true))
        && item
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| CLAUDE_HIDDEN_BUILTINS.contains(&name.trim()))
}

/// The well-formed entries of one reply array, sorted by name, first of each
/// name kept (the sort is stable, so input order breaks ties).
fn catalog_entries<'a>(items: impl IntoIterator<Item = &'a Value>) -> Vec<ListingEntry> {
    let mut entries: Vec<ListingEntry> = items
        .into_iter()
        .filter_map(|item| {
            let item = item.as_object()?;
            let name = item.get("name")?.as_str()?.trim();
            if name.is_empty() {
                return None;
            }
            let description = item
                .get("description")
                .and_then(Value::as_str)
                .and_then(normalize_description);
            Some(ListingEntry {
                name: name.to_owned(),
                description,
            })
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries.dedup_by(|later, earlier| later.name == earlier.name);
    entries
}

/// Ask `program` for `cwd`'s catalog: read the folder's trust with `trust_of`
/// FIRST, then [`fetch_with`] over `program` followed by the rest of
/// [`build_catalog_argv`] and the child environment [`build_catalog_env`] for
/// that ONE verdict, with the one [`initialize_request_line`] and `timeout`.
///
/// Blocking, the trust read included: only [`spawn_fetch_in`]'s worker thread
/// calls it, so the verdict is read at fetch time, for this fetch, off the UI
/// thread. `trust_of` and `program` are parameters so the suite can state a
/// record and run a stand-in child; [`spawn_fetch`] is the one site naming the
/// real ones.
fn fetch_in<T>(cwd: &Path, trust_of: T, program: &[String], timeout: Duration) -> Option<Listing>
where
    T: FnOnce(&Path) -> FolderTrust,
{
    let trust = trust_of(cwd);
    let argv: Vec<String> = program
        .iter()
        .cloned()
        .chain(build_catalog_argv(trust).into_iter().skip(1))
        .collect();
    fetch_with(
        &argv,
        build_catalog_env(trust),
        cwd,
        &initialize_request_line(),
        timeout,
    )
}

/// Run `argv` in `cwd` with `env` added to the child's environment, write
/// `request` to its stdin and close it, and return the first stdout line
/// [`parse_initialize_response`] accepts: [`fetch_reaped`]'s answer alone.
///
/// `argv`, `env`, `request` and `timeout` are parameters so the suite can run a
/// stand-in child instead of `claude`; [`fetch_in`] is the only caller, and
/// [`spawn_fetch`] the one site naming the real program.
fn fetch_with(
    argv: &[String],
    env: &[(&str, &str)],
    cwd: &Path,
    request: &str,
    timeout: Duration,
) -> Option<Listing> {
    fetch_reaped(argv, env, cwd, request, timeout).listing
}

/// What one [`fetch_reaped`] left behind.
struct Fetched {
    /// The catalog, or `None` when the child gave no answer in time.
    listing: Option<Listing>,
    /// Whether the stdout reader thread was JOINED before the call returned.
    /// `false` when it was left past [`CATALOG_READER_GRACE`], or when no child
    /// (or no stdout) ever started one.
    #[allow(dead_code)] // Instrumentation: read by two timed-out fetch tests.
    reader_joined: bool,
}

/// [`fetch_with`], reporting whether its stdout reader was joined: the catalog's
/// [`exchange_reaped`], with [`parse_initialize_response`] as its reply parser.
fn fetch_reaped(
    argv: &[String],
    env: &[(&str, &str)],
    cwd: &Path,
    request: &str,
    timeout: Duration,
) -> Fetched {
    let exchanged = exchange_reaped(argv, env, cwd, request, timeout, parse_initialize_response);
    Fetched {
        listing: exchanged.answer.ok(),
        reader_joined: exchanged.reader_joined,
    }
}

/// Why one [`exchange_reaped`] has no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoAnswer {
    /// No child ran: the argv was empty, or the spawn failed (a missing program
    /// or `cwd`).
    Spawn,
    /// The child's stdout closed (every writer gone) with no line the parser
    /// accepted.
    Closed,
    /// The deadline passed before an answer; the child's group was killed.
    TimedOut,
}

/// What one [`exchange_reaped`] left behind.
pub struct Exchanged<T> {
    /// The first stdout line the parser accepted, or why there was none.
    pub answer: Result<T, NoAnswer>,
    /// Whether the stdout reader thread was JOINED before the call returned.
    /// `false` when it was left past [`CATALOG_READER_GRACE`], or when no child
    /// (or no stdout) ever started one.
    reader_joined: bool,
}

/// Run `argv` in `cwd` with `env` added to the child's environment, write
/// `request` to its stdin and close it, and answer the first stdout line `parse`
/// accepts — the ONE child exchange behind the catalog fetch and the move
/// (`crate::claude_move`); only the request and the reply parser differ.
///
/// `env` reaches the CHILD's environment alone, over whatever it inherits; this
/// process's own environment is never written.
///
/// The answer is an `Err` when the child cannot be spawned (a missing program or
/// `cwd`), its stdout ends with no answer, or `timeout` passes first. Never
/// panics and never touches the terminal: stdin and stdout are pipes, stderr is
/// discarded, and the child leads a process group of its own (on unix), outside
/// the board's foreground group, so a timeout can end everything it started.
///
/// One `timeout` deadline bounds the answer, the close of stdout (every writer
/// gone, grandchildren included) AND the child's own exit; then:
/// 1. stdout not closed by then: [`kill_group`];
/// 2. [`reap`] the child, which kills the group and the child itself if it is
///    still running;
/// 3. join the reader, at once when stdout had closed, else once it closes
///    within [`CATALOG_READER_GRACE`].
///
/// Every kill comes BEFORE the reap, the order [`kill_group`]'s contract needs.
pub fn exchange_reaped<T>(
    argv: &[String],
    env: &[(&str, &str)],
    cwd: &Path,
    request: &str,
    timeout: Duration,
    parse: impl Fn(&str) -> Option<T>,
) -> Exchanged<T> {
    let unanswered = Exchanged {
        answer: Err(NoAnswer::Spawn),
        reader_joined: false,
    };
    let deadline = Instant::now() + timeout;
    let Some((program, args)) = argv.split_first() else {
        return unanswered;
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .envs(env.iter().copied())
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Applied in the child before `exec`, and `spawn` returns only after the
    // `exec`, so the group exists before anything below can signal it. The
    // child never reads the TTY (stdin is a pipe), so leaving the board's
    // foreground group costs nothing.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(NEW_PROCESS_GROUP);
    }
    let Ok(mut child) = command.spawn() else {
        return unanswered;
    };
    // Taken BY VALUE and dropped at the end of this statement: claude answers the
    // request either way, but only EOF lets it exit. A failed write (the child
    // already gone) just leaves nothing to answer.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(request.as_bytes());
    }
    // The receiver stays alive until the reader is joined or abandoned, so the
    // reader keeps draining stdout and a child still writing never blocks on a
    // full pipe.
    let reader = child.stdout.take().map(forward_lines);
    let answer = reader
        .as_ref()
        .and_then(|(lines, _)| await_answer(lines, deadline, &parse));
    let closed = reader
        .as_ref()
        .is_none_or(|(lines, _)| await_closed(lines, deadline));
    if !closed {
        kill_group(&mut child);
    }
    reap(&mut child, deadline);
    let reader_joined = reader.is_some_and(|(lines, handle)| {
        (closed || await_closed(&lines, Instant::now() + CATALOG_READER_GRACE))
            && handle.join().is_ok()
    });
    // With no answer, the close decides why: `await_answer` gave up either at
    // EOF (and `await_closed` then saw the hang-up at once) or at the deadline
    // (and `await_closed` had no time left).
    let answer = answer.ok_or(if closed {
        NoAnswer::Closed
    } else {
        NoAnswer::TimedOut
    });
    Exchanged {
        answer,
        reader_joined,
    }
}

/// Forward `stdout`'s lines over a channel from a reader thread of its own, so
/// the caller can wait on them with a deadline, and hand back that thread's
/// handle so the caller can join it. The thread ends at EOF (every writer of
/// the pipe exited or was killed), on a read error, or once the receiver is
/// gone; a non-UTF-8 line is skipped, as `store::skills::read_listing` skips
/// one.
fn forward_lines(stdout: ChildStdout) -> (Receiver<String>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == ErrorKind::InvalidData => {}
                Err(_) => break,
            }
        }
    });
    (rx, handle)
}

/// Drain `lines` unread until the reader hangs up, `true`, or `deadline`
/// passes, `false`. The reader hangs up only as its thread ends: at EOF, when
/// every writer of the pipe has closed it, or on a read error.
fn await_closed(lines: &Receiver<String>, deadline: Instant) -> bool {
    loop {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            return false;
        };
        match lines.recv_timeout(left) {
            Ok(_) => {}
            Err(RecvTimeoutError::Disconnected) => return true,
            Err(RecvTimeoutError::Timeout) => return false,
        }
    }
}

/// The first line among `lines` that `parse` accepts, or `None` once the lines
/// end (the child closed stdout) or `deadline` passes.
fn await_answer<T>(
    lines: &Receiver<String>,
    deadline: Instant,
    parse: &impl Fn(&str) -> Option<T>,
) -> Option<T> {
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        let line = lines.recv_timeout(left).ok()?;
        if let Some(answer) = parse(&line) {
            return Some(answer);
        }
    }
}

/// Wait for `child` to exit on its own until `deadline`, polling every
/// [`CATALOG_EXIT_POLL`]; past it, [`kill_group`] and SIGKILL the child itself,
/// then reap it. Reaping is the LAST thing done to the child: only a `try_wait`
/// that saw it exit, or the final `wait`, reaps it, and both come after every
/// kill.
fn reap(child: &mut Child, deadline: Instant) {
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => thread::sleep(CATALOG_EXIT_POLL),
            _ => break,
        }
    }
    kill_group(child);
    // A leader that moved itself into another existing group (`setpgid`) is
    // outside the group `kill_group` names, and `wait` would block on it for as
    // long as it runs. Both kills precede the reap, so the pid is still this
    // child's. Off unix `kill_group` already is this call, and a second is
    // harmless.
    let _ = child.kill();
    let _ = child.wait();
}

/// SIGKILL `child`'s whole process group: the child [`exchange_reaped`] spawned as
/// a group leader and everything it started that stayed in the group. SIGKILL,
/// never SIGTERM first: a fetch is no session the user started, and nothing in
/// the group has work to save. A move's child (`crate::claude_move`) is killed
/// only past `claude_move::MOVE_TIMEOUT`, many times its measured run, so it is
/// stuck rather than mid-move. It reaches only members still IN the group, so
/// [`reap`] also kills the child directly.
///
/// **Contract: call it ONLY before the child has been reaped.** Until then the
/// child's pid, which is the group's id, cannot be handed to another process.
///
/// The id is narrowed by `send::signallable_pid`, the rule `Ctrl-K` signals by:
/// strictly positive (`killpg(2)` reads `0` as the caller's own group) and never
/// the board's own pid. A pid that rule refuses, which a spawned child's never
/// is, falls back to killing the child alone.
#[cfg(unix)]
fn kill_group(child: &mut Child) {
    let Some(pgid) = crate::send::signallable_pid(child.id(), std::process::id()) else {
        let _ = child.kill();
        return;
    };
    // SAFETY: `killpg` takes two scalars and touches no memory this side owns, so
    // there is no pointer, lifetime or aliasing obligation to uphold. `pgid` is
    // the pid of a child this process spawned with `process_group(0)`
    // (`NEW_PROCESS_GROUP`), which made it the id of the child's own group, and
    // the child is not yet reaped (this fn's contract), so the id cannot have been
    // recycled onto another group. `signallable_pid` made it strictly positive and
    // not the board's own pid, so the call can name neither the caller's group
    // (`0`) nor a broadcast. The result and `errno` are ignored: `ESRCH` means the
    // group is already gone, and no other failure leaves anything for a timed-out
    // fetch to do.
    let _ = unsafe { libc::killpg(pgid, libc::SIGKILL) };
}

/// The off-unix stand-in for the unix [`kill_group`]: there is no process
/// group to name, so it kills the child alone, as `Child::kill` does.
#[cfg(not(unix))]
fn kill_group(child: &mut Child) {
    let _ = child.kill();
}

/// Fetch `cwd`'s catalog on a thread of its own and deliver exactly one
/// [`AppEvent::CatalogFetched`] carrying `cwd` back. The ONE site that names the
/// real trust reader (`claude_trust::folder_trust`), program and timeout, as
/// `watch::EventLoop::spawn_settings_model_probe` is for its read.
pub fn spawn_fetch(cwd: PathBuf, tx: Sender<AppEvent>) {
    spawn_fetch_in(
        cwd,
        tx,
        claude_trust::folder_trust,
        vec![CLAUDE_PROGRAM.to_owned()],
        CATALOG_FETCH_TIMEOUT,
    );
}

/// [`spawn_fetch`] over a stated `trust_of`, `program` and `timeout`: one
/// worker thread runs [`fetch_in`], the trust read included. The read is
/// blocking FS work, so it must stay INSIDE the worker; hoisting it to the
/// spawning (UI) thread is the regression the suite pins here.
fn spawn_fetch_in<T>(
    cwd: PathBuf,
    tx: Sender<AppEvent>,
    trust_of: T,
    program: Vec<String>,
    timeout: Duration,
) where
    T: FnOnce(&Path) -> FolderTrust + Send + 'static,
{
    spawn_fetch_with(cwd, tx, move |cwd| {
        fetch_in(cwd, trust_of, &program, timeout)
    });
}

/// [`spawn_fetch`] over a stated `fetch`: a one-shot thread that runs it and
/// sends one [`AppEvent::CatalogFetched`]. A failed send is ignored — the board
/// that asked has gone away, and the next compose on that folder asks again.
/// `fetch` is a SEAM production swaps exactly never, so the suite can state an
/// answer instead of spawning `claude`.
fn spawn_fetch_with<F>(cwd: PathBuf, tx: Sender<AppEvent>, fetch: F)
where
    F: FnOnce(&Path) -> Option<Listing> + Send + 'static,
{
    thread::spawn(move || {
        let listing = fetch(&cwd);
        let _ = tx.send(AppEvent::CatalogFetched { cwd, listing });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::mpsc::RecvTimeoutError;

    fn entry(name: &str, description: Option<&str>) -> ListingEntry {
        ListingEntry {
            name: name.to_owned(),
            description: description.map(str::to_owned),
        }
    }

    fn names(entries: &[ListingEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    /// A reply line with `subtype`, `request_id` and a `body` spliced in raw.
    fn reply(subtype: &str, request_id: &str, body: &str) -> String {
        format!(
            r#"{{"type":"control_response","response":{{"subtype":"{subtype}","request_id":"{request_id}","response":{body}}}}}"#
        )
    }

    fn success(body: &str) -> String {
        reply("success", CATALOG_REQUEST_ID, body)
    }

    /// The trimmed real reply (claude 2.1.284) in `initialize_response.json`;
    /// see [`fixture_from`].
    fn fixture_line(request_id: Option<&str>) -> String {
        fixture_from("initialize_response.json", request_id)
    }

    /// The trimmed real reply (claude 2.1.284) in
    /// `tests/fixtures/claude_catalog/<file>`, minified to the one line claude
    /// writes, its `request_id` restamped with `request_id`.
    fn fixture_from(file: &str, request_id: Option<&str>) -> String {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/claude_catalog")
            .join(file);
        let text = std::fs::read_to_string(path).expect("the fixture is checked in");
        let mut value: Value = serde_json::from_str(&text).expect("the fixture is JSON");
        if let Some(id) = request_id {
            value["response"]["request_id"] = Value::from(id);
        }
        value.to_string()
    }

    #[test]
    fn the_catalog_argv_is_exact() {
        assert_eq!(
            build_catalog_argv(FolderTrust::Trusted),
            [
                "claude",
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--no-session-persistence",
                "--strict-mcp-config",
                "--settings",
                r#"{"disableAllHooks":true}"#,
            ]
        );
    }

    /// A folder claude does not trust gets the trusted argv plus
    /// `--setting-sources user`: hooks stay off through `--settings`, and the
    /// repository's own settings never load.
    #[test]
    fn the_untrusted_catalog_argv_reads_user_settings_only() {
        assert_eq!(
            build_catalog_argv(FolderTrust::Untrusted),
            [
                "claude",
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--no-session-persistence",
                "--strict-mcp-config",
                "--settings",
                r#"{"disableAllHooks":true}"#,
                "--setting-sources",
                "user",
            ]
        );
    }

    /// The trusted form adds nothing to the child's environment; the untrusted
    /// form turns claude's git prefetch off. Spelled as literals, so a typo in
    /// either const fails here.
    #[test]
    fn the_catalog_env_turns_off_claudes_git_prefetch_only_when_untrusted() {
        assert_eq!(build_catalog_env(FolderTrust::Trusted), []);
        assert_eq!(
            build_catalog_env(FolderTrust::Untrusted),
            [("CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS", "1")]
        );
    }

    #[test]
    fn the_request_line_is_one_initialize_control_request() {
        let line = initialize_request_line();
        assert!(line.ends_with('\n'), "claude reads one line: {line:?}");
        assert_eq!(
            line.matches('\n').count(),
            1,
            "exactly one newline: {line:?}"
        );
        let value: Value = serde_json::from_str(line.trim_end()).expect("the line is JSON");
        assert_eq!(
            value,
            json!({
                "type": "control_request",
                "request_id": "snapback-catalog",
                "request": { "subtype": "initialize" },
            })
        );
    }

    #[test]
    fn the_real_reply_parses_to_its_commands_and_agents() {
        let got = parse_initialize_response(&fixture_line(Some(CATALOG_REQUEST_ID)))
            .expect("the restamped reply is the answer");
        assert_eq!(
            names(&got.commands),
            [
                "code-review",
                "compact",
                "cr-review",
                "sbnomodel",
                "sbx-skill",
                "sbz"
            ]
        );
        assert_eq!(names(&got.agents), ["Explore", "lead", "sby-agent"]);
        let described = |entries: &[ListingEntry], name: &str| {
            entries
                .iter()
                .find(|e| e.name == name)
                .and_then(|e| e.description.clone())
        };
        assert_eq!(
            described(&got.commands, "compact").as_deref(),
            Some("Free up context by summarizing the conversation so far")
        );
        assert_eq!(
            described(&got.commands, "sbz").as_deref(),
            Some("Probe command Z for snapback (project)")
        );
        assert_eq!(
            described(&got.agents, "sby-agent").as_deref(),
            Some("Probe agent Y for snapback")
        );
        let review = described(&got.commands, "cr-review").expect("described");
        assert!(
            review.starts_with("Independently review") && review.ends_with("verdict. (user)"),
            "the source tag stays, verbatim: {review}"
        );
        assert!(got
            .commands
            .iter()
            .chain(&got.agents)
            .all(|e| e.description.is_some()));
    }

    #[test]
    fn a_reply_to_another_request_is_not_the_answer() {
        assert_eq!(
            parse_initialize_response(&fixture_line(None)),
            None,
            "the probe's own request id is not snapback's"
        );
        let body = r#"{"commands":[{"name":"x"}]}"#;
        assert_eq!(
            parse_initialize_response(&reply("success", "other", body)),
            None
        );
        assert!(parse_initialize_response(&success(body)).is_some());
    }

    #[test]
    fn an_error_reply_is_not_the_answer() {
        let real = r#"{"type":"control_response","response":{"subtype":"error","request_id":"snapback-catalog","error":"Unsupported control request subtype: nope"}}"#;
        assert_eq!(parse_initialize_response(real), None);
        // Carries a catalog body, so only the subtype check can reject it.
        let with_body = reply(
            "error",
            CATALOG_REQUEST_ID,
            r#"{"commands":[{"name":"x"}]}"#,
        );
        assert_eq!(parse_initialize_response(&with_body), None);
    }

    #[test]
    fn noise_and_malformed_lines_are_not_the_answer() {
        let hook =
            r#"{"type":"system","subtype":"hook_started","hook_name":"SessionStart:startup"}"#;
        let malformed = r#"{"type":"control_response","response":{"subtype":"success""#;
        // A catalog body under the wrong record type: only the type check rejects it.
        let wrong_type = format!(
            r#"{{"type":"system","note":"control_response","response":{{"subtype":"success","request_id":"{CATALOG_REQUEST_ID}","response":{{"commands":[{{"name":"x"}}]}}}}}}"#
        );
        for line in [hook, malformed, wrong_type.as_str(), ""] {
            assert_eq!(parse_initialize_response(line), None, "{line}");
        }
    }

    #[test]
    fn a_success_body_without_a_list_is_not_the_answer() {
        for body in [
            r#"{"pid":1,"models":[]}"#,
            r#"{"commands":"x","agents":{"name":"y"}}"#,
            r#"[{"commands":[]}]"#,
            "null",
        ] {
            assert_eq!(parse_initialize_response(&success(body)), None, "{body}");
        }
    }

    #[test]
    fn a_body_with_only_agents_still_yields_them() {
        let got = parse_initialize_response(&success(
            r#"{"agents":[{"name":"sby-agent","description":"Probe","model":"haiku"}]}"#,
        ))
        .expect("an agents array alone is a catalog");
        assert!(got.commands.is_empty());
        assert_eq!(got.agents, [entry("sby-agent", Some("Probe"))]);
    }

    #[test]
    fn malformed_entries_are_skipped_and_duplicates_dropped() {
        let got = parse_initialize_response(&success(
            r#"{"commands":[1,"str",null,{"description":"nameless"},{"name":"  "},{"name":5},
                {"name":" ok ","description":"fine","builtin":true},
                {"name":"b"},{"name":"a","description":"first"},{"name":"a","description":"second"},
                {"name":"c","description":7},{"name":"d","description":"say \u001b[1mhi\n now"}]}"#,
        ))
        .expect("the well-formed entries survive");
        assert_eq!(
            got.commands,
            [
                entry("a", Some("first")),
                entry("b", None),
                entry("c", None),
                entry("d", Some("say [1mhi now")),
                entry("ok", Some("fine")),
            ]
        );
        assert!(got.agents.is_empty());
    }

    /// The real reply lists claude's menu-hidden built-ins as plain
    /// `builtin: true` entries; none of them reaches the pick list.
    #[test]
    fn claudes_hidden_builtins_are_never_offered() {
        let got = parse_initialize_response(&fixture_from(
            "initialize_hidden_builtins.json",
            Some(CATALOG_REQUEST_ID),
        ))
        .expect("the restamped reply is the answer");
        assert_eq!(
            names(&got.commands),
            ["compact", "sbq-manual", "sbq-visible"]
        );
        assert_eq!(names(&got.agents), ["sbq-agent"]);
        assert!(
            got.commands.iter().any(|e| e.name == "sbq-manual"),
            "a disable-model-invocation skill is user-invocable and stays"
        );
    }

    /// Only an entry whose `builtin` is the JSON boolean `true` is dropped by
    /// name. The seven names are literals, so a typo in the const fails here,
    /// and the user's `heapdump` comes AFTER the built-in one, so a dedup ahead
    /// of the filter would keep the wrong one. `pro-trial-expired` and `update`
    /// are hidden too, but claude's print-mode gate keeps them out of the reply,
    /// so they are not pinned and pass.
    #[test]
    fn only_a_builtin_is_dropped_by_name() {
        let got = parse_initialize_response(&success(
            r#"{"commands":[
                {"name":"__remote-workflow","builtin":true},
                {"name":"agents","builtin":true},
                {"name":"design-consent","builtin":true},
                {"name":"design-revoke","builtin":true},
                {"name":"extra-usage","builtin":true},
                {"name":"heapdump","builtin":true},
                {"name":"workflow-launch-exec","builtin":true},
                {"name":"heapdump","description":"mine (user)"},
                {"name":"agents","builtin":false},
                {"name":"extra-usage","builtin":"true"},
                {"name":"pro-trial-expired","builtin":true},
                {"name":"update","builtin":true},
                {"name":"compact","builtin":true}]}"#,
        ))
        .expect("the reply is the answer");
        assert_eq!(
            got.commands,
            [
                entry("agents", None),
                entry("compact", None),
                entry("extra-usage", None),
                entry("heapdump", Some("mine (user)")),
                entry("pro-trial-expired", None),
                entry("update", None),
            ]
        );
    }

    /// Stand-in children (`sh`, `true`, `sleep`, `perl`): no test spawns
    /// `claude`.
    #[cfg(unix)]
    mod children {
        use super::*;

        /// Generous for the stand-ins that answer or exit at once, so a loaded
        /// machine cannot turn a slow spawn into a false failure.
        const ANSWERS_WITHIN: Duration = Duration::from_secs(10);

        /// A stand-in claude: it reads ONE line and, only if that line is an
        /// `initialize` request, drains stdin to EOF and then prints a hook-style
        /// noise line followed by `$1`, the canned reply. It answers only AFTER
        /// EOF, so an answer proves the request arrived AND stdin was closed.
        fn answering(reply: &str) -> Vec<String> {
            [
                "sh",
                "-c",
                r#"IFS= read -r line; cat >/dev/null
case "$line" in *'"subtype":"initialize"'*)
  printf '%s\n' '{"type":"system","subtype":"hook_started"}' "$1";;
esac"#,
                "sh",
                reply,
            ]
            .into_iter()
            .map(str::to_owned)
            .collect()
        }

        fn argv(words: &[&str]) -> Vec<String> {
            words.iter().map(|w| (*w).to_owned()).collect()
        }

        #[test]
        fn a_child_that_answers_the_request_yields_its_catalog() {
            let canned = success(r#"{"commands":[{"name":"sbping","description":"Ping"}]}"#);
            let got = fetch_with(
                &answering(&canned),
                &[],
                &std::env::temp_dir(),
                &initialize_request_line(),
                ANSWERS_WITHIN,
            );
            assert_eq!(
                got.map(|l| l.commands),
                Some(vec![entry("sbping", Some("Ping"))])
            );
            // Control: the stand-in stays silent for any other request.
            let silent = fetch_with(
                &answering(&canned),
                &[],
                &std::env::temp_dir(),
                "{\"type\":\"control_request\",\"request\":{\"subtype\":\"other\"}}\n",
                ANSWERS_WITHIN,
            );
            assert_eq!(silent, None);
        }

        #[test]
        fn a_child_that_exits_without_answering_yields_none() {
            let started = Instant::now();
            let got = fetch_with(
                &argv(&["true"]),
                &[],
                &std::env::temp_dir(),
                &initialize_request_line(),
                ANSWERS_WITHIN,
            );
            assert_eq!(got, None);
            assert!(
                started.elapsed() < ANSWERS_WITHIN,
                "stdout's EOF ends the wait, not the timeout"
            );
        }

        #[test]
        fn a_hung_child_is_killed_and_reaped_at_the_timeout() {
            /// The stand-in would run this long on its own.
            const HANGS_FOR: Duration = Duration::from_secs(5);
            const TIMEOUT: Duration = Duration::from_millis(300);
            /// Well above the timeout plus a kill, well below [`HANGS_FOR`].
            const RETURNS_WITHIN: Duration = Duration::from_secs(3);

            let started = Instant::now();
            let got = fetch_with(
                &argv(&["sh", "-c", &format!("sleep {}", HANGS_FOR.as_secs())]),
                &[],
                &std::env::temp_dir(),
                &initialize_request_line(),
                TIMEOUT,
            );
            let took = started.elapsed();
            assert_eq!(got, None);
            assert!(
                took < RETURNS_WITHIN,
                "the child must be killed at the timeout, not waited out: {took:?}"
            );
        }

        /// A timeout ends the child's whole process group, and the fetch joins its
        /// stdout reader before returning. The stand-in's leader `exec`s into
        /// `sleep 5`, and a background subshell, a grandchild in the same group,
        /// holds stdout open and would touch a marker at 2 s. It pins three
        /// failure modes:
        /// - killing the leader alone: the grandchild lives, so the marker
        ///   appears and the reader, still waiting on its stdout, is not joined;
        /// - no group of its own (`process_group`): the group kill names no
        ///   group, the leader runs out its 5 s, and the call blows its bound;
        /// - a skipped join: `reader_joined` stays `false`.
        #[test]
        fn a_timed_out_child_takes_its_whole_process_group_with_it() {
            const TIMEOUT: Duration = Duration::from_millis(300);
            /// The timeout, plus [`CATALOG_READER_GRACE`], plus slack for a loaded
            /// machine, and well below the leader's own 5 s.
            const RETURNS_WITHIN: Duration = Duration::from_secs(3);
            /// Past the grandchild's 2 s, so a survivor has touched the marker.
            const MARKER_CHECKED_AT: Duration = Duration::from_millis(2500);

            let dir = temp_dir("group");
            let marker = dir.join("grandchild-survived");
            let marker_arg = marker.to_str().expect("a UTF-8 temp path");

            let started = Instant::now();
            let fetched = fetch_reaped(
                &argv(&[
                    "sh",
                    "-c",
                    r#"(sleep 2; touch "$1") & exec sleep 5"#,
                    "sh",
                    marker_arg,
                ]),
                &[],
                &dir,
                &initialize_request_line(),
                TIMEOUT,
            );
            let took = started.elapsed();

            assert_eq!(fetched.listing, None);
            assert!(
                took < RETURNS_WITHIN,
                "the group must be killed at the timeout, not waited out: {took:?}"
            );
            assert!(
                fetched.reader_joined,
                "the stdout reader must be joined before the fetch returns"
            );
            thread::sleep(MARKER_CHECKED_AT.saturating_sub(started.elapsed()));
            assert!(
                !marker.exists(),
                "the grandchild must die with its group, not outlive the fetch"
            );
            std::fs::remove_dir_all(&dir).ok();
        }

        /// A timed-out leader that moved itself into ANOTHER existing process
        /// group is still killed. The group kill names the group it LEFT, so
        /// only [`reap`]'s direct kill reaches it. The stand-in is `perl`,
        /// because `sh` cannot `setpgid` itself: it joins this test process's
        /// group, records `<pgid> <pid>` in a marker, keeps stdout open and
        /// sleeps `RUNS_FOR`. Without the direct kill, `wait` waits those out
        /// and the call blows its bound; a leader that never exited would hang
        /// the worker forever.
        #[test]
        fn a_timed_out_leader_that_left_its_group_is_still_killed() {
            /// The stand-in would run this long on its own.
            const RUNS_FOR: Duration = Duration::from_secs(5);
            /// Leaves `perl` time to start, leave its group and write the
            /// marker before the group kill, so the kill meets a leader that
            /// already left.
            const TIMEOUT: Duration = Duration::from_secs(1);
            /// The timeout, plus [`CATALOG_READER_GRACE`], plus slack for a
            /// loaded machine, and well below [`RUNS_FOR`].
            const RETURNS_WITHIN: Duration = Duration::from_secs(3);

            let dir = temp_dir("left-group");
            let marker = dir.join("leader-pgid-pid");
            let marker_arg = marker.to_str().expect("a UTF-8 temp path");
            let leaves_its_group = format!(
                r#"setpgrp(0, getpgrp(getppid())) or exit 3; open(my $f, ">", $ARGV[0]) or exit 4; print $f getpgrp(0), " ", $$, "\n"; close $f; sleep {}"#,
                RUNS_FOR.as_secs()
            );

            let started = Instant::now();
            let fetched = fetch_reaped(
                &argv(&["perl", "-e", &leaves_its_group, marker_arg]),
                &[],
                &dir,
                &initialize_request_line(),
                TIMEOUT,
            );
            let took = started.elapsed();

            let recorded = std::fs::read_to_string(&marker).unwrap_or_default();
            let ids: Vec<u32> = recorded
                .split_whitespace()
                .filter_map(|id| id.parse().ok())
                .collect();
            assert!(
                matches!(ids[..], [pgid, pid] if pgid != pid),
                "the stand-in must have left its own group before the kill (`perl` must be \
                 on PATH and able to `setpgid`), recorded {recorded:?}"
            );
            assert_eq!(fetched.listing, None);
            assert!(
                took < RETURNS_WITHIN,
                "the leader must be killed at the timeout, not waited out: {took:?}"
            );
            assert!(
                fetched.reader_joined,
                "the leader's stdout closes once it is killed, so the reader is joined"
            );
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn a_missing_program_yields_none() {
            let got = fetch_with(
                &argv(&["snapback-test-no-such-claude"]),
                &[],
                &std::env::temp_dir(),
                &initialize_request_line(),
                ANSWERS_WITHIN,
            );
            assert_eq!(got, None);
            assert_eq!(
                fetch_with(
                    &[],
                    &[],
                    &std::env::temp_dir(),
                    &initialize_request_line(),
                    ANSWERS_WITHIN
                ),
                None,
                "an empty argv is no child, not a panic"
            );
        }

        #[test]
        fn a_missing_cwd_yields_none() {
            let canned = success(r#"{"commands":[{"name":"sbping"}]}"#);
            let got = fetch_with(
                &answering(&canned),
                &[],
                Path::new("/nonexistent/snapback-catalog-cwd"),
                &initialize_request_line(),
                ANSWERS_WITHIN,
            );
            assert_eq!(got, None);
        }

        /// The one command the [`telling_its_form`] stand-in lists when its argv
        /// carries `--setting-sources user`.
        const UNTRUSTED_FORM: &str = "untrusted-form";
        /// ... and the one it lists otherwise.
        const TRUSTED_FORM: &str = "trusted-form";

        /// A stand-in claude that reports which argv form reached it: it reads
        /// one line, drains stdin, and answers a successful catalog whose one
        /// command is [`UNTRUSTED_FORM`] when its arguments (`$*`, everything
        /// [`fetch_in`] appends after the program) hold `--setting-sources user`,
        /// else [`TRUSTED_FORM`].
        fn telling_its_form() -> Vec<String> {
            let reply = |name: &str| success(&format!(r#"{{"commands":[{{"name":"{name}"}}]}}"#));
            let script = format!(
                r#"IFS= read -r line; cat >/dev/null
case " $* " in
  *' --setting-sources user '*) printf '%s\n' '{untrusted}';;
  *) printf '%s\n' '{trusted}';;
esac"#,
                untrusted = reply(UNTRUSTED_FORM),
                trusted = reply(TRUSTED_FORM),
            );
            vec!["sh".to_owned(), "-c".to_owned(), script, "sh".to_owned()]
        }

        /// A fresh, CANONICAL temp dir (on macOS `/var` is `/private/var`), so a
        /// record keyed on it is keyed the way claude keys trust.
        fn temp_dir(tag: &str) -> PathBuf {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            let base =
                std::fs::canonicalize(std::env::temp_dir()).expect("canonicalise the temp dir");
            let dir = base.join(format!(
                "snapback-claude-catalog-{tag}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("create the test's temp dir");
            dir
        }

        /// A global config whose `projects` flags exactly `flagged`, the shape
        /// claude writes.
        fn record_flagging(flagged: &[&Path]) -> String {
            let projects: serde_json::Map<String, Value> = flagged
                .iter()
                .map(|folder| {
                    let key = folder.to_str().expect("a UTF-8 temp path").to_owned();
                    (key, json!({ "hasTrustDialogAccepted": true }))
                })
                .collect();
            json!({ "projects": projects }).to_string()
        }

        /// The command names [`telling_its_form`] answered `folder`'s fetch
        /// with, its trust read from the record at `cfg` (never the real one).
        fn form_fetched(folder: &Path, cfg: &Path) -> Option<Vec<String>> {
            fetch_in(
                folder,
                |folder| claude_trust::folder_trust_in(cfg, folder),
                &telling_its_form(),
                ANSWERS_WITHIN,
            )
            .map(|listing| listing.commands.into_iter().map(|e| e.name).collect())
        }

        /// Every record that does not trust the folder sends the untrusted form
        /// to the child: no flags, a flagged SIBLING, a torn record that would
        /// flag the folder if it parsed, and no record at all.
        #[test]
        fn a_folder_the_record_does_not_trust_is_fetched_with_user_settings_only() {
            let root = temp_dir("untrusted");
            let folder = root.join("folder");
            let sibling = root.join("sibling");
            std::fs::create_dir_all(&folder).expect("create the folder");
            std::fs::create_dir_all(&sibling).expect("create its sibling");
            let trusting = record_flagging(&[&folder]);
            let torn = &trusting[..trusting.len() - 1];
            let sibling_only = record_flagging(&[&sibling]);

            for (name, contents) in [
                ("empty-projects.json", r#"{"projects":{}}"#),
                ("sibling.json", sibling_only.as_str()),
                ("torn.json", torn),
            ] {
                let cfg = root.join(name);
                std::fs::write(&cfg, contents).expect("write the record");
                assert_eq!(
                    form_fetched(&folder, &cfg),
                    Some(vec![UNTRUSTED_FORM.to_owned()]),
                    "{name}"
                );
            }
            assert_eq!(
                form_fetched(&folder, &root.join("missing.json")),
                Some(vec![UNTRUSTED_FORM.to_owned()]),
                "a missing record"
            );
            std::fs::remove_dir_all(&root).ok();
        }

        #[test]
        fn a_folder_the_record_trusts_is_fetched_with_its_project_settings() {
            let root = temp_dir("trusted");
            let folder = root.join("folder");
            std::fs::create_dir_all(&folder).expect("create the folder");
            let cfg = root.join("claude.json");
            std::fs::write(&cfg, record_flagging(&[&folder])).expect("write the record");

            assert_eq!(
                form_fetched(&folder, &cfg),
                Some(vec![TRUSTED_FORM.to_owned()])
            );
            std::fs::remove_dir_all(&root).ok();
        }

        /// The start of the one command [`telling_its_git_env`] lists; the rest
        /// is the child's [`DISABLE_GIT_INSTRUCTIONS_ENV`] value.
        const GIT_ENV_PREFIX: &str = "git-env=";
        /// ... and the rest when the child has no such variable at all.
        const GIT_ENV_UNSET: &str = "unset";

        /// A stand-in claude that reports the git-prefetch switch in its OWN
        /// environment: it reads one line, drains stdin, and answers a successful
        /// catalog whose one command is [`GIT_ENV_PREFIX`] followed by its
        /// [`DISABLE_GIT_INSTRUCTIONS_ENV`] value, or by [`GIT_ENV_UNSET`].
        fn telling_its_git_env() -> Vec<String> {
            let reply = success(&format!(
                r#"{{"commands":[{{"name":"{GIT_ENV_PREFIX}%s"}}]}}"#
            ));
            let script = format!(
                r#"IFS= read -r line; cat >/dev/null
printf '{reply}\n' "${{{DISABLE_GIT_INSTRUCTIONS_ENV}-{GIT_ENV_UNSET}}}""#
            );
            vec!["sh".to_owned(), "-c".to_owned(), script, "sh".to_owned()]
        }

        /// The switch is the untrusted CHILD's alone: that child sees it set over
        /// whatever it inherits, the trusted child sees exactly what this process
        /// has, and this process's own environment is never written.
        #[test]
        fn the_git_prefetch_switch_reaches_only_the_untrusted_child() {
            let before = std::env::var_os(DISABLE_GIT_INSTRUCTIONS_ENV);
            let root = temp_dir("git-env");
            let folder = root.join("folder");
            std::fs::create_dir_all(&folder).expect("create the folder");
            let cfg = root.join("claude.json");
            let fetched_under = |record: &str| {
                std::fs::write(&cfg, record).expect("write the record");
                fetch_in(
                    &folder,
                    |f| claude_trust::folder_trust_in(&cfg, f),
                    &telling_its_git_env(),
                    ANSWERS_WITHIN,
                )
                .map(|listing| names(&listing.commands).join(","))
            };

            assert_eq!(
                fetched_under(r#"{"projects":{}}"#),
                Some(format!("{GIT_ENV_PREFIX}1")),
                "the untrusted child's value overrides any inherited one"
            );
            let inherited = before.as_deref().map_or_else(
                || GIT_ENV_UNSET.to_owned(),
                |value| value.to_string_lossy().into_owned(),
            );
            assert_eq!(
                fetched_under(&record_flagging(&[&folder])),
                Some(format!("{GIT_ENV_PREFIX}{inherited}")),
                "the trusted child inherits this process's environment untouched"
            );
            assert_eq!(
                std::env::var_os(DISABLE_GIT_INSTRUCTIONS_ENV),
                before,
                "the switch is set on the child, never on this process"
            );
            std::fs::remove_dir_all(&root).ok();
        }

        /// The trust read is the WORKER's (AGENTS.md OFF-UI-THREAD): the spawn
        /// returns before a slow read does, the read runs on another thread and
        /// for the asked folder, and its verdict picks the argv the child sees.
        #[test]
        fn the_trust_read_runs_on_the_fetch_worker() {
            /// Longer than a spawn takes, so a read on the SPAWNING thread
            /// cannot beat it by luck.
            const READ_BLOCKS: Duration = Duration::from_millis(300);

            let folder = temp_dir("worker");
            let (tx, rx) = mpsc::channel::<AppEvent>();
            let (read_tx, read_rx) = mpsc::channel();
            let spawner = thread::current().id();

            let spawned_at = Instant::now();
            spawn_fetch_in(
                folder.clone(),
                tx,
                move |cwd: &Path| {
                    let _ = read_tx.send((thread::current().id(), cwd.to_path_buf()));
                    thread::sleep(READ_BLOCKS);
                    FolderTrust::Untrusted
                },
                telling_its_form(),
                ANSWERS_WITHIN,
            );
            let returned_in = spawned_at.elapsed();
            assert!(
                returned_in < READ_BLOCKS,
                "spawning must not wait on the trust read: returned in {returned_in:?}"
            );

            let (read_on, read_for) = read_rx
                .recv_timeout(ANSWERS_WITHIN)
                .expect("the fetch reads the folder's trust");
            assert_ne!(read_on, spawner, "the trust read runs on the worker");
            assert_eq!(read_for, folder, "the trust read is for the asked folder");

            match rx.recv_timeout(ANSWERS_WITHIN) {
                Ok(AppEvent::CatalogFetched { cwd, listing }) => {
                    assert_eq!(cwd, folder);
                    assert_eq!(
                        listing.map(|l| names(&l.commands).join(",")),
                        Some(UNTRUSTED_FORM.to_owned()),
                        "the worker's verdict picks the argv the child sees"
                    );
                }
                other => panic!("the worker must deliver CatalogFetched, got {other:?}"),
            }
            std::fs::remove_dir_all(&folder).ok();
        }
    }

    /// The worker has the settings read's one-shot shape: the spawn returns
    /// before the fetch does, exactly one `CatalogFetched` arrives carrying the
    /// asked `cwd` and the fetch's answer verbatim, and the thread then ends
    /// (its sender drops, disconnecting the receiver).
    #[test]
    fn the_worker_delivers_exactly_one_catalog_fetched() {
        /// Longer than a spawn takes, so an INLINE fetch cannot beat it by luck.
        const FETCH_BLOCKS: Duration = Duration::from_millis(300);
        const DELIVERED_WITHIN: Duration = Duration::from_secs(2);

        let (tx, rx) = mpsc::channel::<AppEvent>();
        let asked = PathBuf::from("/tmp/snapback-catalog-project");
        let stated = Listing {
            commands: vec![entry("sbping", Some("Ping"))],
            agents: vec![entry("sby-agent", None)],
        };
        let answer = stated.clone();
        let fetched_in = asked.clone();

        let spawned_at = Instant::now();
        spawn_fetch_with(asked.clone(), tx, move |cwd| {
            assert_eq!(cwd, fetched_in, "the fetch runs in the asked folder");
            thread::sleep(FETCH_BLOCKS);
            Some(answer)
        });
        let returned_in = spawned_at.elapsed();
        assert!(
            returned_in < FETCH_BLOCKS,
            "spawning must not wait on the fetch: returned in {returned_in:?}"
        );

        match rx.recv_timeout(DELIVERED_WITHIN) {
            Ok(AppEvent::CatalogFetched { cwd, listing }) => {
                assert_eq!(cwd, asked);
                assert_eq!(listing, Some(stated));
            }
            other => panic!("the worker must deliver CatalogFetched, got {other:?}"),
        }
        match rx.recv_timeout(DELIVERED_WITHIN) {
            Err(RecvTimeoutError::Disconnected) => {}
            other => panic!("a one-shot sends once and ends, got {other:?}"),
        }
    }

    /// "No answer" is delivered too, never swallowed: the consumer decides that a
    /// `None` is not cached.
    #[test]
    fn the_worker_delivers_no_answer_as_none() {
        let (tx, rx) = mpsc::channel::<AppEvent>();
        spawn_fetch_with(PathBuf::from("/tmp"), tx, |_| None);
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(AppEvent::CatalogFetched { listing, .. }) => assert_eq!(listing, None),
            other => panic!("no answer is still delivered, got {other:?}"),
        }
    }
}
