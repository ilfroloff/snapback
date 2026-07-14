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
//!   rapid writes collapses to one reload signal.
//! * [`EventLoop`] — merges three sources into ONE receiver: a crossterm input
//!   thread, the watcher channel, and a periodic tick.
//!
//! Later phases consume these: `search` and the `tui/*` modules match on
//! [`AppEvent`] and drive the loop; they import `crate::watch::AppEvent`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event as CrosstermEvent};
use notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{new_debouncer, DebounceEventResult, DebouncedEventKind, Debouncer};

use crate::agents::{self, LiveAgent};

/// Debounce window for coalescing filesystem event storms into one reload.
///
/// The debouncer waits this long after the last raw event before emitting, so
/// a burst of rapid writes is delivered as a single batch.
pub const DEBOUNCE: Duration = Duration::from_millis(200);

/// Default cadence of the periodic [`AppEvent::Tick`] in the merged loop.
pub const TICK: Duration = Duration::from_millis(250);

/// Cadence of the OFF-THREAD `claude agents --json` poll that refreshes the
/// live-session badges.
///
/// Deliberately coarser than [`TICK`]: it spawns a child process, so it runs on
/// its own thread at a relaxed interval rather than on the render cadence, and
/// never blocks drawing (see [`EventLoop::spawn_agents_poller`]).
pub const AGENTS_REFRESH: Duration = Duration::from_millis(1000);

/// How long the input thread blocks in `crossterm::event::poll` before waking to
/// re-check its shutdown flag.
///
/// Kept small so [`EventLoop`]'s join-on-drop returns promptly — the resume
/// handoff must not stall noticeably while the reader is torn down before
/// `claude` is spawned onto the same stdin — yet large enough to avoid a busy
/// spin between polls.
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(50);

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
    /// A refreshed live-agent set from `claude agents --json`, computed OFF the
    /// UI thread by the agents poller and delivered on the same channel so the
    /// shell-out never blocks rendering.
    LiveAgents(HashMap<String, LiveAgent>),
    /// A periodic wake-up. The update loop does nothing costly on this.
    Tick,
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
    pub fn spawn(root: &Path, tx: Sender<AppEvent>) -> Result<Self> {
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
                // A send failure means the receiver (TUI) has gone away; ignore.
                let _ = tx.send(AppEvent::SessionsChanged);
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
    // Set to `true` on drop to tell the input thread to stop. The thread polls
    // this flag between reads (rather than blocking forever on stdin), so it can
    // observe the request and exit.
    input_shutdown: Arc<AtomicBool>,
    // The input thread's handle, joined on drop so the reader is fully gone
    // BEFORE this `EventLoop` (and thus `run`) returns and `claude` is spawned
    // onto the same fd 0. `Option` so `Drop` can `take()` it out to `join()`.
    input_handle: Option<JoinHandle<()>>,
}

impl EventLoop {
    /// Wire the input thread, filesystem watcher, and tick thread onto one
    /// receiver, watching `root` and ticking every `tick`.
    pub fn new(root: &Path, tick: Duration) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<AppEvent>();
        let watcher = SessionWatcher::spawn(root, tx.clone())?;
        let input_shutdown = Arc::new(AtomicBool::new(false));
        let input_handle = spawn_input_thread(tx.clone(), Arc::clone(&input_shutdown));
        spawn_tick_thread(tx.clone(), tick);
        Ok(Self {
            rx,
            tx,
            _watcher: watcher,
            input_shutdown,
            input_handle: Some(input_handle),
        })
    }

    /// Start the OFF-THREAD live-agent poller (real runtime path only).
    ///
    /// Spawns a dedicated thread that runs `claude agents --json` every
    /// `interval` and delivers each result as [`AppEvent::LiveAgents`] on the
    /// shared channel, so the shell-out can NEVER block rendering. It is not
    /// started by [`new`](Self::new) so the event-loop unit tests never spawn
    /// `claude`; [`crate::tui::run`] opts in once per board session. The thread
    /// exits when the receiver drops (see [`spawn_agents_thread`]), so it is
    /// bounded to the board session like the tick thread.
    pub fn spawn_agents_poller(&self, interval: Duration) {
        spawn_agents_thread(self.tx.clone(), interval);
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
    /// Shut the input reader down cleanly, then join it, before the loop goes
    /// away.
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
    fn drop(&mut self) {
        self.input_shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.input_handle.take() {
            let _ = handle.join();
        }
    }
}

