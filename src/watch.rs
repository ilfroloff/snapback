//! Filesystem watcher + debounced event stream.
//!
//! This module owns three things the TUI update loop depends on:
//!
//! * [`AppEvent`] — the unified event the elm-style loop consumes. Every input
//!   source is normalized into this one enum.
//! * [`SessionWatcher`] — a recursive `notify` watcher over the store root,
//!   debounced ~200ms via `notify-debouncer-mini`. It *coalesces* an entire
//!   debounce batch (any number of raw filesystem events, across any number of
//!   paths) into a single [`AppEvent::SessionsChanged`], so an event storm from
//!   rapid writes collapses to one reload signal. Each batch is also *filtered*
//!   by the same depth-2 `.jsonl` predicate `store::discover::discover` uses;
//!   irrelevant files and deep subagent paths are dropped, and any uncertain
//!   path falls through to reload.
//! * [`EventLoop`] — merges three sources into ONE receiver: a crossterm input
//!   thread, the watcher channel, and a periodic tick.
//!
//! Later phases consume these: `search` and the `tui/*` modules match on
//! [`AppEvent`] and drive the loop; they import `crate::watch::AppEvent`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, LazyLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, Event as CrosstermEvent};
use notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{new_debouncer, DebounceEventResult, DebouncedEventKind, Debouncer};

use crate::agents::{self, ReportedAgent};
use crate::store::discover::{is_session_path, store_depth, StoreDepth};

/// Debounce window for coalescing filesystem event storms into one reload.
///
/// The debouncer waits this long after the last raw event before emitting, so
/// a burst of rapid writes is delivered as a single batch.
pub const DEBOUNCE: Duration = Duration::from_millis(200);

/// Default cadence of the periodic [`AppEvent::Tick`] in the merged loop.
pub const TICK: Duration = Duration::from_millis(250);

/// Cadence of the OFF-THREAD `claude agents --json --all` poll that refreshes the
/// reported-agent badges.
///
/// Measured: each `claude agents --json --all` spawn costs ~0.64 CPU-s. On a
/// 1 s period that is a ~36 % duty cycle; on a 5 s period it is ~7 %. The poll
/// is only a display signal (`set_reported_agents`) — every real decision
/// (resume gate, attach, delete confirm) re-probes via [`crate::agents::live_agents`],
/// so a slightly staler badge is cheap compared with the CPU it saves.
///
/// See also [`AGENTS_IDLE_AFTER`], which skips the shell-out entirely while the
/// board is idle.
pub const AGENTS_REFRESH: Duration = Duration::from_millis(5000);

/// How long the board can be idle before the `claude agents` poll is skipped.
///
/// Idle means no input events and no filesystem changes that triggered a reload.
/// While idle the badge may go stale, but the board is not changing, and the
/// first input or `SessionsChanged` event after idle forces a poll on the next
/// [`AGENTS_REFRESH`] loop turn.
///
/// 60 s is a compromise: long enough to stop the constant ~3.0-core drain from
/// instances left open on quiescent desktops, short enough that badges refresh
/// within a minute of the user coming back.
pub const AGENTS_IDLE_AFTER: Duration = Duration::from_secs(60);

/// How long the input thread blocks in `crossterm::event::poll` before waking to
/// re-check its shutdown flag.
///
/// Kept small so [`EventLoop`]'s join-on-drop returns promptly — the resume
/// handoff must not stall noticeably while the reader is torn down before
/// `claude` is spawned onto the same stdin — yet large enough to avoid a busy
/// spin between polls.
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Process-local epoch for the board-activity stamp.
///
/// All activity timestamps are millis since this instant, so they stay small
/// (`u64`) and comparable across threads without needing a `SystemTime` clock.
static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Return millis since [`EPOCH`]. Used for the board-activity stamp.
fn now_ms() -> u64 {
    EPOCH.elapsed().as_millis() as u64
}

/// Decide whether the `claude agents` poll is due given board activity.
///
/// The poll runs while the board has been active within the last
/// `idle_after` window. Once idle for `idle_after` or longer, the shell-out is
/// skipped to save CPU. The first input or `SessionsChanged` event after idle
/// refreshes the activity stamp, so polling resumes on the next loop turn
/// (within one [`AGENTS_REFRESH`]).
///
/// `last_activity_ms` and `now_ms` are both millis since the process-local
/// [`EPOCH`], seeded fresh by [`EventLoop::new`] at construction — so there is
/// no "never active" state to special-case here, and in particular a
/// `last_activity_ms` of `0` is an ORDINARY reading (the very first `now_ms()`
/// call, taken before `EPOCH`'s `LazyLock` has measured any elapsed time), not
/// a sentinel. Treating `0` as special would collide with that legitimate
/// reading and silently swallow the first poll on every fresh launch — keep
/// this gate a plain elapsed-time comparison so it cannot.
///
/// This function is pure: use it with `now_ms()` and the shared activity stamp
/// in the agent poller thread.
pub fn agents_poll_due(last_activity_ms: u64, now_ms: u64, idle_after: Duration) -> bool {
    let idle_ms = idle_after.as_millis() as u64;
    now_ms.saturating_sub(last_activity_ms) < idle_ms
}

/// A single event consumed by the TUI update loop, from any source.
#[derive(Debug)]
pub enum AppEvent {
    /// A terminal input event (key press, resize, mouse, ...).
    Input(CrosstermEvent),
    /// The session store on disk changed; the update loop should reload.
    ///
    /// Emitted at most once per debounce window regardless of how many raw
    /// filesystem events fired within it.
    SessionsChanged,
    /// A refreshed reported-agent set from `claude agents --json --all`, computed
    /// OFF the UI thread by the agents poller and delivered on the same channel
    /// so the shell-out never blocks rendering.
    ///
    /// Reported, not live: the set includes agents that reported completion, and
    /// it is a DISPLAY signal only — every hand-off (the resume gate, and Attach
    /// for its job id) probes claude directly rather than reading it (see
    /// [`crate::agents::live_agents`]).
    ReportedAgents(HashMap<String, ReportedAgent>),
    /// A one-shot quick-reply send finished (`claude -p -r <id>`), delivered OFF
    /// the UI thread by the detached send driver (see [`crate::send::spawn_send`]).
    ///
    /// Contrast the recurring [`ReportedAgents`](Self::ReportedAgents): this fires
    /// EXACTLY ONCE per send, from a thread spawned for that one send, not from a
    /// poller. `status` is the mapped result (cost on success, an error hint on
    /// failure — see [`crate::send::status_for_send`]); `session_id` is the
    /// authoritative id the send targeted, so the handler can tell whether the
    /// finished send is the row currently previewed and re-anchor it to the
    /// newest turn.
    SendFinished {
        /// Authoritative `sessionId` the send targeted.
        session_id: String,
        /// Mapped board status for the completed send.
        status: String,
        /// Whether the status is a transient confirmation (`true`) or a sticky
        /// failure/refusal (`false`). Classified by the send mapper so the UI never
        /// infers it from the text.
        success: bool,
    },
    /// A one-shot interrupt (`claude stop <job-id>`) finished, delivered OFF the UI
    /// thread by the detached interrupt driver (see [`crate::send::spawn_interrupt`]).
    ///
    /// Like [`SendFinished`](Self::SendFinished) it fires EXACTLY ONCE per interrupt,
    /// from a thread spawned for that one stop. `status` is the mapped result
    /// (`"stopped"` on success, a reason on failure — see
    /// [`crate::send::status_for_stop`]). The transcript itself is unchanged by
    /// stopping, so there is nothing to reconcile beyond surfacing the result — but
    /// the completion still carries the target `session_id` so the handler only
    /// clears the in-flight guard when the interrupt that finished is the one the
    /// board is still tracking, and a stale result cannot land on a surface that has
    /// moved on.
    InterruptFinished {
        /// Authoritative `sessionId` the interrupt targeted.
        session_id: String,
        /// Mapped board status for the completed interrupt.
        status: String,
        /// Whether the status is a transient confirmation (`true`) or a sticky
        /// failure/refusal (`false`).
        success: bool,
    },
    /// A one-shot background-agent launch (`claude [--agent <name>] --bg <prompt>`)
    /// finished, delivered OFF the UI thread by the detached launch driver (see
    /// [`crate::send::spawn_bg_launch`]).
    ///
    /// Like [`SendFinished`](Self::SendFinished) it fires EXACTLY ONCE per launch,
    /// from a thread spawned for that one launch. `status` is the mapped result
    /// (started / started-but-warned / a reason on failure — see
    /// [`crate::send::status_for_bg_launch`]).
    ///
    /// There is nothing to key by ROW — a brand-new agent has no `sessionId` until
    /// claude mints one, and it reaches the board through the ordinary watcher →
    /// reload path — but there IS something to key by DISPATCH. `launch_id` is the
    /// board-local id [`crate::tui::app::App::dispatch_draft`] stamped on the card,
    /// echoed back so the handler closes the card that launch belongs to and never
    /// a compose the user opened while waiting.
    BgLaunchFinished {
        /// The dispatch this result belongs to (see
        /// [`crate::tui::app::App::launching_draft`]).
        launch_id: u64,
        /// Mapped board status for the completed launch.
        status: String,
        /// Whether the status is a transient clean start (`true`) or a sticky
        /// warned/failed start (`false`). The warned case stays sticky so a silent
        /// downgrade is never auto-dismissed.
        success: bool,
    },
    /// A periodic wake-up. The update loop does nothing costly on this.
    Tick,
}

/// Classification of a single watcher path relative to the store root.
///
/// `notify` may report arbitrary paths under the store root (and, on macOS,
/// occasionally a directory path because FSEvents coalesces at directory
/// granularity). The watcher therefore filters each batch by the same
/// depth-pinned `.jsonl` rule `discover()` uses, but conservatively: anything
/// whose name-shape or proven file-type cannot hold a consumable session is
/// `Ignorable`; anything uncertain falls through as `Ambiguous` and triggers a
/// reload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchPathClass {
    /// A depth-2 `.jsonl` path: this is exactly the shape `discover()` consumes.
    Session,
    /// Definitely not a session file: too deep, or a proven non-session file/dir.
    Ignorable,
    /// Uncertain (metadata missing, or a directory whose children might be
    /// sessions). Reload to be safe.
    Ambiguous,
}

/// Pure core of [`classify_watch_path`]: given where the path sits in the store
/// shape, whether the name-shape matches a session file, and the known metadata
/// facts, return the class.
///
/// The matrix is the load-bearing design; this helper keeps it unit-testable
/// without touching the filesystem. It takes a [`StoreDepth`] rather than a
/// number so the store's level arithmetic stays in
/// [`crate::store::discover`] — the watcher REACTS to the shape, it does not
/// re-derive it (see the SUBAGENT EXCLUSION BY DEPTH rule in `AGENTS.md`).
fn classify_with_metadata(
    depth: StoreDepth,
    is_session: bool,
    is_file: bool,
    is_dir: bool,
) -> WatchPathClass {
    if is_session {
        return WatchPathClass::Session;
    }
    match depth {
        // Not under the root at all: the store shape rules nothing out, so
        // never drop it on a guess.
        StoreDepth::Outside => WatchPathClass::Ambiguous,
        // The root itself; changes there can reshape the whole tree, so treat
        // it as uncertain.
        StoreDepth::Root => WatchPathClass::Ambiguous,
        // An `<encoded-cwd>` directory holds sessions; only a proven file can
        // be ignored. Missing metadata means we cannot tell file from directory.
        StoreDepth::CwdLevel => {
            if is_file {
                WatchPathClass::Ignorable
            } else {
                WatchPathClass::Ambiguous
            }
        }
        // A session-level DIRECTORY's children sit below the consumable shape
        // and are therefore unconsumed; a session-level non-session file is
        // also safe to ignore. Missing metadata after a remove is ambiguous.
        StoreDepth::SessionLevel => {
            if is_file || is_dir {
                WatchPathClass::Ignorable
            } else {
                WatchPathClass::Ambiguous
            }
        }
        // Below the consumable shape: cannot contain a session by the subagent-
        // exclusion rule, regardless of metadata.
        StoreDepth::BelowSession => WatchPathClass::Ignorable,
    }
}

/// Classify a watcher-reported path relative to the store root.
///
/// Reads BOTH halves of the shape rule from `store::discover` — the level from
/// [`store_depth`] and the name from [`is_session_path`] — and conservatively
/// inspects metadata only to widen the `Ignorable` classification. Paths
/// outside the root or whose metadata is missing are `Ambiguous` so the watcher
/// never drops a real change on a guess.
pub fn classify_watch_path(root: &Path, path: &Path) -> WatchPathClass {
    let depth = store_depth(root, path);
    let is_session = is_session_path(root, path);
    let meta = std::fs::metadata(path);
    let is_file = meta.as_ref().map(|m| m.is_file()).unwrap_or(false);
    let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
    classify_with_metadata(depth, is_session, is_file, is_dir)
}

/// Decide whether a debounce batch should trigger a store reload.
///
/// A batch needs a reload if any path in it is not definitely ignorable. An
/// empty batch needs no reload.
pub fn batch_needs_reload(classes: impl IntoIterator<Item = WatchPathClass>) -> bool {
    classes.into_iter().any(|c| c != WatchPathClass::Ignorable)
}

/// A live, debounced filesystem watcher over the store root.
///
/// Holds the underlying debouncer alive for its lifetime; dropping this stops
/// watching. Each debounce batch is coalesced into exactly one
/// [`AppEvent::SessionsChanged`] sent on the provided channel.
pub struct SessionWatcher {
    // The debouncer owns the notify watcher + its worker thread. It must be kept
    // alive for events to keep flowing, hence stored even though never read.
    _debouncer: Debouncer<RecommendedWatcher>,
}

impl SessionWatcher {
    /// Start watching `root` recursively, forwarding a coalesced
    /// [`AppEvent::SessionsChanged`] to `tx` for each debounce batch.
    ///
    /// The debouncer invokes the handler once per ~[`DEBOUNCE`] window with the
    /// full batch of changed paths; the handler maps that entire batch to a
    /// single send, so `N` rapid writes produce ONE `SessionsChanged`.
    ///
    /// Before sending, the batch is filtered by the same depth-2 `.jsonl`
    /// predicate that [`crate::store::discover::discover`] uses (see
    /// [`classify_watch_path`]). Paths that cannot be a session file and are
    /// provably not a directory that could contain one are dropped; anything
    /// uncertain falls through to reload so a real change is never missed.
    pub fn spawn(root: &Path, tx: Sender<AppEvent>, activity: Arc<AtomicU64>) -> Result<Self> {
        // `notify` reports canonical paths (on macOS `/var` resolves to
        // `/private/var`), so classify against the canonical root to keep
        // `strip_prefix` from failing on every event.
        let watch_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let mut debouncer = new_debouncer(DEBOUNCE, move |res: DebounceEventResult| {
            // Coalesce an entire debounce batch into exactly ONE reload signal,
            // reacting only to SETTLED changes. notify-debouncer-mini emits an
            // intermediate `AnyContinuous` heartbeat while a path is still
            // being written, then a final `Any` once it settles. A rapid write
            // storm therefore surfaces as (one or more) `AnyContinuous` batches
            // followed by a single `Any` batch; keying on `Any` collapses the
            // whole storm into one signal and never reloads mid-write. Errors
            // and continuous-only batches are skipped to keep it deterministic.
            let settled = matches!(res, Ok(ref events)
                if events.iter().any(|e| matches!(e.kind, DebouncedEventKind::Any)));
            if settled {
                // Filter the settled batch by the same depth-2 `.jsonl` predicate
                // `discover()` uses. Only paths that might be a session file (or
                // whose metadata/status is uncertain) trigger a reload; irrelevant
                // files and deep subagent paths are dropped before any store work.
                let Ok(events) = res else { return };
                if batch_needs_reload(
                    events
                        .iter()
                        .map(|e| classify_watch_path(&watch_root, &e.path)),
                ) {
                    // A real session-affecting change: touch the activity stamp
                    // so the agents poller knows the board is not idle.
                    activity.store(now_ms(), Ordering::Relaxed);
                    // A send failure means the receiver (TUI) has gone away; ignore.
                    let _ = tx.send(AppEvent::SessionsChanged);
                }
            }
        })
        .context("failed to create filesystem debouncer")?;

        debouncer
            .watcher()
            .watch(root, RecursiveMode::Recursive)
            .with_context(|| format!("failed to watch store root {}", root.display()))?;

        Ok(Self {
            _debouncer: debouncer,
        })
    }
}