/// Forward crossterm terminal events as [`AppEvent::Input`] on a poll loop that
/// wakes every [`INPUT_POLL_INTERVAL`] to observe `shutdown`, returning the
/// thread's [`JoinHandle`] so [`EventLoop`] can join it on drop.
///
/// Unlike a bare blocking `event::read()`, polling with a bounded timeout lets
/// the thread notice a shutdown request instead of parking forever on stdin.
/// The thread exits when any of the following occurs, so `join()` can never
/// hang (including in a non-TTY/test environment where the terminal source
/// errors):
///
/// * `shutdown` is observed set (clean teardown from [`EventLoop`]'s `Drop`);
/// * a send fails, meaning the TUI receiver has gone away;
/// * `poll` or `read` returns `Err` (no terminal / stdin closed).
fn spawn_input_thread(tx: Sender<AppEvent>, shutdown: Arc<AtomicBool>) -> JoinHandle<()> {
    thread::spawn(move || loop {
        if shutdown.load(Ordering::Relaxed) {
            break; // Clean shutdown requested by EventLoop::drop.
        }
        match event::poll(INPUT_POLL_INTERVAL) {
            // An event is ready: read and forward it.
            Ok(true) => match event::read() {
                Ok(ev) => {
                    if tx.send(AppEvent::Input(ev)).is_err() {
                        break; // TUI receiver gone.
                    }
                }
                Err(_) => break, // Read failed (no terminal / stdin closed).
            },
            // Timed out with no event: loop back to re-check the shutdown flag.
            Ok(false) => {}
            // Poll failed (no terminal): exit rather than busy-spin on errors.
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

/// Poll `claude agents --json` OFF-THREAD, emitting an [`AppEvent::LiveAgents`]
/// immediately and then every `interval`, until the receiver drops.
///
/// The first poll fires BEFORE any sleep so live badges appear on load; each
/// subsequent poll refreshes them on the autorefresh cadence. The shell-out is
/// fail-soft (see [`crate::agents::live_agents`]): a missing `claude` or bad
/// output just delivers an empty set. Exits when a send fails (the TUI receiver
/// has gone away), so — like the tick thread — it never accumulates across the
/// resume round trips that recreate the [`EventLoop`].
fn spawn_agents_thread(tx: Sender<AppEvent>, interval: Duration) {
    thread::spawn(move || loop {
        let live = agents::live_agents();
        if tx.send(AppEvent::LiveAgents(live)).is_err() {
            break; // TUI receiver gone.
        }
        thread::sleep(interval);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

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
        // Bind (not `_`) so the watcher/debouncer stays alive for the test.
        let _watcher = SessionWatcher::spawn(&dir, tx).expect("spawn watcher");

        // Let the recursive watch establish, pre-create the file so the storm
        // below is pure modifies, then discard the settle/create events so we
        // measure only the storm.
        let file = dir.join("sess-temp.jsonl");
        thread::sleep(Duration::from_millis(300));
        fs::write(&file, b"{\"seed\":true}\n").expect("seed jsonl");
        thread::sleep(Duration::from_millis(600));
        drain_changes(&rx);

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
        // drain until the change signal arrives).
        fs::write(dir.join("new-session.jsonl"), b"{}\n").expect("write jsonl");
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
    /// flag on its poll cadence (or error out on a non-TTY stdin) and exit
    /// rather than block forever on `event::read`. A hang here would be the
    /// zombie-reader / thread-leak regression: the guarantee is that the reader
    /// is gone before `run` returns and `claude` is spawned onto the same stdin.
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
}