/// Merges the three event sources into a single [`AppEvent`] receiver.
///
/// On construction it spawns:
/// * a crossterm input thread forwarding [`AppEvent::Input`],
/// * the [`SessionWatcher`] forwarding [`AppEvent::SessionsChanged`],
/// * a tick thread forwarding [`AppEvent::Tick`] every `tick`,
///
/// all onto one channel. The TUI update loop reads that single receiver.
pub struct EventLoop {
    rx: Receiver<AppEvent>,
    // A live sender clone, used to spawn additional off-thread producers (the
    // live-agents poller) that deliver onto the SAME merged channel.
    tx: Sender<AppEvent>,
    // Kept alive so the watcher keeps running for the loop's lifetime.
    _watcher: SessionWatcher,
    // Set to `true` on drop to tell every thread this loop owns to stop. The
    // input reader polls it between reads (rather than blocking forever on
    // stdin) and the agents poller checks it once per loop turn, so BOTH can
    // observe the request and exit — the poller regardless of whether its idle
    // gate let a shell-out through.
    shutdown: Arc<AtomicBool>,
    // The input thread's handle, joined on drop so the reader is fully gone
    // BEFORE this `EventLoop` (and thus `run`) returns and `claude` is spawned
    // onto the same fd 0. `Option` so `Drop` can `take()` it out to `join()`.
    input_handle: Option<JoinHandle<()>>,
    // Board-activity timestamp: millis since [`EPOCH`], touched only by real
    // activity sources (crossterm input events and emitted `SessionsChanged`)
    // and read by the agents poller to decide whether to shell out. The 250 ms
    // `Tick` thread MUST NEVER touch this stamp, or the board would never idle.
    activity: Arc<AtomicU64>,
}

impl EventLoop {
    /// Wire the input thread, filesystem watcher, and tick thread onto one
    /// receiver, watching `root` and ticking every `tick`.
    ///
    /// Creates the shared board-activity stamp seeded with the current time, so
    /// the first agents poll (started separately via [`spawn_agents_poller`]) is
    /// considered due immediately.
    pub fn new(root: &Path, tick: Duration) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<AppEvent>();
        let activity = Arc::new(AtomicU64::new(now_ms()));
        let watcher = SessionWatcher::spawn(root, tx.clone(), Arc::clone(&activity))?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let input_handle = spawn_input_thread(
            tx.clone(),
            Arc::clone(&shutdown),
            Arc::clone(&activity),
            read_terminal_event,
        );
        spawn_tick_thread(tx.clone(), tick);
        Ok(Self {
            rx,
            tx,
            _watcher: watcher,
            shutdown,
            input_handle: Some(input_handle),
            activity,
        })
    }

    /// Start the OFF-THREAD reported-agent poller (real runtime path only).
    ///
    /// Spawns a dedicated thread that runs `claude agents --json --all` every
    /// `interval` and delivers each result as [`AppEvent::ReportedAgents`] on the
    /// shared channel, so the shell-out can NEVER block rendering. It is not
    /// started by [`new`](Self::new) so the event-loop unit tests never spawn
    /// `claude`; [`crate::tui::run`] opts in once per board session. The thread
    /// exits when this loop drops (see [`spawn_agents_thread`]), so it is
    /// bounded to the board session like the tick thread.
    ///
    /// This is the ONE production call site, and it is where the real shell-out
    /// ([`crate::agents::reported_agents`]) and the real idle window
    /// ([`AGENTS_IDLE_AFTER`]) are named — both are parameters of the thread
    /// rather than constants baked into its body, so the suite can state a small
    /// window and a stated poll instead of spawning `claude`.
    ///
    /// The poller skips the shell-out while the board is idle (see
    /// [`AGENTS_IDLE_AFTER`]), but it keeps looping on `interval` so activity
    /// resuming polls within one interval.
    pub fn spawn_agents_poller(&self, interval: Duration) {
        spawn_agents_thread(
            self.tx.clone(),
            interval,
            AGENTS_IDLE_AFTER,
            Arc::clone(&self.activity),
            Arc::clone(&self.shutdown),
            agents::reported_agents,
        );
    }

    /// A clone of the merged channel's sender, for spawning a one-shot off-thread
    /// producer that delivers back onto the SAME receiver.
    ///
    /// Used by [`crate::tui::run`] to hand the detached quick-reply send driver
    /// ([`crate::send::spawn_send`]) a channel for its lone
    /// [`AppEvent::SendFinished`], the same way [`spawn_agents_poller`] clones it
    /// for the recurring agents poll. Cloning a `Sender` keeps the channel open;
    /// the send thread's clone drops when it finishes.
    ///
    /// [`spawn_agents_poller`]: Self::spawn_agents_poller
    #[must_use]
    pub fn sender(&self) -> Sender<AppEvent> {
        self.tx.clone()
    }

    /// Block until the next merged event, or `None` once all senders drop.
    pub fn recv(&self) -> Option<AppEvent> {
        self.rx.recv().ok()
    }

    /// Block for the next event up to `timeout`; `None` on timeout/disconnect.
    ///
    /// Not on the binary's runtime path — the TUI loop blocks on
    /// [`recv`](Self::recv) — but exercised by this module's watcher tests, which
    /// poll with a timeout so they never block forever. Retained + `dead_code`
    /// allowed narrowly here (rather than crate-wide) for that reason.
    #[allow(dead_code)]
    pub fn recv_timeout(&self, timeout: Duration) -> Option<AppEvent> {
        self.rx.recv_timeout(timeout).ok()
    }
}

impl Drop for EventLoop {
    /// Signal every thread this loop owns to stop, then join the input reader,
    /// before the loop goes away.
    ///
    /// Setting the flag first and *joining* second guarantees the reader thread
    /// has fully exited — and so has released its hold on stdin (fd 0) — by the
    /// time this returns. Because `run`'s local `EventLoop` drops as part of
    /// `run` returning (before control reaches `main`), the reader is provably
    /// gone before `resume::launch` spawns `claude` onto that same fd 0, so the
    /// two never contend for keystrokes. Joining here also bounds each resume
    /// round trip's thread to that iteration: it is joined before the next
    /// `EventLoop::new` spawns a fresh one, so input threads never accumulate.
    /// Join errors (a panicked reader) are ignored — teardown is best-effort.
    ///
    /// The same flag is what bounds the agents poller (see
    /// [`spawn_agents_thread`]). That one is NOT joined — it may be mid
    /// shell-out, and a hand-off must not wait on `claude` — so it is bounded
    /// rather than awaited: it reads the flag at the top of every loop turn and
    /// exits within one `interval` of the turn that observes it, plus — for a
    /// flag set just after a check — the `claude` shell-out that turn had already
    /// begun, since the sleep comes after the poll. Bounded, not immediate: that
    /// slack is precisely what not waiting on a child costs.
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.input_handle.take() {
            let _ = handle.join();
        }
    }
}

/// Wait up to `timeout` for a real terminal event: the PRODUCTION input source
/// for [`spawn_input_thread`].
///
/// Crossterm's `poll` + `read` pair, collapsed into the one call the reader loop
/// makes per turn. `Ok(None)` is the timeout (no event within `timeout`) and
/// EITHER half's `Err` propagates unchanged, so an absent or torn-down terminal
/// still ends the read exactly as a bare `poll`/`read` pair did.
fn read_terminal_event(timeout: Duration) -> std::io::Result<Option<CrosstermEvent>> {
    if event::poll(timeout)? {
        Ok(Some(event::read()?))
    } else {
        Ok(None)
    }
}

/// Forward terminal events as [`AppEvent::Input`] on a poll loop that wakes
/// every [`INPUT_POLL_INTERVAL`] to observe `shutdown`, returning the thread's
/// [`JoinHandle`] so [`EventLoop`] can join it on drop.
///
/// Unlike a bare blocking `event::read()`, polling with a bounded timeout lets
/// the thread notice a shutdown request instead of parking forever on stdin.
/// The thread exits when any of the following occurs, so `join()` can never
/// hang (including in a non-TTY/test environment where the terminal source
/// errors):
///
/// * `shutdown` is observed set (clean teardown from [`EventLoop`]'s `Drop`);
/// * a send fails, meaning the TUI receiver has gone away;
/// * `read` returns `Err` (no terminal / stdin closed).
///
/// Every event that arrives also stamps `activity`, the board-activity clock the
/// agents poller gates on: that store is the "first input after idle resumes
/// polling" half of [`AGENTS_IDLE_AFTER`]'s guarantee. A timed-out turn
/// deliberately leaves the stamp alone — a 50 ms wake-up with nothing to read is
/// not board activity, and stamping it would keep the poll (and its `claude`
/// spawn) awake forever on a board nobody is using.
///
/// `read` is the terminal source, and it is a PARAMETER for the same reason
/// [`spawn_agents_thread`]'s `poll` is: a SEAM, swapped by production exactly
/// never — [`EventLoop::new`] passes [`read_terminal_event`] and is the only call
/// site that passes anything — so the suite can STATE an input event instead of
/// needing a TTY it does not have, and assert what this loop does with one.
fn spawn_input_thread<F>(
    tx: Sender<AppEvent>,
    shutdown: Arc<AtomicBool>,
    activity: Arc<AtomicU64>,
    read: F,
) -> JoinHandle<()>
where
    F: Fn(Duration) -> std::io::Result<Option<CrosstermEvent>> + Send + 'static,
{
    thread::spawn(move || loop {
        if shutdown.load(Ordering::Relaxed) {
            break; // Clean shutdown requested by EventLoop::drop.
        }
        match read(INPUT_POLL_INTERVAL) {
            // An event arrived: forward it.
            Ok(Some(ev)) => {
                // Real user activity: keep the agents poller awake.
                activity.store(now_ms(), Ordering::Relaxed);
                if tx.send(AppEvent::Input(ev)).is_err() {
                    break; // TUI receiver gone.
                }
            }
            // Timed out with no event: loop back to re-check the shutdown flag.
            Ok(None) => {}
            // Poll or read failed (no terminal): exit rather than busy-spin.
            Err(_) => break,
        }
    })
}

/// Emit an [`AppEvent::Tick`] every `interval` until the receiver drops.
fn spawn_tick_thread(tx: Sender<AppEvent>, interval: Duration) {
    thread::spawn(move || loop {
        thread::sleep(interval);
        if tx.send(AppEvent::Tick).is_err() {
            break;
        }
    });
}

/// Run `poll` OFF-THREAD, emitting an [`AppEvent::ReportedAgents`] immediately
/// and then every `interval`, until the owning [`EventLoop`] goes away.
///
/// The first poll fires BEFORE any sleep so badges appear on load; each
/// subsequent poll refreshes them on the autorefresh cadence. In production
/// `poll` is [`crate::agents::reported_agents`], whose shell-out is fail-soft: a
/// missing `claude` or bad output just delivers an empty set. It is a PARAMETER
/// rather than a direct call for the reason `App::live_probe` is — it is a SEAM,
/// swapped by production exactly never, so the suite can state a poll's answer
/// (and count that it happened at all) without spawning a child. `idle_after` is
/// a parameter for the same reason: a test states a window measured in
/// milliseconds instead of waiting out [`AGENTS_IDLE_AFTER`]'s real minute.
///
/// While the board has been idle longer than `idle_after`, the poll is skipped
/// but the loop keeps sleeping on `interval`, so the first input or
/// `SessionsChanged` event after idle resumes polling within one interval.
///
/// **Two exits, and the FIRST is the structural one.** Every loop turn begins by
/// reading `shutdown` — the flag [`EventLoop`]'s `Drop` sets — so the thread is
/// bounded to its board session whether or not the idle gate ever lets a poll
/// through. That check is what keeps the guarantee unconditional: the older
/// exit-on-failed-send alone became circumstantial the moment a poll could be
/// skipped, since a board left idle across a resume round trip would send
/// nothing, notice nothing, and accumulate one poller per round trip. The failed
/// send is still honored as the second exit, for the window between the receiver
/// dropping and the flag being read.
fn spawn_agents_thread<F>(
    tx: Sender<AppEvent>,
    interval: Duration,
    idle_after: Duration,
    activity: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    poll: F,
) where
    F: Fn() -> HashMap<String, ReportedAgent> + Send + 'static,
{
    thread::spawn(move || loop {
        if shutdown.load(Ordering::Relaxed) {
            break; // The EventLoop that owns the receiver is going away.
        }
        let now = now_ms();
        let last = activity.load(Ordering::Relaxed);
        if agents_poll_due(last, now, idle_after) {
            let reported = poll();
            if tx.send(AppEvent::ReportedAgents(reported)).is_err() {
                break; // TUI receiver gone.
            }
        }
        thread::sleep(interval);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    /// A board-activity stamp value [`now_ms`] can never produce, so a test can
    /// tell "the watcher wrote a real reading here" from "nothing touched it".
    ///
    /// Seeding with `0` cannot do that job: `0` is a legitimate reading (the
    /// very first `now_ms()` call, before `EPOCH`'s `LazyLock` has measured any
    /// elapsed time), and which test makes that call first is up to the test
    /// harness. `now_ms` counts millis since a process-local epoch, so it can
    /// never reach `u64::MAX`.
    const UNTOUCHED: u64 = u64::MAX;

    /// A unique, empty temp directory to use as an isolated store root.
    ///
    /// Never touches the real `~/.claude/projects`.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "snapback-watch-{tag}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp store dir");
        dir
    }

    /// Poll the loop until `pred` matches an event or `budget` elapses.
    fn wait_for(events: &EventLoop, budget: Duration, pred: impl Fn(&AppEvent) -> bool) -> bool {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            if let Some(ev) = events.recv_timeout(Duration::from_millis(100)) {
                if pred(&ev) {
                    return true;
                }
            }
        }
        false
    }

    /// Re-arm the board-activity stamp to [`UNTOUCHED`] after a test's SETUP.
    ///
    /// Every watcher test has to create the `<encoded-cwd>` directory its
    /// subject lives in, and a new directory is an AMBIGUOUS path — it can hold
    /// sessions — so the watcher rightly reloads and stamps for it. Re-arming
    /// once the setup's events are drained scopes the stamp assertions to the
    /// MEASURED change alone, which is the only thing they mean to claim.
    fn rearm(activity: &Arc<AtomicU64>) {
        activity.store(UNTOUCHED, Ordering::Relaxed);
    }

    /// Poll `pred` until it holds or `budget` elapses.
    fn wait_until(budget: Duration, mut pred: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            if pred() {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        pred()
    }

    /// The id the poll double below reports, so a test can tell the set it
    /// returned from an empty one the real shell-out would produce.
    const POLLED_ID: &str = "polled-by-the-double";

    /// A `read` double for [`spawn_input_thread`]: it COUNTS the turns the
    /// reader loop actually took and hands back ONE stated event each time the
    /// test releases it, so the loop's behavior is assertable without a TTY
    /// (production's `read` is [`read_terminal_event`], which needs a real
    /// terminal — without a controlling terminal, as in CI, the real reader
    /// errors out immediately and never reaches the body under test).
    ///
    /// It sleeps `timeout` on an empty turn exactly as `crossterm::event::poll`
    /// blocks, so a test never busy-spins the reader, and it CLEARS `release` as
    /// it fires so one request yields exactly one event.
    ///
    /// The returned closure owns a handle on `turns`, and the reader thread owns
    /// the closure — so `Arc::strong_count(turns)` falling back to `1` is also
    /// proof the thread has EXITED, the same standing [`counting_poll`] gives the
    /// poller's `calls`. That is what lets a shutdown claim be asserted under a
    /// BOUND instead of awaited by an unbounded `join()`.
    fn stated_input(
        turns: &Arc<AtomicUsize>,
        release: &Arc<AtomicBool>,
    ) -> impl Fn(Duration) -> std::io::Result<Option<CrosstermEvent>> + Send + 'static {
        let turns = Arc::clone(turns);
        let release = Arc::clone(release);
        move |timeout| {
            turns.fetch_add(1, Ordering::Relaxed);
            if release.swap(false, Ordering::Relaxed) {
                return Ok(Some(CrosstermEvent::Key(KeyEvent::new(
                    KeyCode::Char('j'),
                    KeyModifiers::NONE,
                ))));
            }
            thread::sleep(timeout);
            Ok(None)
        }
    }

    /// A `poll` double for [`spawn_agents_thread`]: it COUNTS the polls that
    /// actually ran and hands back a STATED set, so a test can pin the idle gate
    /// without spawning `claude` (production's `poll` is
    /// [`crate::agents::reported_agents`], which shells out).
    ///
    /// The returned closure owns a handle on `calls`, and the poller thread owns
    /// the closure — so `Arc::strong_count(&calls)` falling back to `1` is also
    /// proof the thread has EXITED, without threading a `JoinHandle` through the
    /// production seam for a test's benefit alone.
    fn counting_poll(
        calls: &Arc<AtomicUsize>,
    ) -> impl Fn() -> HashMap<String, ReportedAgent> + Send + 'static {
        let calls = Arc::clone(calls);
        move || {
            calls.fetch_add(1, Ordering::Relaxed);
            HashMap::from([(
                POLLED_ID.to_string(),
                ReportedAgent {
                    kind: "background".to_string(),
                    id: None,
                    state: None,
                    status: None,
                    name: None,
                },
            )])
        }
    }

    /// A board-activity stamp that is provably OUTSIDE `idle_after` — an IDLE
    /// board — by taking the reading first and then letting the real clock run
    /// past the window.
    ///
    /// Subtracting from `now_ms()` instead would be unsound: the stamp counts
    /// millis since a process-local epoch that the FIRST `now_ms()` call starts,
    /// so `now_ms()` can legitimately return `0` and there would be nothing to
    /// subtract — and which test makes that first call is up to the harness.
    /// Waiting the window out is the only way to say "idle" that does not depend
    /// on test ordering.
    fn stamp_idle_past(idle_after: Duration) -> Arc<AtomicU64> {
        let stamped = now_ms();
        thread::sleep(idle_after * 3);
        Arc::new(AtomicU64::new(stamped))
    }

    /// Drain and count the `SessionsChanged` signals queued on `rx`, asserting
    /// the watcher only ever emits that variant.
    fn drain_changes(rx: &Receiver<AppEvent>) -> usize {
        let mut count = 0;
        while let Ok(ev) = rx.try_recv() {
            assert!(
                matches!(ev, AppEvent::SessionsChanged),
                "watcher must only emit SessionsChanged, got {ev:?}"
            );
            count += 1;
        }
        count
    }

    /// Task 3.3: a storm of rapid writes to a `.jsonl` under a temp store dir
    /// must collapse into exactly ONE debounced `SessionsChanged` (not
    /// one-per-write); a subsequent remove is likewise a single change.
    ///
    /// The write storm and the remove are measured in their own settled
    /// windows: on macOS the OS delivers create/modify vs. remove
    /// notifications far enough apart that they mature in separate debounce
    /// windows, which is real OS behavior, not a coalescing failure. Isolating
    /// each phase keeps the "N writes -> 1 event" assertion deterministic.
    #[test]
    fn rapid_writes_coalesce_to_single_sessions_changed() {
        let dir = unique_temp_dir("coalesce");
        let (tx, rx) = mpsc::channel::<AppEvent>();
        let activity = Arc::new(AtomicU64::new(UNTOUCHED));
        // Bind (not `_`) so the watcher/debouncer stays alive for the test.
        let _watcher =
            SessionWatcher::spawn(&dir, tx, Arc::clone(&activity)).expect("spawn watcher");

        // Let the recursive watch establish, pre-create the file at depth 2 so
        // the storm below is pure modifies to a session-shaped path, then
        // discard the settle/create events so we measure only the storm.
        let cwd = dir.join("encoded-cwd");
        fs::create_dir(&cwd).expect("create encoded-cwd dir");
        let file = cwd.join("sess-temp.jsonl");
        thread::sleep(Duration::from_millis(300));
        fs::write(&file, b"{\"seed\":true}\n").expect("seed jsonl");
        thread::sleep(Duration::from_millis(600));
        drain_changes(&rx);
        rearm(&activity);

        // Event storm: many rapid writes to the SAME path within one debounce
        // window. Generous slack (>> 200ms) so the debouncer has fired once.
        for i in 0..12 {
            fs::write(&file, format!("{{\"n\":{i}}}\n")).expect("write jsonl");
        }
        thread::sleep(Duration::from_millis(600));
        assert_eq!(
            drain_changes(&rx),
            1,
            "a 12-write storm must collapse to exactly ONE SessionsChanged, not one-per-write"
        );

        // A remove is a distinct change: exactly one more debounced signal.
        fs::remove_file(&file).expect("remove jsonl");
        thread::sleep(Duration::from_millis(600));
        assert_eq!(
            drain_changes(&rx),
            1,
            "the remove must emit exactly ONE debounced SessionsChanged"
        );

        // A session-affecting change is board ACTIVITY, so the stamp the agents
        // poller gates on must carry a real reading by now.
        assert_ne!(
            activity.load(Ordering::Relaxed),
            UNTOUCHED,
            "a session write must stamp the board-activity clock the agents \
             poller reads, or the poll stays skipped while the store churns"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Task 1.5a: modifying an existing irrelevant depth-2 `.txt` must NOT
    /// trigger a reload. The file is created after the watch establishes and
    /// its settle events are drained so the test measures only the in-place
    /// modify.
    ///
    /// End-to-end evidence, not the guarantee: it holds only while `notify`
    /// reports the FILE. See
    /// `classify_watch_path_pins_the_two_end_to_end_ignorable_shapes` for the
    /// backend-free statement of the same claim.
    #[test]
    fn irrelevant_txt_at_depth_two_emits_no_sessions_changed() {
        let dir = unique_temp_dir("txt");
        let (tx, rx) = mpsc::channel::<AppEvent>();
        let activity = Arc::new(AtomicU64::new(UNTOUCHED));
        let _watcher =
            SessionWatcher::spawn(&dir, tx, Arc::clone(&activity)).expect("spawn watcher");

        thread::sleep(Duration::from_millis(300));
        let cwd = dir.join("encoded-cwd");
        fs::create_dir(&cwd).expect("create encoded-cwd dir");
        let txt = cwd.join("notes.txt");
        fs::write(&txt, b"initial\n").expect("seed txt");
        thread::sleep(Duration::from_millis(600));
        drain_changes(&rx);
        rearm(&activity);

        fs::write(&txt, b"irrelevant\n").expect("modify txt");
        thread::sleep(Duration::from_millis(600));
        assert_eq!(
            drain_changes(&rx),
            0,
            "an irrelevant depth-2 .txt must not trigger SessionsChanged"
        );

        // A filtered-out write is not board ACTIVITY either. Stamping it would
        // hold the agents poll — and its `claude` spawn — awake on a board
        // nobody is using, which is the drain AGENTS_IDLE_AFTER exists to stop.
        assert_eq!(
            activity.load(Ordering::Relaxed),
            UNTOUCHED,
            "an ignorable path must leave the board-activity clock alone"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Task 1.5b: modifying an existing `.jsonl` at depth 3 (inside
    /// subagents) must NOT trigger a reload.
    ///
    /// End-to-end evidence, not the guarantee — same caveat and same
    /// backend-free counterpart as the `.txt` test above.
    #[test]
    fn depth_three_subagent_jsonl_emits_no_sessions_changed() {
        let dir = unique_temp_dir("subagent");
        let (tx, rx) = mpsc::channel::<AppEvent>();
        let activity = Arc::new(AtomicU64::new(UNTOUCHED));
        let _watcher =
            SessionWatcher::spawn(&dir, tx, Arc::clone(&activity)).expect("spawn watcher");

        thread::sleep(Duration::from_millis(300));
        let subagent_dir = dir.join("encoded-cwd").join("sess").join("subagents");
        fs::create_dir_all(&subagent_dir).expect("create subagent dir");
        let agent = subagent_dir.join("agent-1.jsonl");
        fs::write(&agent, b"{}\n").expect("seed subagent jsonl");
        thread::sleep(Duration::from_millis(600));
        drain_changes(&rx);
        rearm(&activity);

        fs::write(&agent, b"{\"x\":1}\n").expect("modify subagent jsonl");
        thread::sleep(Duration::from_millis(600));
        assert_eq!(
            drain_changes(&rx),
            0,
            "a depth-3 subagent .jsonl must not trigger SessionsChanged"
        );

        // Nor is a subagent's own churn board activity — background agents write
        // constantly, and that must not be what keeps the poll awake.
        assert_eq!(
            activity.load(Ordering::Relaxed),
            UNTOUCHED,
            "a subagent path below the session level must leave the \
             board-activity clock alone"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Task 1.5c: modifying an existing real depth-2 `.jsonl` still emits
    /// exactly one `SessionsChanged`.
    #[test]
    fn depth_two_jsonl_emits_one_sessions_changed() {
        let dir = unique_temp_dir("session");
        let (tx, rx) = mpsc::channel::<AppEvent>();
        let activity = Arc::new(AtomicU64::new(UNTOUCHED));
        let _watcher =
            SessionWatcher::spawn(&dir, tx, Arc::clone(&activity)).expect("spawn watcher");

        thread::sleep(Duration::from_millis(300));
        let cwd = dir.join("encoded-cwd");
        fs::create_dir(&cwd).expect("create encoded-cwd dir");
        let sess = cwd.join("sess.jsonl");
        fs::write(&sess, b"{}\n").expect("seed session jsonl");
        thread::sleep(Duration::from_millis(600));
        drain_changes(&rx);
        rearm(&activity);

        fs::write(&sess, b"{\"x\":1}\n").expect("modify session jsonl");
        thread::sleep(Duration::from_millis(600));
        assert_eq!(
            drain_changes(&rx),
            1,
            "a depth-2 .jsonl must emit exactly one SessionsChanged"
        );

        // The other half of the same write: a session-level change IS board
        // activity, so it must stamp the clock the agents poller gates on.
        assert_ne!(
            activity.load(Ordering::Relaxed),
            UNTOUCHED,
            "a depth-2 .jsonl write must stamp the board-activity clock, or the \
             agents poll stays skipped while the store is churning"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Task 3.2: the merged loop delivers BOTH the periodic tick and the
    /// watcher's change signal onto a single receiver.
    #[test]
    fn event_loop_merges_tick_and_watcher() {
        let dir = unique_temp_dir("merge");
        let events = EventLoop::new(&dir, Duration::from_millis(40)).expect("event loop");

        // The tick source feeds the shared receiver.
        assert!(
            wait_for(&events, Duration::from_secs(2), |e| matches!(
                e,
                AppEvent::Tick
            )),
            "tick source was not merged into the receiver"
        );

        // The watcher source feeds the SAME receiver (ticks interleave, so we
        // drain until the change signal arrives). Use a depth-2 session path so
        // the new filter lets it through.
        let cwd = dir.join("encoded-cwd");
        fs::create_dir(&cwd).expect("create encoded-cwd dir");
        fs::write(cwd.join("new-session.jsonl"), b"{}\n").expect("write jsonl");
        assert!(
            wait_for(&events, Duration::from_secs(3), |e| matches!(
                e,
                AppEvent::SessionsChanged
            )),
            "watcher source was not merged into the receiver"
        );

        drop(events);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Join-on-drop shutdown: constructing an `EventLoop` and dropping it must
    /// return promptly. This exercises `Drop for EventLoop`, which sets the
    /// shutdown flag and *joins* the input thread — the reader must observe the
    /// flag on its poll cadence (or error out without a controlling terminal,
    /// as in CI) and exit rather than block forever on `event::read`. A hang
    /// here would be the zombie-reader / thread-leak regression: the guarantee
    /// is that the reader is gone before `run` returns and `claude` is spawned
    /// onto the same stdin.
    #[test]
    fn event_loop_drop_joins_input_thread_promptly() {
        let dir = unique_temp_dir("drop");
        let events = EventLoop::new(&dir, Duration::from_millis(40)).expect("event loop");

        let start = Instant::now();
        drop(events);
        let elapsed = start.elapsed();

        // Bound is generous vs. the 50ms poll interval so it is not flaky, yet
        // far below any true "blocked on stdin forever" hang.
        assert!(
            elapsed < Duration::from_secs(2),
            "dropping an EventLoop must join its input thread promptly, took {elapsed:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Wiring-seam regression for the BLOCKER this remediation plan fixes
    /// (Finding 1/3): before this fix, every test exercised only the pure
    /// `agents_poll_due` gate in isolation, so the sentinel collision at the
    /// REAL `EventLoop::new` -> `spawn_agents_thread` seam went uncaught.
    /// This reads the private `activity` field `EventLoop::new` seeds at
    /// construction and feeds it straight to `agents_poll_due`, the same way
    /// `spawn_agents_thread`'s poll loop does — proving the seeded stamp is
    /// due for the first agents poll immediately, matching the doc comment
    /// above `EventLoop::new`. Deliberately does NOT start a poller, and the two
    /// spawns are declined for DIFFERENT reasons: `spawn_agents_poller` is the
    /// production wiring, which names the real `agents::reported_agents` shell-out
    /// with no seam to state — calling it here would spawn `claude`, violating the
    /// claude-free test rule. `spawn_agents_thread` DOES take a stated poll
    /// (`counting_poll`), but routing this claim through it would trade a
    /// deterministic question — is the seeded stamp due? — for a race against the
    /// poller's first loop turn, and the poller's obedience to that gate is
    /// already pinned by the three tests below.
    #[test]
    fn event_loop_seeds_activity_due_for_first_poll() {
        let dir = unique_temp_dir("seed-due");
        let events = EventLoop::new(&dir, Duration::from_millis(40)).expect("event loop");

        let seeded = events.activity.load(Ordering::Relaxed);
        assert!(
            agents_poll_due(seeded, now_ms(), AGENTS_IDLE_AFTER),
            "EventLoop::new's seeded activity stamp must be due for the \
             first agents poll immediately, not only after the first input \
             or SessionsChanged event"
        );

        drop(events);
        let _ = fs::remove_dir_all(&dir);
    }

    // --- spawn_input_thread: the board-activity stamp at the REAL seam -------

    /// The reader stamps the board-activity clock for every event it forwards —
    /// and for NOTHING else.
    ///
    /// This is the input half of `AGENTS_IDLE_AFTER`'s documented guarantee
    /// ("the first input OR `SessionsChanged` event after idle resumes polling");
    /// the watcher half is pinned by
    /// `depth_two_jsonl_emits_one_sessions_changed` and its two negative
    /// counterparts. Deleting the `activity.store` from the reader loop left the
    /// whole suite green, so a board being NAVIGATED while the store is quiescent
    /// would silently stop refreshing badges a minute in, with nothing to notice.
    ///
    /// Both halves are asserted because the negative one is what keeps the stamp
    /// honest: a timed-out 50 ms poll turn must NOT stamp, or the board could
    /// never go idle and `AGENTS_IDLE_AFTER` would be unreachable. The reader is
    /// driven through the stated `read` seam, so this needs no TTY.
    ///
    /// That seam also makes this the ONE place the reader's SHUTDOWN check is
    /// covered deterministically: without a controlling terminal, as in CI,
    /// production's reader ends on `Err(_)` long before the flag could matter,
    /// and every other reader test passes against a loop that ignores the flag
    /// outright. The teardown below therefore states that claim under a bound
    /// instead of leaving it to an unbounded `join()`.
    #[test]
    fn input_thread_stamps_board_activity_only_for_real_events() {
        let (tx, rx) = mpsc::channel::<AppEvent>();
        let activity = Arc::new(AtomicU64::new(UNTOUCHED));
        let shutdown = Arc::new(AtomicBool::new(false));
        let turns = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(AtomicBool::new(false));

        let handle = spawn_input_thread(
            tx,
            Arc::clone(&shutdown),
            Arc::clone(&activity),
            stated_input(&turns, &release),
        );

        // Empty turns first: the reader is looping and reading nothing.
        let polled_twice = || turns.load(Ordering::Relaxed) >= 2;
        assert!(
            wait_until(Duration::from_secs(2), polled_twice),
            "the reader must keep polling its source between shutdown checks"
        );
        assert_eq!(
            activity.load(Ordering::Relaxed),
            UNTOUCHED,
            "a timed-out poll turn is not board activity — stamping one would hold \
             the agents poll (and its `claude` spawn) awake on a board nobody is \
             touching, which is the drain AGENTS_IDLE_AFTER exists to stop"
        );

        // Now state ONE event and take it off the merged channel.
        release.store(true, Ordering::Relaxed);
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(AppEvent::Input(_)) => {}
            other => panic!("the reader must forward the stated event, got {other:?}"),
        }

        // The stamp is written BEFORE the send, and the channel's own
        // synchronization publishes it, so having RECEIVED the event is proof the
        // store already happened — this needs no sleep and cannot race.
        assert_ne!(
            activity.load(Ordering::Relaxed),
            UNTOUCHED,
            "a real input event must stamp the board-activity clock the agents \
             poller reads, or the first input after idle never resumes polling"
        );

        // The flag path is asserted under a BOUND, then joined — never joined
        // alone. This test is the only coverage the reader's shutdown check has:
        // with no TTY the production reader ends on `Err(_)` instead, so
        // `event_loop_drop_joins_input_thread_promptly` stays green against a
        // reader that ignores the flag entirely. An unbounded `join()` would turn
        // that regression into an indefinite HANG — a suite that never reports,
        // rather than one that reports red. So prove the thread is GONE the way
        // `agents_poller_exits_on_shutdown_even_while_idle` does, off the handle
        // the reader's own closure holds on `turns`.
        shutdown.store(true, Ordering::Relaxed);
        assert!(
            wait_until(Duration::from_secs(2), || Arc::strong_count(&turns) == 1),
            "the reader must observe the shutdown flag and exit within its poll \
             cadence — that exit is what lets `EventLoop`'s join-on-drop release \
             stdin before `claude` is spawned onto the same fd"
        );
        let _ = handle.join();
    }

    // --- spawn_agents_thread: the idle gate at the REAL seam -----------------
    //
    // The pure `agents_poll_due` tests below say what the gate DECIDES. These
    // say the poller OBEYS it — the two are different claims, and only the
    // second was missing: deleting the `agents_poll_due` call from the loop
    // left the whole suite green. Every one of these runs `claude`-free by
    // stating the poll (`counting_poll`) and the window (`idle_after`), both of
    // which are parameters of `spawn_agents_thread` for exactly this reason.

    /// An idle board must not poll AT ALL: no shell-out, and no
    /// `ReportedAgents` on the channel.
    #[test]
    fn agents_poller_skips_the_poll_while_the_board_is_idle() {
        let idle_after = Duration::from_millis(20);
        let (tx, rx) = mpsc::channel::<AppEvent>();
        let calls = Arc::new(AtomicUsize::new(0));
        let activity = stamp_idle_past(idle_after);
        let shutdown = Arc::new(AtomicBool::new(false));

        spawn_agents_thread(
            tx,
            Duration::from_millis(5),
            idle_after,
            activity,
            Arc::clone(&shutdown),
            counting_poll(&calls),
        );

        // Budget enough for many loop turns: an ungated poller sends on its
        // FIRST turn, before any sleep, so one turn would already be enough.
        match rx.recv_timeout(Duration::from_millis(400)) {
            Err(RecvTimeoutError::Timeout) => {}
            other => panic!("an idle board must deliver no ReportedAgents, got {other:?}"),
        }
        assert_eq!(
            calls.load(Ordering::Relaxed),
            0,
            "an idle board must not run the poll at all — in production that call \
             spawns `claude`, which is the whole cost AGENTS_IDLE_AFTER exists to avoid"
        );

        shutdown.store(true, Ordering::Relaxed);
    }

    /// The positive counterpart: a board that just saw activity polls
    /// IMMEDIATELY — before the first sleep — and delivers what the poll
    /// returned. `interval` is far longer than the budget on purpose, so a
    /// poller that slept first would fail this rather than pass it slowly.
    #[test]
    fn agents_poller_polls_immediately_while_the_board_is_active() {
        let (tx, rx) = mpsc::channel::<AppEvent>();
        let calls = Arc::new(AtomicUsize::new(0));
        let activity = Arc::new(AtomicU64::new(now_ms()));
        let shutdown = Arc::new(AtomicBool::new(false));

        spawn_agents_thread(
            tx,
            Duration::from_secs(30),
            AGENTS_IDLE_AFTER,
            activity,
            Arc::clone(&shutdown),
            counting_poll(&calls),
        );

        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(AppEvent::ReportedAgents(reported)) => assert!(
                reported.contains_key(POLLED_ID),
                "the delivered set must be the one the poll returned, got {reported:?}"
            ),
            other => panic!("an active board must deliver ReportedAgents, got {other:?}"),
        }
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "the first poll fires before the first sleep, exactly once"
        );

        shutdown.store(true, Ordering::Relaxed);
    }

    /// The poller notices its owner going away on EVERY loop turn, including
    /// turns the idle gate skipped.
    ///
    /// This is the invariant the idle gate nearly cost: the loop's older exit
    /// was a FAILED SEND, which an idle poller never attempts, so a board left
    /// idle across a resume round trip would have leaked one poller per trip.
    /// The receiver is deliberately held alive here, so the flag is provably
    /// what ended the thread and not a disconnect.
    #[test]
    fn agents_poller_exits_on_shutdown_even_while_idle() {
        let idle_after = Duration::from_millis(20);
        let interval = Duration::from_millis(5);
        let (tx, _rx) = mpsc::channel::<AppEvent>();
        let calls = Arc::new(AtomicUsize::new(0));
        let activity = stamp_idle_past(idle_after);
        let shutdown = Arc::new(AtomicBool::new(false));

        spawn_agents_thread(
            tx,
            interval,
            idle_after,
            activity,
            Arc::clone(&shutdown),
            counting_poll(&calls),
        );

        // Still running, still idle: it holds the poll closure (and so a handle
        // on `calls`) and has sent nothing that could have ended it.
        thread::sleep(interval * 8);
        assert_eq!(
            Arc::strong_count(&calls),
            2,
            "the poller must still be looping before the flag is set"
        );
        assert_eq!(
            calls.load(Ordering::Relaxed),
            0,
            "the board is idle, so no turn may have polled"
        );

        shutdown.store(true, Ordering::Relaxed);

        assert!(
            wait_until(Duration::from_secs(2), || Arc::strong_count(&calls) == 1),
            "the poller must exit within one interval of the shutdown flag being set, \
             even though an idle board never attempts the send that used to be its only exit"
        );
    }

    // --- classify_with_metadata matrix tests (pure, no filesystem) ---
    //
    // The levels are named, not counted: the depth arithmetic these used to
    // spell out as `0`/`1`/`2`/`3` now lives in `store::discover::depth_of`
    // alone (see the SUBAGENT EXCLUSION BY DEPTH rule). The matrix below is
    // unchanged — each case still pins exactly the class it always did.

    #[test]
    fn class_matrix_depth_two_jsonl_is_session() {
        assert_eq!(
            classify_with_metadata(StoreDepth::SessionLevel, true, false, false),
            WatchPathClass::Session
        );
    }

    #[test]
    fn class_matrix_depth_three_any_name_is_ignorable() {
        assert_eq!(
            classify_with_metadata(StoreDepth::BelowSession, false, true, false),
            WatchPathClass::Ignorable
        );
        assert_eq!(
            classify_with_metadata(StoreDepth::BelowSession, false, false, true),
            WatchPathClass::Ignorable
        );
        assert_eq!(
            classify_with_metadata(StoreDepth::BelowSession, false, false, false),
            WatchPathClass::Ignorable
        );
    }

    #[test]
    fn class_matrix_depth_two_non_jsonl_proven_is_ignorable() {
        assert_eq!(
            classify_with_metadata(StoreDepth::SessionLevel, false, true, false),
            WatchPathClass::Ignorable
        );
        assert_eq!(
            classify_with_metadata(StoreDepth::SessionLevel, false, false, true),
            WatchPathClass::Ignorable
        );
    }

    #[test]
    fn class_matrix_depth_two_non_jsonl_missing_meta_is_ambiguous() {
        assert_eq!(
            classify_with_metadata(StoreDepth::SessionLevel, false, false, false),
            WatchPathClass::Ambiguous
        );
    }

    #[test]
    fn class_matrix_depth_one_file_is_ignorable() {
        assert_eq!(
            classify_with_metadata(StoreDepth::CwdLevel, false, true, false),
            WatchPathClass::Ignorable
        );
    }

    #[test]
    fn class_matrix_depth_one_dir_or_missing_meta_is_ambiguous() {
        assert_eq!(
            classify_with_metadata(StoreDepth::CwdLevel, false, false, true),
            WatchPathClass::Ambiguous
        );
        assert_eq!(
            classify_with_metadata(StoreDepth::CwdLevel, false, false, false),
            WatchPathClass::Ambiguous
        );
    }

    #[test]
    fn class_matrix_depth_zero_is_ambiguous() {
        assert_eq!(
            classify_with_metadata(StoreDepth::Root, false, false, true),
            WatchPathClass::Ambiguous
        );
    }

    /// The arm `classify_watch_path`'s own early return used to carry: a path
    /// that is not under the root at all. Naming the level moved that case INTO
    /// the matrix, so it needs a case here — the store's shape rules nothing
    /// out for such a path, so it must never be dropped on a guess.
    #[test]
    fn class_matrix_outside_the_root_is_ambiguous() {
        assert_eq!(
            classify_with_metadata(StoreDepth::Outside, false, true, false),
            WatchPathClass::Ambiguous
        );
        assert_eq!(
            classify_with_metadata(StoreDepth::Outside, false, false, true),
            WatchPathClass::Ambiguous
        );
        assert_eq!(
            classify_with_metadata(StoreDepth::Outside, false, false, false),
            WatchPathClass::Ambiguous
        );
    }

    // --- classify_watch_path integration tests (with temp filesystem) ---

    #[test]
    fn classify_watch_path_session_file() {
        let dir = unique_temp_dir("class-session");
        let cwd = dir.join("project-cwd");
        fs::create_dir(&cwd).unwrap();
        let path = cwd.join("sess.jsonl");
        fs::write(&path, b"{}\n").unwrap();

        assert_eq!(classify_watch_path(&dir, &path), WatchPathClass::Session);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn classify_watch_path_subagent_depth_three_ignorable() {
        let dir = unique_temp_dir("class-subagent");
        let path = dir
            .join("project-cwd")
            .join("sess")
            .join("subagents")
            .join("agent-1.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{}\n").unwrap();

        assert_eq!(classify_watch_path(&dir, &path), WatchPathClass::Ignorable);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn classify_watch_path_depth_two_txt_ignorable() {
        let dir = unique_temp_dir("class-txt");
        let cwd = dir.join("project-cwd");
        fs::create_dir(&cwd).unwrap();
        let path = cwd.join("notes.txt");
        fs::write(&path, b"hello").unwrap();

        assert_eq!(classify_watch_path(&dir, &path), WatchPathClass::Ignorable);

        let _ = fs::remove_dir_all(&dir);
    }

    /// The DETERMINISTIC statement of what the two "no `SessionsChanged`"
    /// end-to-end tests above are really claiming.
    ///
    /// Those two stand up a real watcher and assert the batch produced NOTHING,
    /// which is a claim about `notify`'s reporting granularity as much as about
    /// snapback: a backend that coalesced at DIRECTORY granularity would put the
    /// parent in the batch instead of the file, and a parent is deliberately
    /// `Ambiguous` — a directory can hold sessions, and a real change is never
    /// dropped on a guess. The guarantee snapback actually owns is how each
    /// exact path classifies, and it lives here, with no backend in the loop.
    /// These are the same two path shapes, spelled the same way.
    #[test]
    fn classify_watch_path_pins_the_two_end_to_end_ignorable_shapes() {
        let dir = unique_temp_dir("class-e2e-shapes");
        let cwd = dir.join("encoded-cwd");

        // `irrelevant_txt_at_depth_two_emits_no_sessions_changed`'s subject.
        let txt = cwd.join("notes.txt");
        fs::create_dir_all(&cwd).unwrap();
        fs::write(&txt, b"irrelevant\n").unwrap();
        assert_eq!(classify_watch_path(&dir, &txt), WatchPathClass::Ignorable);

        // `depth_three_subagent_jsonl_emits_no_sessions_changed`'s subject.
        let sess = cwd.join("sess");
        let subagents = sess.join("subagents");
        let agent = subagents.join("agent-1.jsonl");
        fs::create_dir_all(&subagents).unwrap();
        fs::write(&agent, b"{\"x\":1}\n").unwrap();
        assert_eq!(classify_watch_path(&dir, &agent), WatchPathClass::Ignorable);

        // The coalescing counterparts, which is exactly why the end-to-end pair
        // is evidence rather than the guarantee. Both directories BELOW the
        // session level stay ignorable; the `<encoded-cwd>` directory does not,
        // and must not.
        assert_eq!(classify_watch_path(&dir, &sess), WatchPathClass::Ignorable);
        assert_eq!(
            classify_watch_path(&dir, &subagents),
            WatchPathClass::Ignorable
        );
        assert_eq!(
            classify_watch_path(&dir, &cwd),
            WatchPathClass::Ambiguous,
            "a directory that can hold sessions must reload, coalesced or not"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn classify_watch_path_depth_one_dir_ambiguous() {
        let dir = unique_temp_dir("class-cwd-dir");
        let path = dir.join("project-cwd");
        fs::create_dir(&path).unwrap();

        assert_eq!(classify_watch_path(&dir, &path), WatchPathClass::Ambiguous);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn classify_watch_path_removed_depth_two_ambiguous() {
        let dir = unique_temp_dir("class-removed");
        let cwd = dir.join("project-cwd");
        fs::create_dir(&cwd).unwrap();
        let path = cwd.join("notes.txt");
        fs::write(&path, b"hello").unwrap();
        fs::remove_file(&path).unwrap();

        assert_eq!(classify_watch_path(&dir, &path), WatchPathClass::Ambiguous);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn classify_watch_path_outside_root_ambiguous() {
        let dir = unique_temp_dir("class-outside");
        assert_eq!(
            classify_watch_path(&dir, Path::new("/tmp/something.jsonl")),
            WatchPathClass::Ambiguous
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // --- batch_needs_reload tests ---

    #[test]
    fn batch_needs_reload_empty_false() {
        assert!(!batch_needs_reload(std::iter::empty::<WatchPathClass>()));
    }

    #[test]
    fn batch_needs_reload_all_ignorable_false() {
        assert!(!batch_needs_reload([
            WatchPathClass::Ignorable,
            WatchPathClass::Ignorable,
        ]));
    }

    #[test]
    fn batch_needs_reload_single_ambiguous_true() {
        assert!(batch_needs_reload([WatchPathClass::Ambiguous]));
    }

    #[test]
    fn batch_needs_reload_single_session_true() {
        assert!(batch_needs_reload([WatchPathClass::Session]));
    }

    #[test]
    fn batch_needs_reload_mixed_session_and_ignorable_true() {
        assert!(batch_needs_reload([
            WatchPathClass::Ignorable,
            WatchPathClass::Session,
            WatchPathClass::Ignorable,
        ]));
    }

    // --- agents_poll_due tests (pure, no filesystem) ---

    #[test]
    fn agents_poll_due_fresh_activity_is_due() {
        let now = 100_000;
        let idle = Duration::from_secs(60);
        assert!(agents_poll_due(now, now, idle), "exactly now is active");
        assert!(agents_poll_due(now - 1, now, idle), "1 ms ago is active");
    }

    #[test]
    fn agents_poll_due_exactly_idle_after_is_not_due() {
        let now = 100_000;
        let idle = Duration::from_secs(60);
        let idle_ms = idle.as_millis() as u64;
        assert!(
            !agents_poll_due(now - idle_ms, now, idle),
            "idle for exactly idle_after is not due"
        );
    }

    #[test]
    fn agents_poll_due_past_idle_after_is_not_due() {
        let now = 100_000;
        let idle = Duration::from_secs(60);
        let idle_ms = idle.as_millis() as u64;
        assert!(
            !agents_poll_due(now - idle_ms - 1, now, idle),
            "idle past idle_after is not due"
        );
    }

    /// Regression for the BLOCKER this remediation plan fixes: `0` is no
    /// longer a "never active" sentinel, because it collides with a
    /// legitimate reading — `EPOCH`'s `LazyLock` is first initialized inside
    /// the very `now_ms()` call `EventLoop::new` uses to seed `activity`, so
    /// that seed is `0` on a freshly launched board. The pair
    /// `(last_activity_ms = 0, now_ms = 0)` must be treated as fresh activity
    /// (due), exactly like any other `last_activity_ms == now_ms` pair,
    /// so the very first `claude agents` poll fires immediately instead of
    /// being silently swallowed.
    #[test]
    fn agents_poll_due_zero_seed_at_zero_now_is_due() {
        let idle = Duration::from_secs(60);
        assert!(
            agents_poll_due(0, 0, idle),
            "a last_activity_ms of 0 paired with a now_ms of 0 is an ordinary \
             fresh reading (EPOCH's first now_ms() call), not a sentinel"
        );
    }
}
