//! The subagents ONE session launched: the join from a launch's `tool_use.id` to
//! the subagent meta file claude writes for it.
//!
//! PARENT-SCOPED and DISPLAY-ONLY. Given one session file
//! `<encoded-cwd>/<id>.jsonl`, it lists that session's own
//! `<encoded-cwd>/<id>/subagents/` and reads only the
//! `agent-<agentId>.meta.json` files there. [`transcript_path`] names each one's
//! sibling `agent-<agentId>.jsonl`, which the preview's subagent view reads whole
//! once a click, `Ctrl-X ↓` or a `←`/`→` step switched the pane to it, and which
//! [`read_record`] stats, reads the head of and — for one that can be running —
//! the END of, for the selected session's [`SubagentRecord`], on a worker
//! thread. Nothing it reads feeds
//! discovery, the watcher, the content index, resume, fork, delete or move: those
//! never read under `<id>/subagents/` (AGENTS.md SUBAGENT EXCLUSION BY DEPTH),
//! and a subagent never becomes a board row. The watcher only NAMES a written
//! transcript by its path (`store::discover::subagent_transcript_at`, through
//! [`transcript_agent_id`]), so the subagent view showing it is drawn again.
//!
//! FAIL-SOFT (AGENTS.md FAIL-SOFT parsing): a meta file is read as a
//! `serde_json::Value`, never a typed struct. Every failure only leaves a launch
//! out of the index: a missing or unreadable dir, an unreadable, oversized or
//! malformed meta file, a missing `toolUseId` or `agentType`. Its direction is
//! toward the plain `[tool_use: …]` marker that launch renders without a meta
//! file, so a failure costs the delegation line (and the subagent's place in the
//! [`SubagentRecord`]), never the transcript around it. A transcript
//! [`open_transcript`] refuses costs only the subagent view switched to it, which
//! `store::preview` draws as a single line saying so. A transcript tail
//! [`read_record`] cannot read, or whose records do not parse, reads as a run
//! that has not ended, and a head with no timestamp as a launch time unknown.
//! Apart from the thin [`index_for`], [`open_transcript`] and [`read_record`]
//! wrappers everything here is pure, and nothing shells out (AGENTS.md PURE,
//! GIT-FREE STORE CORE).
//!
//! Layout, measured over 1811 meta files in a local store on 2026-10-11: one flat
//! `subagents/` dir per session holds every agent it launched at ANY depth (the
//! `spawnDepth` 2 and 3 ones included), each as `agent-<17 hex>.meta.json` beside
//! its `agent-<17 hex>.jsonl`. A forked skill adds
//! `agent-<id>.forked-skill.json` and `agent-<id>.forked-skill.marker.json`, which
//! are not meta files.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use super::skills::normalize_description;

/// The dir under a session's `<id>/` that holds its subagents' files.
const SUBAGENTS_DIR: &str = "subagents";

/// What every subagent file name opens with, ahead of the agent id.
const AGENT_FILE_PREFIX: &str = "agent-";

/// What closes a subagent META file's name, `agent-<id>.meta.json`.
const META_FILE_SUFFIX: &str = ".meta.json";

/// What closes a subagent TRANSCRIPT's name, `agent-<id>.jsonl`.
const TRANSCRIPT_FILE_SUFFIX: &str = ".jsonl";

/// The most bytes of one meta file the index reads. The largest of 1811 meta
/// files in a local store measured 264 bytes (2026-10-11), so the cap refuses
/// only a file that is not a meta, and bounds what a hostile one can make a
/// render read.
const META_MAX_BYTES: u64 = 64 * 1024;

/// The most bytes of a subagent transcript's END [`read_record`] reads to tell
/// whether its run ended — only for a transcript that can be running.
///
/// Measured 2026-10-12 with a probe outside the repo (Python, warm cache, the
/// last 64 KB of each transcript in a local store): every tail held a complete
/// turn, and one session's whole `subagents/` dir read in 5 ms at the median,
/// 31 ms at p90 and 435 ms at the maximum, which is why the read runs on a
/// worker thread and never on the keystroke, and opens only the tails written
/// within [`RUNNING_WINDOW`]. A tail holding no complete record reads as a run
/// that has not ended.
const TAIL_MAX_BYTES: u64 = 64 * 1024;

/// How long an UNFINISHED subagent may go without a write — to its own
/// transcript or to any sub-subagent's below it — and still read as running
/// ([`is_running`]); past it, it reads quiet.
///
/// Measured 2026-10-12 over a local store: at 10 minutes, 2.9% of real runs,
/// counting sub-subagents' writes, show a longer silence while still running,
/// and read `quiet` for that long.
const RUNNING_WINDOW: Duration = Duration::from_secs(10 * 60);

/// The tool a subagent calls to deliver its report to the agent that launched
/// it. A `tool_use` of it ENDS the subagent's run ([`run_ended`]).
///
/// FINISH DETECTION ONLY: the delegation join never reads a tool name
/// (`store::preview::delegated_meta`). Undocumented and version-pinned: it exists
/// in Claude Code 2.1.29x (a local store, 2026-10-12, holds it from 2.1.272
/// through 2.1.296 — 1,712 of 1,801 ended runs went through it). The fallback,
/// for a transcript written without it, is the last turn being a final reply (89
/// of the 1,801).
const HANDBACK_TOOL: &str = "SubagentHandback";

/// One launched subagent, as its meta file describes it: the fields a
/// delegation line draws, and the parent the running check reads.
///
/// The file carries more (`spawnDepth`, `requestShape`, `requestNonInteractive`,
/// `model`, `isFork`, `stoppedByUser`, `name`) and none of it is kept. A nested
/// delegation is DRAWN by walking the transcript that launched it; only
/// [`read_record`] reads the graph `parentAgentId` describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentMeta {
    /// The id in the file name, `agent-<agent_id>.meta.json`: the subagent a
    /// delegation line's click switches the preview to, the same id a hand-back
    /// names as `origin.from`, and the stem of its transcript ([`transcript_path`]).
    pub agent_id: String,
    /// `agentType`: the handle a delegation line and the subagent view name.
    pub agent_type: String,
    /// `description`: their annotation. `None` when absent, blank or not a
    /// string.
    pub description: Option<String>,
    /// `parentAgentId`: the subagent that launched this one, so its writes count
    /// for that parent's `running` ([`read_record`]). `None` for a launch of the
    /// session's own, and when the key is absent, blank or not a string: never an
    /// error, only a write that counts for no parent.
    pub parent_agent_id: Option<String>,
}

/// The `(toolUseId, meta)` one meta file declares, or `None` when it declares no
/// launch a delegation line can draw.
///
/// `file_name` is the file's own name, never a path, and must be exactly
/// `agent-<id>.meta.json` with a non-empty `<id>`, so a forked skill's
/// `agent-<id>.forked-skill.json` and `.forked-skill.marker.json` never match.
/// `raw` must be a JSON object with a non-blank string `toolUseId` (nothing to
/// join without it) and a non-blank string `agentType` (without it the launch
/// keeps its plain marker). `description` and `parentAgentId` are optional, and
/// unknown keys are ignored. `toolUseId` and `parentAgentId` are kept as
/// written, because each must equal the id it is matched against. Both drawn
/// fields pass [`normalize_description`], so no control character or raw escape
/// reaches a cell. PURE.
pub fn parse_meta(file_name: &str, raw: &str) -> Option<(String, SubagentMeta)> {
    let agent_id = meta_agent_id(file_name)?;
    let value: Value = serde_json::from_str(raw).ok()?;
    let object = value.as_object()?;
    let id = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
    };
    let tool_use_id = id("toolUseId")?;
    let drawn = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .and_then(normalize_description)
    };
    let meta = SubagentMeta {
        agent_id: agent_id.to_owned(),
        agent_type: drawn("agentType")?,
        description: drawn("description"),
        parent_agent_id: id("parentAgentId").map(str::to_owned),
    };
    Some((tool_use_id.to_owned(), meta))
}

/// The agent id in a meta file's name, or `None` when `file_name` is not
/// exactly `agent-<id>.meta.json` with a non-empty `<id>`. An id holding a path
/// separator is refused too, so [`transcript_path`] can never leave its dir.
fn meta_agent_id(file_name: &str) -> Option<&str> {
    agent_id_between(file_name, META_FILE_SUFFIX)
}

/// The agent id in a subagent TRANSCRIPT's name — the inverse of the name
/// [`transcript_path`] builds — or `None` when `file_name` is not exactly
/// `agent-<id>.jsonl` with a non-empty, separator-free `<id>`. A meta file and a
/// forked skill's `.json` files never match. PURE.
///
/// The watcher's shape check (`store::discover::subagent_transcript_at`) reads
/// a written file's name through this, so the file it names a subagent's
/// transcript is, by construction, the one the subagent view opens.
pub fn transcript_agent_id(file_name: &str) -> Option<&str> {
    agent_id_between(file_name, TRANSCRIPT_FILE_SUFFIX)
}

/// The `<id>` of `agent-<id><suffix>`, when it is non-empty and holds no path
/// separator.
fn agent_id_between<'a>(file_name: &'a str, suffix: &str) -> Option<&'a str> {
    let id = file_name
        .strip_prefix(AGENT_FILE_PREFIX)?
        .strip_suffix(suffix)?;
    (!id.is_empty() && !id.contains(std::path::is_separator)).then_some(id)
}

/// `<encoded-cwd>/<id>/subagents` for the session file
/// `<encoded-cwd>/<id>.jsonl`: the file's OWN parent joined with its OWN stem,
/// the derivation `delete::remove` uses for the sibling dir, so it names this
/// session's dir and no other. `None` when the path has no parent or no stem.
/// PURE: the dir need not exist.
pub fn subagents_dir(session_file: &Path) -> Option<PathBuf> {
    let parent = session_file.parent()?;
    let stem = session_file.file_stem()?;
    Some(parent.join(stem).join(SUBAGENTS_DIR))
}

/// The transcript of the subagent `agent_id` in `dir`,
/// `<dir>/agent-<agent_id>.jsonl`. PURE.
pub fn transcript_path(dir: &Path, agent_id: &str) -> PathBuf {
    dir.join(format!(
        "{AGENT_FILE_PREFIX}{agent_id}{TRANSCRIPT_FILE_SUFFIX}"
    ))
}

/// Every launch of ONE session that a meta file describes, keyed by the
/// `toolUseId` of the `tool_use` block that started it.
#[derive(Debug, Default)]
pub struct SubagentIndex {
    by_tool_use_id: HashMap<String, SubagentMeta>,
    /// The session's subagents dir the index was listed from. `None` only when
    /// the session path had no dir to derive ([`subagents_dir`]).
    dir: Option<PathBuf>,
}

impl SubagentIndex {
    /// The subagent the launch `tool_use_id` started, when a meta file names it.
    pub fn lookup(&self, tool_use_id: &str) -> Option<&SubagentMeta> {
        self.by_tool_use_id.get(tool_use_id)
    }

    /// The dir the subagents' transcripts sit in, for [`transcript_path`].
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// Every subagent the index holds, in no particular order: what
    /// [`read_record`] lists.
    fn metas(&self) -> impl Iterator<Item = &SubagentMeta> {
        self.by_tool_use_id.values()
    }
}

/// The [`SubagentIndex`] of the session file `session_file`: lists its
/// [`subagents_dir`], SORTS the names so the result is deterministic, runs
/// [`parse_meta`] on each meta file, and keeps the FIRST meta on a duplicate
/// `toolUseId`. A transcript is never read here, however large.
///
/// The duplicate rule exists for determinism only: 0 duplicate `toolUseId`s
/// across 171 session dirs, measured 2026-10-11. An unreadable dir or file
/// yields a possibly empty index, never an error.
pub fn index_for(session_file: &Path) -> SubagentIndex {
    index_with(session_file, list_file_names, read_meta_file)
}

/// [`index_for`] over an injected lister and reader, so a test can record every
/// path the index touches.
fn index_with(
    session_file: &Path,
    list: impl FnOnce(&Path) -> Vec<String>,
    mut read: impl FnMut(&Path) -> Option<String>,
) -> SubagentIndex {
    let Some(dir) = subagents_dir(session_file) else {
        return SubagentIndex::default();
    };
    let mut names = list(&dir);
    names.sort_unstable();
    let mut by_tool_use_id = HashMap::new();
    for name in names {
        if meta_agent_id(&name).is_none() {
            continue;
        }
        let Some(raw) = read(&dir.join(&name)) else {
            continue;
        };
        if let Some((tool_use_id, meta)) = parse_meta(&name, &raw) {
            by_tool_use_id.entry(tool_use_id).or_insert(meta);
        }
    }
    SubagentIndex {
        by_tool_use_id,
        dir: Some(dir),
    }
}

/// The entry names in `dir`, in no particular order. An unreadable dir has
/// none, and a name that is not UTF-8 is skipped: no meta file's name is one.
fn list_file_names(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect()
}

/// The subagent transcript at `path` ([`transcript_path`]), opened for a render,
/// or `None` unless it is a REGULAR file: not a symlink (per `symlink_metadata`),
/// and never a FIFO, whose open would block the render on the UI thread. Only the
/// subagent view — a click, `Ctrl-X ↓` or a `←`/`→` step switched to it — calls it
/// (`store::preview::render_subagent`), and nothing bounds the read after it: the
/// view draws the whole transcript, as the session's own render does.
pub fn open_transcript(path: &Path) -> Option<File> {
    if !fs::symlink_metadata(path).ok()?.file_type().is_file() {
        return None;
    }
    File::open(path).ok()
}

/// The text of the meta file at `path`, or `None` unless it is a REGULAR file
/// (not a symlink, per `symlink_metadata`, and never a FIFO a read would block
/// on) of at most [`META_MAX_BYTES`] of UTF-8. The read is bounded too, so a file
/// that grows after the check is still never read whole.
fn read_meta_file(path: &Path) -> Option<String> {
    if !fs::symlink_metadata(path).ok()?.file_type().is_file() {
        return None;
    }
    let mut text = String::new();
    let read = File::open(path)
        .ok()?
        .take(META_MAX_BYTES + 1)
        .read_to_string(&mut text)
        .ok()?;
    u64::try_from(read)
        .is_ok_and(|read| read <= META_MAX_BYTES)
        .then_some(text)
}

// --- the per-session record: the selected session's subagents ----------------

/// The most bytes of a subagent transcript's HEAD [`read_record`] reads for its
/// launch time ([`read_launch_time`]).
///
/// Measured 2026-10-13 over 2,049 subagent transcripts in a local store: the
/// first record ran 6,185 bytes at the median, 17,828 at p99 and 28,646 at the
/// most, so the cap holds every one with room to spare and bounds what a hostile
/// file can make the worker read. The read stops at the first record carrying a
/// timestamp, so it costs a buffered chunk or two in practice.
const HEAD_MAX_BYTES: u64 = 64 * 1024;

/// One subagent of ONE session as the background refresh last read it
/// ([`read_record`]): what its surfaces draw, and the facts the running check
/// ([`is_running`]) and the launch order ([`launch_order`]) are worked out from.
///
/// It answers "is the subagent's own run still going?" from its own transcript,
/// never "did the agent that launched it get the result?": a run can end and its
/// hand-back still be lost, and nothing here checks one against the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subagent {
    /// Its id: the transcript the subagent view switches to.
    pub agent_id: String,
    /// Its `agentType` ([`SubagentMeta::agent_type`]).
    pub agent_type: String,
    /// Its `description` ([`SubagentMeta::description`]).
    pub description: Option<String>,
    /// When it launched: the `timestamp` of the first record in its transcript
    /// that carries one ([`read_launch_time`]). `None` when the transcript is
    /// missing or not a regular file, or its first [`HEAD_MAX_BYTES`] hold no such
    /// record: a garbled line, no `timestamp`, or one that is not RFC 3339.
    pub launched: Option<OffsetDateTime>,
    /// The newest write to its own transcript or to any sub-subagent's below it,
    /// found through `parentAgentId` transitively ([`latest_write`]): the instant
    /// [`is_running`] measures from, and the one an age is stated from. `None`
    /// when none of them has an mtime.
    pub newest_write: Option<SystemTime>,
    /// Whether its own run ENDED, from its tail ([`run_ended`]). `Some` when the
    /// read parsed that tail — a tail it could not read, or no regular
    /// transcript, is `Some(false)`, a run that has not ended — and `None` when
    /// it did not open it at all: nothing at or below the subagent was written
    /// within [`RUNNING_WINDOW`] of the read, so it cannot be running. The record
    /// never claims a run finished from a tail it did not parse.
    pub ended: Option<bool>,
}

/// Every subagent ONE session launched, at any depth, in LAUNCH ORDER
/// ([`launch_order`]): the one record the subagent view's title, its `←`/`→`
/// steps and the preview's running list read, and the `Ctrl-X ↓` target is
/// picked from.
///
/// Read off the UI thread ([`read_record`], on the background refresh's worker),
/// and held by the board for the SELECTED session only. IN MEMORY ONLY.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubagentRecord {
    /// In [`launch_order`], an invariant [`new`](Self::new) keeps.
    subagents: Vec<Subagent>,
}

impl SubagentRecord {
    /// The record of `subagents`, put in [`launch_order`]. PURE.
    #[must_use]
    pub fn new(mut subagents: Vec<Subagent>) -> Self {
        subagents.sort_by(launch_order);
        Self { subagents }
    }

    /// Every subagent, in [`launch_order`]: what the preview's running list
    /// filters by [`is_running`]. PURE.
    #[must_use]
    pub fn subagents(&self) -> &[Subagent] {
        &self.subagents
    }

    /// Where `agent_id` stands in launch order, as the subagent view's title
    /// states it: its 1-based position and the count (`2/5`). `None` when the
    /// record does not list it. PURE.
    #[must_use]
    pub fn position(&self, agent_id: &str) -> Option<(usize, usize)> {
        let index = self.index_of(agent_id)?;
        Some((index + 1, self.subagents.len()))
    }

    /// The subagent `agent_id`, whose facts the subagent view's title states, or
    /// `None` when the record does not list it. PURE.
    #[must_use]
    pub fn get(&self, agent_id: &str) -> Option<&Subagent> {
        self.subagents.get(self.index_of(agent_id)?)
    }

    /// The subagent one step `forward` (`→`) or back (`←`) from `agent_id` in
    /// launch order, wrapping round at both ends. `None` only for a record with
    /// no subagents; with one, it is that one.
    ///
    /// An `agent_id` the record does not list — a launch whose meta file landed
    /// after this read, opened by a click on its `◆ delegated to …` line — counts
    /// as the NEWEST launch, after every listed one: `→` wraps round to the first
    /// subagent and `←` lands on the last. PURE.
    #[must_use]
    pub fn step(&self, agent_id: &str, forward: bool) -> Option<&Subagent> {
        let count = self.subagents.len();
        if count == 0 {
            return None;
        }
        let next = match (self.index_of(agent_id), forward) {
            (Some(index), true) => (index + 1) % count,
            (Some(index), false) => (index + count - 1) % count,
            (None, true) => 0,
            (None, false) => count - 1,
        };
        self.subagents.get(next)
    }

    /// The subagent `Ctrl-X ↓` opens, as of `now`: the FIRST one RUNNING
    /// ([`is_running`]) in launch order; else the most recently written one — the
    /// newest [`newest_write`](Subagent::newest_write), one with none only when
    /// none has one — a tie going to the LATER launch, the one whose own write it
    /// most likely was (a parent shares its running child's write). `None` with
    /// no subagents. PURE.
    #[must_use]
    pub fn open_target(&self, now: SystemTime) -> Option<&Subagent> {
        self.subagents
            .iter()
            .find(|subagent| is_running(subagent.ended, subagent.newest_write, now))
            .or_else(|| {
                self.subagents
                    .iter()
                    .enumerate()
                    .max_by_key(|&(index, subagent)| (subagent.newest_write, index))
                    .map(|(_, subagent)| subagent)
            })
    }

    /// `agent_id`'s index in launch order.
    fn index_of(&self, agent_id: &str) -> Option<usize> {
        self.subagents
            .iter()
            .position(|subagent| subagent.agent_id == agent_id)
    }
}

/// The LAUNCH ORDER `←`/`→` step through: the first-record timestamp
/// ([`Subagent::launched`]) ascending; a subagent with none after every one with
/// one, since no launch time can be placed among them; ties — and every
/// timestamp-less subagent among themselves — by agent id, so the order holds
/// from read to read. PURE.
#[must_use]
pub fn launch_order(a: &Subagent, b: &Subagent) -> Ordering {
    a.launched
        .is_none()
        .cmp(&b.launched.is_none())
        .then_with(|| a.launched.cmp(&b.launched))
        .then_with(|| a.agent_id.cmp(&b.agent_id))
}

/// Whether a run reads as RUNNING at `now`: its tail was parsed and it had not
/// ended (`ended`, [`Subagent::ended`]), and it or a sub-subagent below it wrote
/// (`newest_write`) within [`RUNNING_WINDOW`] of `now`, the window inclusive.
/// A write AFTER `now` (the clocks disagree) is as recent as a write can be.
///
/// Worked out at DRAW time from facts the read stored and a `now` the caller
/// holds, so a subagent turns quiet when its window passes with no file read.
/// PURE.
#[must_use]
pub fn is_running(ended: Option<bool>, newest_write: Option<SystemTime>, now: SystemTime) -> bool {
    ended == Some(false) && within_window(newest_write, now)
}

/// Where a subagent's own run stands at a `now`, in the words the subagent
/// view's title states it in (`tui::view`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentStatus {
    /// [`is_running`].
    Running,
    /// Its end was read: `ended` is `Some(true)`.
    Finished,
    /// Neither: an unfinished run silent past [`RUNNING_WINDOW`], or one whose end
    /// was never read (`ended` `None`), which cannot be running either.
    Quiet,
}

impl SubagentStatus {
    /// The status of a run with the stored facts `ended` and `newest_write` at
    /// `now`, over the SAME [`is_running`] the running list and `Ctrl-X ↓` ask,
    /// so the title can never call running what the list leaves off. Worked out
    /// at draw time, like [`is_running`]. PURE.
    #[must_use]
    pub fn of(ended: Option<bool>, newest_write: Option<SystemTime>, now: SystemTime) -> Self {
        if is_running(ended, newest_write, now) {
            Self::Running
        } else if ended == Some(true) {
            Self::Finished
        } else {
            Self::Quiet
        }
    }
}

/// Whether `written` lies within [`RUNNING_WINDOW`] of `now`, or after it. PURE.
fn within_window(written: Option<SystemTime>, now: SystemTime) -> bool {
    written.is_some_and(|at| {
        now.duration_since(at)
            .map_or(true, |idle| idle <= RUNNING_WINDOW)
    })
}

/// The `message.stop_reason` values that make an assistant text record a FINAL
/// reply ([`after_record`]) in a run that has not handed back: claude ended the
/// turn there. Text after a hand-back needs none of them.
///
/// Measured 2026-10-12 over a local store: of the assistant text records later
/// joined by a `tool_use` of the same turn, 14,942 carried a null `stop_reason`
/// and 92 `tool_use`, never `end_turn`; of the real final replies, 1,117 carried
/// `end_turn`, 2 `stop_sequence` and 5 null. So a null or `tool_use` one keeps the
/// run open — a narrated tool call never reads as finished mid-turn — and those 5
/// read as unfinished unless a hand-back came before them.
const FINAL_STOP_REASONS: [&str; 2] = ["end_turn", "stop_sequence"];

/// Where a run stands after the records [`run_ended`] has walked so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum RunState {
    /// Still going, or no turn read yet.
    #[default]
    Open,
    /// Its last turn was a final reply: assistant text, no `tool_use`, and a
    /// [`FINAL_STOP_REASONS`] `stop_reason`.
    Replied,
    /// Its last turn called [`HANDBACK_TOOL`], or was text with no `tool_use`
    /// after that call.
    HandedBack,
}

/// Whether a subagent's own run has ENDED, judged from the END of its
/// transcript: `tail` holds its last bytes, and `cut` says they start mid-file,
/// so their first line may be a fragment and is skipped.
///
/// The run has ended when its LAST turn is either
///
/// * an assistant record holding a [`HANDBACK_TOOL`] `tool_use` — the
///   `tool_result` answering it is not a turn of its own — or a text reply
///   after it: an assistant record holding text and no `tool_use`, whatever its
///   `message.stop_reason` (claude 2.1.29x often writes one after the
///   hand-back's answer); or
/// * a FINAL REPLY: an assistant record holding text and no `tool_use`, whose
///   `message.stop_reason` is one of [`FINAL_STOP_REASONS`].
///
/// Any later turn reopens it: a pending `tool_use` (a sub-subagent's launch
/// among them), a `tool_result` answering one, a thinking-only record, a text
/// record whose `stop_reason` is null, missing or `tool_use` (a turn still being
/// written) save where the first case keeps the run ended, and any user text —
/// a new prompt, claude's `isMeta` nudge to hand back, or
/// `[Request interrupted by user]`. A record of any other
/// type (`attachment`, `system`, …) is not a turn, and a line that is not JSON —
/// the cut first line, a last one still being written — is skipped. PURE.
fn run_ended(tail: &[u8], cut: bool) -> bool {
    let whole = if cut {
        tail.iter()
            .position(|&byte| byte == b'\n')
            .map_or(&tail[..0], |end| &tail[end + 1..])
    } else {
        tail
    };
    whole
        .split(|&byte| byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .fold(RunProgress::default(), |run, record| run.after(&record))
        .ended()
}

/// A run judged one record at a time, in file order, by [`run_ended`]'s rule:
/// the ONE fold of [`after_record`], which [`run_ended`] runs over a tail and the
/// subagent view's render (`store::preview::render_subagent`) over the whole
/// transcript it streams, so the record and the view never judge the same
/// records two ways. Starts as a run with no turn read. PURE.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunProgress(RunState);

impl RunProgress {
    /// The run once `record` follows it.
    #[must_use]
    pub fn after(self, record: &Value) -> Self {
        Self(after_record(self.0, record))
    }

    /// Whether the run has ENDED after the records folded in so far.
    #[must_use]
    pub fn ended(self) -> bool {
        self.0 != RunState::Open
    }
}

/// Where a run stands once `record` follows a run standing at `state`. PURE.
fn after_record(state: RunState, record: &Value) -> RunState {
    let message = record.get("message");
    let content = message.and_then(|message| message.get("content"));
    let blocks = content
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let has = |kind: &str| blocks.iter().any(|block| block_type(block) == Some(kind));
    let final_stop = message
        .and_then(|message| message.get("stop_reason"))
        .and_then(Value::as_str)
        .is_some_and(|reason| FINAL_STOP_REASONS.contains(&reason));
    let text = has("text") || content.is_some_and(Value::is_string);
    match record.get("type").and_then(Value::as_str) {
        Some("assistant") if blocks.iter().any(is_handback) => RunState::HandedBack,
        Some("assistant") if has("tool_use") => RunState::Open,
        // Measured 2026-10-13 over a local store: 1,041 text replies followed a
        // `SubagentHandback`, their `stop_reason` `end_turn` 1,038 times and null
        // 3, and none was later joined by a `tool_use` of the same message. So
        // text after a hand-back keeps the run ended whatever its stop reason. A
        // few runs did go on after one (a new prompt, more tool calls), so a
        // hand-back is not ended for good: the other arms still reopen it.
        Some("assistant") if text && state == RunState::HandedBack => state,
        Some("assistant") if text && final_stop => RunState::Replied,
        Some("assistant") => RunState::Open,
        // The answer to a tool call — the hand-back's own included — is no turn.
        Some("user")
            if !blocks.is_empty()
                && blocks
                    .iter()
                    .all(|block| block_type(block) == Some("tool_result")) =>
        {
            if state == RunState::HandedBack {
                state
            } else {
                RunState::Open
            }
        }
        Some("user") => RunState::Open,
        _ => state,
    }
}

/// A content block's `type`.
fn block_type(block: &Value) -> Option<&str> {
    block.get("type").and_then(Value::as_str)
}

/// Whether a content block is a `tool_use` of [`HANDBACK_TOOL`].
fn is_handback(block: &Value) -> bool {
    block_type(block) == Some("tool_use")
        && block.get("name").and_then(Value::as_str) == Some(HANDBACK_TOOL)
}

/// The newest of `agent_id`'s own write and every write below it — its
/// sub-subagents', found through `children` (parent id -> the ids it launched),
/// transitively. A cycle in that graph is walked once. PURE.
fn latest_write(
    agent_id: &str,
    children: &HashMap<&str, Vec<&str>>,
    written: &HashMap<&str, SystemTime>,
) -> Option<SystemTime> {
    let mut seen = HashSet::new();
    let mut pending = vec![agent_id];
    let mut latest = None;
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        latest = latest.max(written.get(id).copied());
        pending.extend(children.get(id).into_iter().flatten().copied());
    }
    latest
}

/// The selected session's subagents: the background refresh's whole read, run on
/// its worker (`watch::spawn_subagent_refresh`), never on a keystroke or a frame.
///
/// It lists that session's OWN `subagents/` dir through [`index_for`]'s rules —
/// so a subagent whose meta file the index refuses is not listed — reading every
/// meta file, then STATS each listed transcript for its mtime and reads its head
/// for its launch time ([`read_launch_time`]). It opens the TAIL ([`run_ended`])
/// of a transcript only when it or a sub-subagent below it was written within
/// [`RUNNING_WINDOW`] of `now` — the running subagents and every parent above
/// one — since no other can be running; every other one is stat and head only,
/// its [`ended`](Subagent::ended) `None`. No other session's dir is read.
/// Fail-soft throughout: an unreadable dir lists nothing, a transcript that is
/// missing or not a regular file has no write and no launch time, and an
/// unreadable tail is a run that has not ended.
#[must_use]
pub fn read_record(session_file: &Path, now: SystemTime) -> SubagentRecord {
    read_record_with(
        session_file,
        now,
        list_file_names,
        read_meta_file,
        stat_transcript,
        read_launch_time,
        read_tail_ended,
    )
}

/// What a stat of one subagent transcript gives [`read_record`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TranscriptStat {
    /// Its mtime, when the platform has one to give.
    written: Option<SystemTime>,
    /// Its length then, which bounds the tail read.
    len: u64,
}

/// [`read_record`] over an injected lister, meta reader, stat, head reader and
/// tail reader, so a test can record every path the read touches — and which
/// transcripts it opens the tail of.
fn read_record_with(
    session_file: &Path,
    now: SystemTime,
    list: impl FnOnce(&Path) -> Vec<String>,
    read_meta: impl FnMut(&Path) -> Option<String>,
    mut stat: impl FnMut(&Path) -> Option<TranscriptStat>,
    mut launched_at: impl FnMut(&Path) -> Option<OffsetDateTime>,
    mut tail_ended: impl FnMut(&Path, u64) -> bool,
) -> SubagentRecord {
    let index = index_with(session_file, list, read_meta);
    let Some(dir) = index.dir() else {
        return SubagentRecord::default();
    };
    let metas: Vec<&SubagentMeta> = index.metas().collect();
    let mut stats: HashMap<&str, TranscriptStat> = HashMap::new();
    let mut launches: HashMap<&str, OffsetDateTime> = HashMap::new();
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    for meta in &metas {
        let id = meta.agent_id.as_str();
        let path = transcript_path(dir, id);
        if let Some(found) = stat(&path) {
            stats.insert(id, found);
            if let Some(at) = launched_at(&path) {
                launches.insert(id, at);
            }
        }
        if let Some(parent) = meta.parent_agent_id.as_deref() {
            children.entry(parent).or_default().push(id);
        }
    }
    let written: HashMap<&str, SystemTime> = stats
        .iter()
        .filter_map(|(&id, stat)| Some((id, stat.written?)))
        .collect();
    let subagents = metas
        .iter()
        .map(|meta| {
            let id = meta.agent_id.as_str();
            let newest_write = latest_write(id, &children, &written);
            let ended = within_window(newest_write, now).then(|| {
                stats
                    .get(id)
                    .is_some_and(|stat| tail_ended(&transcript_path(dir, id), stat.len))
            });
            Subagent {
                agent_id: meta.agent_id.clone(),
                agent_type: meta.agent_type.clone(),
                description: meta.description.clone(),
                launched: launches.get(id).copied(),
                newest_write,
                ended,
            }
        })
        .collect();
    SubagentRecord::new(subagents)
}

/// The stat of the transcript at `path`: its mtime and length, or `None` unless
/// it is a REGULAR file — never a symlink (per `symlink_metadata`) or a FIFO,
/// whose open would block the worker.
fn stat_transcript(path: &Path) -> Option<TranscriptStat> {
    let metadata = fs::symlink_metadata(path).ok()?;
    metadata.file_type().is_file().then(|| TranscriptStat {
        written: metadata.modified().ok(),
        len: metadata.len(),
    })
}

/// When the subagent whose transcript is at `path` launched: the
/// [`record_timestamp`] of the first line in its first [`HEAD_MAX_BYTES`] that
/// has one, read a line at a time so it stops there. A forked subagent's
/// transcript opens with a `fork-context-ref` record that carries no timestamp
/// (1 of 2,049 measured 2026-10-13), so a line without one is passed over rather
/// than ending the search. `None` when the file cannot be opened or no line in
/// the head has one; a line the cap cut short never parses.
fn read_launch_time(path: &Path) -> Option<OffsetDateTime> {
    let mut head = BufReader::new(File::open(path).ok()?.take(HEAD_MAX_BYTES));
    let mut line = Vec::new();
    loop {
        line.clear();
        if head.read_until(b'\n', &mut line).ok()? == 0 {
            return None;
        }
        if let Some(at) = record_timestamp(&line) {
            return Some(at);
        }
    }
}

/// The RFC 3339 `timestamp` one transcript line carries, or `None` when the line
/// is not a JSON object, has no string `timestamp`, or that string is not RFC
/// 3339. PURE.
fn record_timestamp(line: &[u8]) -> Option<OffsetDateTime> {
    let record: Value = serde_json::from_slice(line).ok()?;
    let raw = record.get("timestamp")?.as_str()?;
    OffsetDateTime::parse(raw, &Rfc3339).ok()
}

/// Whether the run in the transcript at `path`, `len` bytes long when it was
/// stat'ed, ENDED, from its last [`TAIL_MAX_BYTES`] ([`run_ended`]). A tail that
/// cannot be read is a run that has not ended.
fn read_tail_ended(path: &Path, len: u64) -> bool {
    read_tail(path, len).is_some_and(|(tail, cut)| run_ended(&tail, cut))
}

/// The last [`TAIL_MAX_BYTES`] of the file at `path`, which was `len` bytes long
/// when its metadata was read, and whether they start mid-file. The read is
/// bounded too, so a file that grows meanwhile is still never read whole.
fn read_tail(path: &Path, len: u64) -> Option<(Vec<u8>, bool)> {
    let start = len.saturating_sub(TAIL_MAX_BYTES);
    let mut file = File::open(path).ok()?;
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut tail = Vec::new();
    file.take(TAIL_MAX_BYTES).read_to_end(&mut tail).ok()?;
    Some((tail, start > 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashSet;

    use crate::store::discover;

    /// The committed fixture store root.
    fn fixtures_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("store")
    }

    /// The fixture folder and session that carry the delegation fixture.
    const FIXTURE_FOLDER: &str = "-Users-me-project-epsilon";
    const FIXTURE_SESSION: &str = "sess-delegation-1";

    fn fixture_session() -> PathBuf {
        fixtures_root()
            .join(FIXTURE_FOLDER)
            .join(format!("{FIXTURE_SESSION}.jsonl"))
    }

    /// An isolated temp dir; never the real `~/.claude/projects`.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "snapback-subagents-{tag}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    const META_NAME: &str = "agent-a1b2c3d4e5f6a7b8c.meta.json";
    const META_RAW: &str =
        r#"{"agentType":"lead","description":"Plan the work","toolUseId":"toolu_1"}"#;

    fn meta(agent_id: &str, agent_type: &str, description: Option<&str>) -> SubagentMeta {
        SubagentMeta {
            agent_id: agent_id.to_owned(),
            agent_type: agent_type.to_owned(),
            description: description.map(str::to_owned),
            parent_agent_id: None,
        }
    }

    // --- parse_meta --------------------------------------------------------

    #[test]
    fn a_meta_file_yields_its_tool_use_id_and_the_drawn_fields() {
        assert_eq!(
            parse_meta(META_NAME, META_RAW),
            Some((
                "toolu_1".to_owned(),
                meta("a1b2c3d4e5f6a7b8c", "lead", Some("Plan the work"))
            ))
        );
    }

    /// Only `agent-<id>.meta.json` with a non-empty, separator-free `<id>` is a
    /// meta file. The body is a valid meta every time, so the NAME alone refuses.
    #[test]
    fn a_name_that_is_not_exactly_a_meta_file_name_yields_nothing() {
        for name in [
            "a1b2c3d4e5f6a7b8c.meta.json",
            "agent-a1b2c3d4e5f6a7b8c.json",
            "agent-a1b2c3d4e5f6a7b8c.jsonl",
            "agent-a1b2c3d4e5f6a7b8c.meta.json.bak",
            "agent-.meta.json",
            "agent-../escape.meta.json",
            "agent-",
            "",
        ] {
            assert_eq!(
                parse_meta(name, META_RAW),
                None,
                "{name:?} is not a meta file"
            );
        }
    }

    /// A forked skill writes two more `agent-<id>.*.json` files beside the meta.
    /// They carry valid meta keys here, so only the name rule can keep them out.
    #[test]
    fn a_forked_skills_decoy_files_are_never_meta_files() {
        for name in [
            "agent-a1b2c3d4e5f6a7b8c.forked-skill.json",
            "agent-a1b2c3d4e5f6a7b8c.forked-skill.marker.json",
        ] {
            assert_eq!(parse_meta(name, META_RAW), None, "{name:?} is a decoy");
        }
    }

    #[test]
    fn a_body_that_is_not_a_json_object_yields_nothing() {
        for raw in ["[]", r#""lead""#, "42", "null", "true"] {
            assert_eq!(parse_meta(META_NAME, raw), None, "{raw:?} is not an object");
        }
    }

    /// Nothing to join without a `toolUseId`.
    #[test]
    fn a_missing_blank_or_non_string_tool_use_id_yields_nothing() {
        for raw in [
            r#"{"agentType":"lead"}"#,
            r#"{"agentType":"lead","toolUseId":""}"#,
            r#"{"agentType":"lead","toolUseId":"   "}"#,
            r#"{"agentType":"lead","toolUseId":7}"#,
            r#"{"agentType":"lead","toolUseId":null}"#,
        ] {
            assert_eq!(parse_meta(META_NAME, raw), None, "{raw}");
        }
    }

    /// Without an `agentType` the header has no handle, so the launch keeps its
    /// plain marker. A type made only of control characters is blank once
    /// cleaned.
    #[test]
    fn a_missing_blank_or_non_string_agent_type_yields_nothing() {
        for raw in [
            r#"{"toolUseId":"toolu_1"}"#,
            r#"{"agentType":"","toolUseId":"toolu_1"}"#,
            r#"{"agentType":" \t ","toolUseId":"toolu_1"}"#,
            r#"{"agentType":"\u001b","toolUseId":"toolu_1"}"#,
            r#"{"agentType":["lead"],"toolUseId":"toolu_1"}"#,
            r#"{"agentType":null,"toolUseId":"toolu_1"}"#,
        ] {
            assert_eq!(parse_meta(META_NAME, raw), None, "{raw}");
        }
    }

    #[test]
    fn a_missing_blank_or_non_string_description_reads_as_none() {
        for raw in [
            r#"{"agentType":"lead","toolUseId":"toolu_1"}"#,
            r#"{"agentType":"lead","toolUseId":"toolu_1","description":""}"#,
            r#"{"agentType":"lead","toolUseId":"toolu_1","description":"  "}"#,
            r#"{"agentType":"lead","toolUseId":"toolu_1","description":3}"#,
            r#"{"agentType":"lead","toolUseId":"toolu_1","description":null}"#,
        ] {
            assert_eq!(
                parse_meta(META_NAME, raw),
                Some((
                    "toolu_1".to_owned(),
                    meta("a1b2c3d4e5f6a7b8c", "lead", None)
                )),
                "{raw}"
            );
        }
    }

    /// Every key the real store carries beside the four read, plus one no
    /// store has, changes nothing.
    #[test]
    fn unknown_keys_are_ignored() {
        let raw = r#"{"agentType":"lead","description":"Plan the work","toolUseId":"toolu_1",
            "spawnDepth":2,"requestShape":"background",
            "requestNonInteractive":true,"model":"opus","isFork":false,"stoppedByUser":true,
            "name":"planner","futureKey":{"nested":[1,2]}}"#;
        assert_eq!(parse_meta(META_NAME, raw), parse_meta(META_NAME, META_RAW));
    }

    /// `parentAgentId` is kept as written, for the running check.
    #[test]
    fn a_parent_agent_id_is_kept_as_written() {
        let raw =
            r#"{"agentType":"lead","toolUseId":"toolu_1","parentAgentId":"b0c1d2e3f4a5b6c7d"}"#;
        let (_, parsed) = parse_meta(META_NAME, raw).expect("still a meta");
        assert_eq!(parsed.parent_agent_id.as_deref(), Some("b0c1d2e3f4a5b6c7d"));
    }

    /// FAIL-SOFT: an absent, blank or non-string `parentAgentId` is no parent,
    /// never a refused meta.
    #[test]
    fn a_missing_blank_or_non_string_parent_agent_id_reads_as_none() {
        for raw in [
            r#"{"agentType":"lead","toolUseId":"toolu_1"}"#,
            r#"{"agentType":"lead","toolUseId":"toolu_1","parentAgentId":""}"#,
            r#"{"agentType":"lead","toolUseId":"toolu_1","parentAgentId":"  "}"#,
            r#"{"agentType":"lead","toolUseId":"toolu_1","parentAgentId":7}"#,
            r#"{"agentType":"lead","toolUseId":"toolu_1","parentAgentId":null}"#,
            r#"{"agentType":"lead","toolUseId":"toolu_1","parentAgentId":["a"]}"#,
        ] {
            assert_eq!(
                parse_meta(META_NAME, raw),
                Some((
                    "toolu_1".to_owned(),
                    meta("a1b2c3d4e5f6a7b8c", "lead", None)
                )),
                "{raw}"
            );
        }
    }

    /// The drawn fields reach a cell, so a raw escape or newline in them must not.
    #[test]
    fn control_characters_never_reach_the_drawn_fields() {
        let raw = r#"{"agentType":"le\u001bad","description":"one\nline\u001b[31m red","toolUseId":"toolu_1"}"#;
        let (_, parsed) = parse_meta(META_NAME, raw).expect("still a meta");
        assert_eq!(parsed.agent_type, "lead");
        assert_eq!(parsed.description.as_deref(), Some("one line[31m red"));
    }

    #[test]
    fn a_garbage_body_yields_nothing_and_never_panics() {
        let deep = "[".repeat(10_000);
        for raw in ["", "{", "{not json", "\u{0}\u{1b}", "}{", deep.as_str()] {
            assert_eq!(parse_meta(META_NAME, raw), None, "{raw:?}");
        }
    }

    // --- paths -------------------------------------------------------------

    #[test]
    fn the_subagents_dir_is_the_sessions_own_sibling_dir() {
        assert_eq!(
            subagents_dir(Path::new("/store/-Users-me-p/sess-1.jsonl")),
            Some(PathBuf::from("/store/-Users-me-p/sess-1/subagents"))
        );
        assert_eq!(subagents_dir(Path::new("/")), None, "no parent, no stem");
    }

    #[test]
    fn a_transcript_path_is_the_agents_jsonl_in_the_dir() {
        assert_eq!(
            transcript_path(Path::new("/s/sess-1/subagents"), "a1b2c3d4e5f6a7b8c"),
            PathBuf::from("/s/sess-1/subagents/agent-a1b2c3d4e5f6a7b8c.jsonl")
        );
    }

    /// The name half of the watcher's shape check reads back exactly the id
    /// [`transcript_path`] wrote, and refuses every other file the dir holds.
    #[test]
    fn a_transcript_name_reads_back_its_agent_id_and_no_other_name_does() {
        let written = transcript_path(Path::new("/s/sess-1/subagents"), "a1b2c3d4e5f6a7b8c");
        let name = written.file_name().and_then(|n| n.to_str()).unwrap();
        assert_eq!(transcript_agent_id(name), Some("a1b2c3d4e5f6a7b8c"));
        for name in [
            "agent-a1b2c3d4e5f6a7b8c.meta.json",
            "agent-a1b2c3d4e5f6a7b8c.forked-skill.json",
            "agent-a1b2c3d4e5f6a7b8c.forked-skill.marker.json",
            "agent-a1b2c3d4e5f6a7b8c.jsonl.bak",
            "a1b2c3d4e5f6a7b8c.jsonl",
            "agent-.jsonl",
            "agent-../escape.jsonl",
            "",
        ] {
            assert_eq!(transcript_agent_id(name), None, "{name:?} is no transcript");
        }
    }

    // --- index_for ---------------------------------------------------------

    /// The fixture's three meta files join by `toolUseId`, the `Task`-named
    /// launch's included. The orphan launch has no meta file, and the two decoys
    /// carry valid meta keys (one claiming `toolu_delegation_1`, and sorting
    /// ahead of the real meta) yet add nothing.
    #[test]
    fn the_fixture_index_joins_every_meta_file_and_nothing_else() {
        let index = index_for(&fixture_session());
        assert_eq!(
            index.lookup("toolu_delegation_1"),
            Some(&meta(
                "a1b2c3d4e5f6a7b8c",
                "parser-auditor",
                Some("Audit the parser per [docs](https://example.com)")
            ))
        );
        assert_eq!(
            index.lookup("toolu_delegation_nested"),
            Some(&SubagentMeta {
                parent_agent_id: Some("a1b2c3d4e5f6a7b8c".to_owned()),
                ..meta(
                    "b0c1d2e3f4a5b6c7d",
                    "fixture-reviewer",
                    Some("Review malformed fixtures")
                )
            })
        );
        assert_eq!(
            index.lookup("toolu_delegation_task_named"),
            Some(&meta(
                "c2d3e4f5a6b7c8d9e",
                "docs-writer",
                Some("Refresh the parser docs")
            ))
        );
        assert_eq!(index.lookup("toolu_delegation_orphan"), None);
        assert_eq!(index.lookup("toolu_delegation_decoy"), None);
        assert_eq!(index.by_tool_use_id.len(), 3, "{:?}", index.by_tool_use_id);
    }

    /// Two meta files claiming one `toolUseId`: the first by NAME wins, whatever
    /// order the dir lists them in. The later name is written first on purpose.
    #[test]
    fn a_duplicate_tool_use_id_keeps_the_first_meta_by_name() {
        let root = unique_temp_dir("dup");
        let dir = root.join("cwd").join("sess").join(SUBAGENTS_DIR);
        fs::create_dir_all(&dir).expect("create subagents dir");
        for (id, agent_type) in [
            ("bbbbbbbbbbbbbbbbb", "second"),
            ("aaaaaaaaaaaaaaaaa", "first"),
        ] {
            fs::write(
                dir.join(format!("agent-{id}.meta.json")),
                format!(r#"{{"agentType":"{agent_type}","toolUseId":"toolu_dup"}}"#),
            )
            .expect("write meta");
        }

        let index = index_for(&root.join("cwd").join("sess.jsonl"));

        assert_eq!(
            index.lookup("toolu_dup"),
            Some(&meta("aaaaaaaaaaaaaaaaa", "first", None))
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_session_with_no_subagents_dir_has_an_empty_index() {
        let root = unique_temp_dir("none");
        let index = index_for(&root.join("cwd").join("sess.jsonl"));
        assert!(index.by_tool_use_id.is_empty());
        assert_eq!(
            index.dir(),
            Some(root.join("cwd").join("sess").join(SUBAGENTS_DIR).as_path())
        );
        assert!(index_for(Path::new("/")).by_tool_use_id.is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    /// A meta file past the cap is not a meta, and neither is a dir or a
    /// symlink under a meta name, even one to a valid meta. One at the cap
    /// still reads.
    #[test]
    fn only_a_regular_file_within_the_cap_is_read() {
        let root = unique_temp_dir("read");
        let cap = usize::try_from(META_MAX_BYTES).expect("cap fits usize");

        let at_cap = root.join("at-cap.meta.json");
        fs::write(&at_cap, " ".repeat(cap)).expect("write at cap");
        assert_eq!(read_meta_file(&at_cap).map(|text| text.len()), Some(cap));

        let past_cap = root.join("past-cap.meta.json");
        fs::write(&past_cap, " ".repeat(cap + 1)).expect("write past cap");
        assert_eq!(read_meta_file(&past_cap), None);

        let dir = root.join("dir.meta.json");
        fs::create_dir_all(&dir).expect("create dir");
        assert_eq!(read_meta_file(&dir), None);

        #[cfg(unix)]
        {
            let target = root.join("target.meta.json");
            fs::write(&target, META_RAW).expect("write target");
            let link = root.join("link.meta.json");
            std::os::unix::fs::symlink(&target, &link).expect("create symlink");
            assert!(
                read_meta_file(&target).is_some(),
                "the premise: the target reads"
            );
            assert_eq!(read_meta_file(&link), None);
        }

        assert_eq!(read_meta_file(&root.join("missing.meta.json")), None);
        let _ = fs::remove_dir_all(&root);
    }

    /// Only a regular file opens as a transcript: a missing one, a dir and a
    /// symlink, even one to a real transcript, all refuse.
    #[test]
    fn only_a_regular_file_opens_as_a_transcript() {
        let root = unique_temp_dir("transcript");
        let transcript = transcript_path(&root, "a1b2c3d4e5f6a7b8c");
        fs::write(&transcript, "{}\n").expect("write transcript");
        let mut text = String::new();
        open_transcript(&transcript)
            .expect("a regular file opens")
            .read_to_string(&mut text)
            .expect("and reads");
        assert_eq!(text, "{}\n");

        assert!(open_transcript(&transcript_path(&root, "missing")).is_none());
        let dir = transcript_path(&root, "dir");
        fs::create_dir_all(&dir).expect("create dir");
        assert!(open_transcript(&dir).is_none());

        #[cfg(unix)]
        {
            let link = transcript_path(&root, "link");
            std::os::unix::fs::symlink(&transcript, &link).expect("create symlink");
            assert!(open_transcript(&link).is_none());
        }
        let _ = fs::remove_dir_all(&root);
    }

    /// The reader's boundary (AGENTS.md SUBAGENT EXCLUSION BY DEPTH): every path
    /// it touches for the fixture session, and every transcript a click on one of
    /// its nodes would open, lies under THAT session's own `subagents/` dir, and
    /// none is a path discovery returns. It reads meta files only, never a
    /// transcript.
    #[test]
    fn the_index_touches_only_its_sessions_subagents_dir_and_nothing_discoverable() {
        let root = fixtures_root();
        // Spelled out from the fixture's layout, never through `subagents_dir`,
        // so a derivation that strays is measured against where it must land.
        let own_dir = root
            .join(FIXTURE_FOLDER)
            .join(FIXTURE_SESSION)
            .join("subagents");
        let mut listed: Vec<PathBuf> = Vec::new();
        let mut read: Vec<PathBuf> = Vec::new();

        let index = index_with(
            &fixture_session(),
            |dir| {
                listed.push(dir.to_path_buf());
                list_file_names(dir)
            },
            |path| {
                read.push(path.to_path_buf());
                read_meta_file(path)
            },
        );

        // The premise: it read the fixture, so an empty touch set cannot pass.
        assert!(!index.by_tool_use_id.is_empty(), "the index read nothing");
        assert!(
            read.iter().all(|path| path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(META_FILE_SUFFIX))),
            "only meta files are read: {read:?}"
        );
        let dir = index.dir().expect("a resolved dir");
        let transcripts: Vec<PathBuf> = index
            .by_tool_use_id
            .values()
            .map(|meta| transcript_path(dir, &meta.agent_id))
            .collect();
        assert!(
            transcripts.iter().all(|path| path.is_file()),
            "every joined launch has its transcript in the fixture: {transcripts:?}"
        );

        let discovered: HashSet<PathBuf> = discover::discover(&root).into_iter().collect();
        for path in listed.iter().chain(&read).chain(&transcripts) {
            assert!(
                path.starts_with(&own_dir),
                "{path:?} lies outside {own_dir:?}"
            );
            assert!(!discovered.contains(path), "{path:?} is discoverable");
        }
    }

    // --- the status rule: whether a run ended --------------------------------

    /// One of the per-reader fixtures beside the store: a subagent transcript
    /// per tail shape, trimmed from claude 2.1.226 and 2.1.296 records.
    fn status_fixture(name: &str) -> Vec<u8> {
        fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
                .join("subagent_status")
                .join(name),
        )
        .expect("read the status fixture")
    }

    /// An assistant record holding `blocks` (comma-joined JSON objects), with no
    /// `stop_reason` at all.
    fn assistant(blocks: &str) -> String {
        format!(r#"{{"type":"assistant","message":{{"role":"assistant","content":[{blocks}]}}}}"#)
    }

    /// An assistant record holding `blocks`, its `message.stop_reason` the JSON
    /// `stop_reason`, where claude writes it.
    fn assistant_stopped(blocks: &str, stop_reason: &str) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{blocks}],"stop_reason":{stop_reason}}}}}"#
        )
    }

    /// An assistant record whose `content` is the bare string `Done.`, stopped
    /// with the JSON `stop_reason`.
    fn string_reply(stop_reason: &str) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","content":"Done.","stop_reason":{stop_reason}}}}}"#
        )
    }

    /// A FINAL reply: `blocks`, in a turn claude ended (`end_turn`).
    fn final_reply(blocks: &str) -> String {
        assistant_stopped(blocks, r#""end_turn""#)
    }

    /// A user record whose `content` is the JSON `content`.
    fn user(content: &str) -> String {
        format!(r#"{{"type":"user","message":{{"role":"user","content":{content}}}}}"#)
    }

    const TEXT: &str = r#"{"type":"text","text":"Done."}"#;
    const THINKING: &str = r#"{"type":"thinking","thinking":"","signature":"c2ln"}"#;
    const BASH: &str = r#"{"type":"tool_use","id":"toolu_b","name":"Bash","input":{}}"#;
    const HANDBACK: &str =
        r#"{"type":"tool_use","id":"toolu_h","name":"SubagentHandback","input":{"message":"ok"}}"#;
    const ANSWER: &str = r#"[{"type":"tool_result","tool_use_id":"toolu_b","content":"ok"}]"#;
    const ATTACHMENT: &str = r#"{"type":"attachment","attachment":{"type":"todo_reminder"}}"#;

    /// `lines` as one transcript tail.
    fn tail_of(lines: &[String]) -> Vec<u8> {
        lines.join("\n").into_bytes()
    }

    /// The two shapes an ENDED run's tail takes in claude's own transcripts — a
    /// `SubagentHandback` answered as the last turn, and a final reply — and an
    /// unfinished one, waiting on the sub-subagent it just launched after a line
    /// of text.
    #[test]
    fn a_handback_or_a_final_reply_ends_the_run_and_a_pending_launch_does_not() {
        assert!(
            run_ended(&status_fixture("handback.jsonl"), false),
            "a SubagentHandback, then its answer"
        );
        assert!(
            run_ended(&status_fixture("final-reply.jsonl"), false),
            "a final reply, then an attachment"
        );
        assert!(
            !run_ended(&status_fixture("unfinished.jsonl"), false),
            "a launch still waiting on its sub-subagent"
        );
    }

    /// The tails that END a run, built record by record.
    #[test]
    fn these_last_turns_end_the_run() {
        for (what, lines) in [
            (
                "the hand-back answered, then a final reply after it (claude 2.1.29x)",
                vec![
                    assistant(HANDBACK),
                    user(ANSWER),
                    ATTACHMENT.to_string(),
                    final_reply(TEXT),
                ],
            ),
            (
                "the nudge to hand back, then the hand-back",
                vec![
                    assistant(TEXT),
                    r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"[handback-send-enforce] Call SubagentHandback."}}"#.to_string(),
                    assistant(HANDBACK),
                    user(ANSWER),
                ],
            ),
            (
                "a hand-back beside another call in one record",
                vec![assistant(&format!("{BASH},{HANDBACK}"))],
            ),
            (
                "a reply with thinking before its text",
                vec![final_reply(&format!("{THINKING},{TEXT}"))],
            ),
            (
                "a reply whose content is a bare string",
                vec![string_reply(r#""end_turn""#)],
            ),
        ] {
            assert!(run_ended(&tail_of(&lines), false), "{what}");
        }
    }

    /// R4-1a: a text reply whose turn claude ENDED (`end_turn`) is final, as a
    /// whole tail or after a hand-back's answer, whatever shape its content has.
    #[test]
    fn an_end_turn_reply_ends_the_run() {
        for (what, lines) in [
            ("text blocks", vec![final_reply(TEXT)]),
            ("a bare string", vec![string_reply(r#""end_turn""#)]),
            (
                "after a hand-back's answer",
                vec![assistant(HANDBACK), user(ANSWER), final_reply(TEXT)],
            ),
        ] {
            assert!(run_ended(&tail_of(&lines), false), "{what}");
        }
    }

    /// R4-1a: a reply cut at a stop sequence ended its turn too.
    #[test]
    fn a_stop_sequence_reply_ends_the_run() {
        let stop = r#""stop_sequence""#;
        for (what, lines) in [
            ("text blocks", vec![assistant_stopped(TEXT, stop)]),
            ("a bare string", vec![string_reply(stop)]),
        ] {
            assert!(run_ended(&tail_of(&lines), false), "{what}");
        }
    }

    /// R4-1a: text whose `stop_reason` is null or missing is a turn still being
    /// written — claude writes a narrated tool call's text ahead of its
    /// `tool_use`, as the `mid-turn.jsonl` fixture caught it — so it never ends
    /// the run, nor re-ends one a new prompt reopened.
    #[test]
    fn a_null_or_missing_stop_reason_leaves_the_run_open() {
        for (what, lines) in [
            ("null, text blocks", vec![assistant_stopped(TEXT, "null")]),
            ("null, a bare string", vec![string_reply("null")]),
            ("missing, text blocks", vec![assistant(TEXT)]),
            (
                "null, after a reply and a new prompt",
                vec![
                    final_reply(TEXT),
                    user(r#""Now fix it.""#),
                    assistant_stopped(TEXT, "null"),
                ],
            ),
        ] {
            assert!(!run_ended(&tail_of(&lines), false), "{what}");
        }
        assert!(
            !run_ended(&status_fixture("mid-turn.jsonl"), false),
            "a narration caught before its tool call"
        );
    }

    /// R4-1a: text in a record claude stopped for a tool call has that call
    /// still to come, so the run stays open.
    #[test]
    fn a_tool_use_stop_reason_leaves_the_run_open() {
        let tool_use = r#""tool_use""#;
        for (what, lines) in [
            ("text blocks", vec![assistant_stopped(TEXT, tool_use)]),
            ("a bare string", vec![string_reply(tool_use)]),
            (
                "thinking, then text",
                vec![assistant_stopped(&format!("{THINKING},{TEXT}"), tool_use)],
            ),
        ] {
            assert!(!run_ended(&tail_of(&lines), false), "{what}");
        }
    }

    /// The hand-back answered as claude writes it — the call, then its result —
    /// followed by `rest`.
    fn after_handback(rest: &[String]) -> Vec<String> {
        let mut lines = vec![assistant(HANDBACK), user(ANSWER)];
        lines.extend_from_slice(rest);
        lines
    }

    /// Text after a hand-back keeps the run ended even with no final stop reason:
    /// the hand-back already ended it.
    #[test]
    fn a_null_reply_after_a_handback_ends_the_run() {
        for (what, rest) in [
            ("null, text blocks", vec![assistant_stopped(TEXT, "null")]),
            ("null, a bare string", vec![string_reply("null")]),
            ("missing, text blocks", vec![assistant(TEXT)]),
            (
                "null, thinking and text in one record",
                vec![assistant_stopped(&format!("{THINKING},{TEXT}"), "null")],
            ),
            (
                "null, after an attachment",
                vec![ATTACHMENT.to_string(), assistant_stopped(TEXT, "null")],
            ),
        ] {
            assert!(run_ended(&tail_of(&after_handback(&rest)), false), "{what}");
        }
    }

    /// An `end_turn` reply after a hand-back ends the run as well.
    #[test]
    fn an_end_turn_reply_after_a_handback_ends_the_run() {
        assert!(run_ended(
            &tail_of(&after_handback(&[final_reply(TEXT)])),
            false
        ));
    }

    /// A hand-back is not ended for good: a tool call or a thinking-only record
    /// after the reply that followed it reopens the run.
    #[test]
    fn a_later_tool_use_or_thinking_reopens_a_handed_back_run() {
        let reply = assistant_stopped(TEXT, "null");
        for (what, rest) in [
            ("a tool call", vec![reply.clone(), assistant(BASH)]),
            ("a thinking-only record", vec![reply, assistant(THINKING)]),
        ] {
            assert!(
                !run_ended(&tail_of(&after_handback(&rest)), false),
                "{what}"
            );
        }
    }

    /// A new prompt after a hand-back reopens the run, and the text that answers
    /// it needs a final stop reason again.
    #[test]
    fn a_new_prompt_after_a_handback_reopens_the_run() {
        let prompt = user(r#""More, please.""#);
        let reply = assistant_stopped(TEXT, "null");
        for (what, rest) in [
            ("the prompt", vec![prompt.clone()]),
            (
                "the prompt, then a null reply",
                vec![prompt.clone(), reply.clone()],
            ),
            ("a null reply, then the prompt", vec![reply, prompt]),
        ] {
            assert!(
                !run_ended(&tail_of(&after_handback(&rest)), false),
                "{what}"
            );
        }
    }

    /// With no hand-back, a null reply leaves the run open, after other tool
    /// calls as on its own.
    #[test]
    fn a_null_reply_with_no_handback_leaves_the_run_open() {
        let reply = assistant_stopped(TEXT, "null");
        for (what, lines) in [
            ("on its own", vec![reply.clone()]),
            (
                "after a tool call answered",
                vec![assistant(BASH), user(ANSWER), reply],
            ),
        ] {
            assert!(!run_ended(&tail_of(&lines), false), "{what}");
        }
    }

    /// The tails that leave a run OPEN: a later turn reopens an ended one, and
    /// nothing that is not a turn ends it.
    #[test]
    fn these_last_turns_leave_the_run_open() {
        for (what, lines) in [
            (
                "a text record, then the tool_use of the same message",
                vec![assistant(TEXT), assistant(BASH)],
            ),
            (
                "a reply, then a new prompt",
                vec![final_reply(TEXT), user(r#""Now fix it.""#)],
            ),
            (
                "a reply, then an interrupt",
                vec![
                    final_reply(TEXT),
                    user(r#"[{"type":"text","text":"[Request interrupted by user]"}]"#),
                ],
            ),
            ("a tool call answered", vec![assistant(BASH), user(ANSWER)]),
            ("a thinking-only record", vec![assistant(THINKING)]),
            (
                "text and a tool call in one record",
                vec![assistant(&format!("{TEXT},{BASH}"))],
            ),
            (
                "a hand-back, then a new prompt",
                vec![
                    assistant(HANDBACK),
                    user(ANSWER),
                    user(r#""More, please.""#),
                ],
            ),
            ("only a prompt", vec![user(r#""Start.""#)]),
            ("only attachments", vec![ATTACHMENT.to_string()]),
            ("an empty user record", vec![final_reply(TEXT), user("[]")]),
        ] {
            assert!(!run_ended(&tail_of(&lines), false), "{what}");
        }
        assert!(!run_ended(b"", false), "an empty tail");
        assert!(
            !run_ended(b"not json\n{\n[]\n\"x\"\n42", false),
            "garbage never ends a run, and never panics"
        );
    }

    /// A tail that starts mid-file drops its first line, however it parses: a
    /// complete record there would be a cut one's tail end in a real read.
    #[test]
    fn a_cut_tail_skips_its_first_line() {
        let reply_first = tail_of(&[final_reply(TEXT), ATTACHMENT.to_string()]);
        assert!(run_ended(&reply_first, false), "premise: whole, it ends");
        assert!(
            !run_ended(&reply_first, true),
            "cut, the first line is dropped"
        );

        let mut mid_codepoint = vec![0x80, 0xbf, b'"', b'}', b'\n'];
        mid_codepoint.extend(final_reply(TEXT).into_bytes());
        assert!(
            run_ended(&mid_codepoint, true),
            "a fragment starting mid-codepoint is skipped, the reply after it read"
        );
        assert!(
            !run_ended(&[0x80, 0xbf], true),
            "a tail with no newline at all"
        );

        let mut half_written = tail_of(&[final_reply(TEXT)]);
        half_written.extend_from_slice(b"\n{\"type\":\"user\",\"mess");
        assert!(
            run_ended(&half_written, false),
            "a last line still being written is skipped"
        );
    }

    // --- the running check ---------------------------------------------------

    /// A fixed `now`, so no test reads the clock.
    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }

    /// `secs` before [`now`].
    fn ago(secs: u64) -> SystemTime {
        now() - Duration::from_secs(secs)
    }

    #[test]
    fn an_ended_run_is_never_running_whenever_it_last_wrote() {
        for written in [Some(ago(1)), Some(ago(86_400)), None] {
            assert!(!is_running(Some(true), written, now()), "{written:?}");
        }
    }

    /// The window is inclusive, and a write after `now` (the clocks disagree) is
    /// as recent as a write can be.
    #[test]
    fn an_open_run_is_running_within_the_window_and_quiet_past_it() {
        let window = RUNNING_WINDOW.as_secs();
        assert!(is_running(Some(false), Some(ago(1)), now()));
        assert!(is_running(Some(false), Some(ago(window)), now()));
        assert!(!is_running(Some(false), Some(ago(window + 1)), now()));
        assert!(!is_running(Some(false), None, now()));
        assert!(is_running(
            Some(false),
            Some(now() + Duration::from_secs(5)),
            now()
        ));
    }

    /// A tail the read never parsed claims nothing, so it is never running — not
    /// even beside a fresh write, which a clock stepped back could hand it.
    #[test]
    fn an_unread_tail_is_never_running() {
        for written in [Some(ago(1)), Some(now() + Duration::from_secs(5)), None] {
            assert!(!is_running(None, written, now()), "{written:?}");
        }
    }

    /// The check reads no file and no clock: the SAME stored facts read running
    /// at one `now` and quiet once that `now` has moved past the window.
    #[test]
    fn a_running_subagent_turns_quiet_as_now_moves_past_the_window() {
        let written = Some(ago(60));
        assert!(is_running(Some(false), written, now()));
        assert!(!is_running(Some(false), written, now() + RUNNING_WINDOW));
    }

    /// A status is RUNNING exactly where [`is_running`] says so; else FINISHED
    /// once the end was read, whenever it last wrote; else QUIET — an unfinished
    /// run silent past the window, and a run whose end was never read.
    #[test]
    fn a_status_is_running_where_is_running_says_else_finished_once_its_end_was_read() {
        use SubagentStatus::{Finished, Quiet, Running};

        let window = RUNNING_WINDOW.as_secs();
        let ahead = Some(now() + Duration::from_secs(5));
        for (ended, written, status) in [
            (Some(false), Some(ago(1)), Running),
            (Some(false), Some(ago(window)), Running),
            (Some(false), ahead, Running),
            (Some(false), Some(ago(window + 1)), Quiet),
            (Some(false), None, Quiet),
            (Some(true), Some(ago(1)), Finished),
            (Some(true), Some(ago(86_400)), Finished),
            (Some(true), None, Finished),
            (None, Some(ago(1)), Quiet),
            (None, Some(ago(86_400)), Quiet),
            (None, None, Quiet),
        ] {
            assert_eq!(
                SubagentStatus::of(ended, written, now()),
                status,
                "{ended:?} {written:?}"
            );
            assert_eq!(
                status == Running,
                is_running(ended, written, now()),
                "{ended:?} {written:?}"
            );
        }
    }

    /// The subagent view's title and the background refresh's record judge the
    /// SAME records alike: over each status fixture, the `ended` the view's
    /// render folds out of the whole transcript and the one the record's read
    /// folds out of its tail give the same status — finished for a hand-back and
    /// for a final reply, running for an open run while it writes, and quiet for
    /// that run once its window has passed. Past the window the record reads no
    /// tail, so there only the view's verdict still tells a finished run from a
    /// quiet one.
    #[test]
    fn the_view_and_the_record_judge_the_same_records_to_the_same_status() {
        use crate::store::preview;
        use SubagentStatus::{Finished, Quiet, Running};

        let root = unique_temp_dir("status-agreement");
        let session_file = root.join("cwd").join("sess.jsonl");
        let dir = subagents_dir(&session_file).expect("a session has a dir");
        fs::create_dir_all(&dir).expect("create subagents dir");
        let written = SystemTime::now();
        let cases = [
            ("hb", "handback.jsonl", Finished),
            ("fr", "final-reply.jsonl", Finished),
            ("open", "unfinished.jsonl", Running),
            ("mid", "mid-turn.jsonl", Running),
        ];
        for (index, (id, fixture, _)) in cases.iter().enumerate() {
            write_subagent(
                &dir,
                id,
                &format!(r#"{{"agentType":"{id}-type","toolUseId":"toolu_{index}"}}"#),
                Some(status_fixture(fixture).as_slice()),
                written,
            );
        }
        let view_ended = |id: &str| {
            let rendered =
                preview::render_subagent(&session_file, id, 80, &HashSet::new(), &HashSet::new());
            assert!(rendered.run_ended.is_some(), "{id}: premise, it rendered");
            rendered.run_ended
        };
        let statuses = |at: SystemTime| {
            let record = read_record(&session_file, at);
            cases.map(|(id, _, _)| {
                let listed = entry(&record, id);
                let from_record = SubagentStatus::of(listed.ended, listed.newest_write, at);
                let from_view = SubagentStatus::of(view_ended(id), listed.newest_write, at);
                (from_record, from_view)
            })
        };

        let read_at = written + Duration::from_secs(1);
        for ((id, _, status), (from_record, from_view)) in cases.iter().zip(statuses(read_at)) {
            assert_eq!(from_view, from_record, "{id}: the two agree");
            assert_eq!(from_record, *status, "{id}");
        }

        let past_window = written + RUNNING_WINDOW + Duration::from_secs(1);
        for ((id, _, status), (from_record, from_view)) in cases.iter().zip(statuses(past_window)) {
            if *status == Running {
                assert_eq!(
                    (from_record, from_view),
                    (Quiet, Quiet),
                    "{id}: an open run gone silent"
                );
            } else {
                assert_eq!(
                    (from_record, from_view),
                    (Quiet, Finished),
                    "{id}: only the view still reads its end"
                );
            }
        }
        let _ = fs::remove_dir_all(&root);
    }

    // --- the record: launch order, steps and the open target ------------------

    /// The instant `secs` after the Unix epoch.
    fn at(secs: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(secs).expect("a valid instant")
    }

    /// A subagent `id` launched at `launched` (seconds), with no write and an
    /// unread tail.
    fn sub(id: &str, launched: Option<i64>) -> Subagent {
        Subagent {
            agent_id: id.to_owned(),
            agent_type: format!("{id}-type"),
            description: None,
            launched: launched.map(at),
            newest_write: None,
            ended: None,
        }
    }

    /// A record of `ids`, launched in the order given.
    fn launched_in_order(ids: &[&str]) -> SubagentRecord {
        SubagentRecord::new(
            ids.iter()
                .zip(1..)
                .map(|(id, secs)| sub(id, Some(secs)))
                .collect(),
        )
    }

    /// The agent ids of `record`, in its order.
    fn ids(record: &SubagentRecord) -> Vec<&str> {
        record
            .subagents
            .iter()
            .map(|subagent| subagent.agent_id.as_str())
            .collect()
    }

    /// The id of what a record method picked.
    fn id_of(subagent: Option<&Subagent>) -> Option<&str> {
        subagent.map(|subagent| subagent.agent_id.as_str())
    }

    /// R4-4a: launch order is the first-record timestamp ascending, a tie going to
    /// the agent id; a subagent with no launch time comes after every one with
    /// one, by agent id among themselves — whatever order the read found them in.
    #[test]
    fn the_record_is_in_launch_order_with_an_unknown_launch_last() {
        let record = SubagentRecord::new(vec![
            sub("z-none", None),
            sub("b-tied", Some(200)),
            sub("late", Some(300)),
            sub("a-none", None),
            sub("a-tied", Some(200)),
            sub("first", Some(100)),
        ]);
        assert_eq!(
            ids(&record),
            ["first", "a-tied", "b-tied", "late", "a-none", "z-none"]
        );
    }

    /// R4-4a: `→` and `←` step through launch order and wrap round at both ends.
    #[test]
    fn stepping_wraps_round_at_both_ends() {
        let record = launched_in_order(&["a", "b", "c"]);
        let step = |from: &str, forward: bool| id_of(record.step(from, forward));
        assert_eq!(step("a", true), Some("b"));
        assert_eq!(step("b", true), Some("c"));
        assert_eq!(
            step("c", true),
            Some("a"),
            "→ from the last wraps to the first"
        );
        assert_eq!(
            step("a", false),
            Some("c"),
            "← from the first wraps to the last"
        );
        assert_eq!(step("c", false), Some("b"));
    }

    /// A subagent the record does not list yet counts as the newest launch: `→`
    /// wraps round to the first, `←` lands on the last.
    #[test]
    fn stepping_from_an_unlisted_subagent_treats_it_as_the_newest_launch() {
        let record = launched_in_order(&["a", "b", "c"]);
        assert_eq!(id_of(record.step("unlisted", true)), Some("a"));
        assert_eq!(id_of(record.step("unlisted", false)), Some("c"));
    }

    /// One subagent steps onto itself; a record with none has nowhere to go.
    #[test]
    fn a_lone_subagent_steps_onto_itself_and_an_empty_record_nowhere() {
        let lone = launched_in_order(&["a"]);
        for forward in [true, false] {
            assert_eq!(id_of(lone.step("a", forward)), Some("a"));
            assert_eq!(id_of(lone.step("unlisted", forward)), Some("a"));
            assert!(SubagentRecord::default().step("a", forward).is_none());
        }
    }

    /// The title's `k/n`: 1-based, out of every subagent the record lists.
    #[test]
    fn a_position_is_one_based_out_of_the_count() {
        let record = launched_in_order(&["a", "b", "c", "d", "e"]);
        assert_eq!(record.position("a"), Some((1, 5)));
        assert_eq!(record.position("b"), Some((2, 5)));
        assert_eq!(record.position("e"), Some((5, 5)));
        assert_eq!(record.position("unlisted"), None);
        assert_eq!(SubagentRecord::default().position("a"), None);
    }

    /// `get` hands out the listed subagent by its id, wherever it stands in
    /// launch order, and nothing for an id the record does not list.
    #[test]
    fn get_finds_a_listed_subagent_by_its_id_and_nothing_else() {
        let record = launched_in_order(&["a", "b", "c"]);
        for id in ["a", "b", "c"] {
            assert_eq!(id_of(record.get(id)), Some(id));
        }
        assert_eq!(id_of(record.get("unlisted")), None);
        assert_eq!(id_of(SubagentRecord::default().get("a")), None);
    }

    /// R4-4a: `Ctrl-X ↓` opens the FIRST running subagent in launch order — not
    /// the freshest writer, and never an ended one, however fresh. Once `now`
    /// passes every window nothing runs, and the most recent write wins instead.
    #[test]
    fn the_open_target_is_the_first_running_subagent_in_launch_order() {
        let record = SubagentRecord::new(vec![
            Subagent {
                newest_write: Some(ago(3)),
                ended: Some(true),
                ..sub("done-fresh", Some(1))
            },
            Subagent {
                newest_write: Some(ago(300)),
                ended: Some(false),
                ..sub("running-early", Some(2))
            },
            Subagent {
                newest_write: Some(ago(5)),
                ended: Some(false),
                ..sub("running-late", Some(3))
            },
        ]);
        assert_eq!(id_of(record.open_target(now())), Some("running-early"));
        assert_eq!(
            id_of(record.open_target(now() + RUNNING_WINDOW + RUNNING_WINDOW)),
            Some("done-fresh"),
            "with none running, the most recent write"
        );
    }

    /// With none running, the most recently written subagent; a tie goes to the
    /// LATER launch (a parent shares its child's write); one with no write loses
    /// to every one with one; an empty record has no target.
    #[test]
    fn with_none_running_the_open_target_is_the_most_recently_written() {
        let quiet = |id: &str, launched: i64, written: Option<u64>| Subagent {
            newest_write: written.map(ago),
            ..sub(id, Some(launched))
        };
        let record = SubagentRecord::new(vec![
            quiet("old", 1, Some(7_200)),
            quiet("newest", 2, Some(900)),
            quiet("no-write", 3, None),
            quiet("older", 4, Some(3_600)),
        ]);
        assert_eq!(id_of(record.open_target(now())), Some("newest"));
        let tied = SubagentRecord::new(vec![
            quiet("parent", 1, Some(900)),
            quiet("child", 2, Some(900)),
        ]);
        assert_eq!(id_of(tied.open_target(now())), Some("child"));
        let unwritten = SubagentRecord::new(vec![quiet("a", 1, None), quiet("b", 2, None)]);
        assert_eq!(
            id_of(unwritten.open_target(now())),
            Some("b"),
            "no write anywhere: the latest launch"
        );
        assert!(SubagentRecord::default().open_target(now()).is_none());
    }

    // --- the record: the launch time --------------------------------------------

    fn rfc3339(raw: &str) -> OffsetDateTime {
        OffsetDateTime::parse(raw, &Rfc3339).expect("a valid RFC 3339 instant")
    }

    /// FAIL-SOFT: a line with no usable timestamp gives none, and never panics.
    #[test]
    fn a_line_gives_its_rfc3339_timestamp_or_none() {
        assert_eq!(
            record_timestamp(br#"{"type":"user","timestamp":"2026-10-12T08:00:00.000Z"}"#),
            Some(rfc3339("2026-10-12T08:00:00.000Z"))
        );
        assert_eq!(
            record_timestamp(b"{\"timestamp\":\"2026-10-12T09:00:00+01:00\"}\n"),
            Some(rfc3339("2026-10-12T08:00:00Z")),
            "an offset is the same instant, and a trailing newline is no harm"
        );
        let deep = "[".repeat(10_000);
        for line in [
            b"".as_slice(),
            b"not json",
            b"[]",
            br#""2026-10-12T08:00:00Z""#,
            br#"{"type":"fork-context-ref","agentId":"a1"}"#,
            br#"{"timestamp":7}"#,
            br#"{"timestamp":null}"#,
            br#"{"timestamp":"yesterday"}"#,
            br#"{"timestamp":"2026-10-12T08:00"#,
            deep.as_bytes(),
        ] {
            assert_eq!(
                record_timestamp(line),
                None,
                "{:?}",
                String::from_utf8_lossy(line)
            );
        }
    }

    /// The launch time is the FIRST timestamp in the head: a later record's never
    /// wins, a forked subagent's opening `fork-context-ref` (no timestamp) and a
    /// garbled line are passed over, and a first record longer than the head cap
    /// leaves none to read — as does a missing or empty file.
    #[test]
    fn the_launch_time_is_the_first_timestamp_in_the_head() {
        let root = unique_temp_dir("head");
        let record = |ts: &str| format!(r#"{{"type":"user","timestamp":"{ts}"}}"#);
        let write = |name: &str, lines: &[String]| {
            let path = root.join(name);
            fs::write(&path, lines.join("\n") + "\n").expect("write transcript");
            path
        };
        let first = "2026-10-12T08:00:00.000Z";
        let second = "2026-10-12T08:05:00.000Z";

        let plain = write("plain.jsonl", &[record(first), record(second)]);
        assert_eq!(read_launch_time(&plain), Some(rfc3339(first)));

        let forked = write(
            "forked.jsonl",
            &[
                r#"{"type":"fork-context-ref","agentId":"a1"}"#.to_owned(),
                record(second),
            ],
        );
        assert_eq!(read_launch_time(&forked), Some(rfc3339(second)));

        let garbled = write("garbled.jsonl", &["{not json".to_owned(), record(second)]);
        assert_eq!(read_launch_time(&garbled), Some(rfc3339(second)));

        let cap = usize::try_from(HEAD_MAX_BYTES).expect("cap fits usize");
        let huge_first = write(
            "huge-first.jsonl",
            &[
                format!(
                    r#"{{"type":"user","message":{{"content":"{}"}},"timestamp":"{first}"}}"#,
                    "x".repeat(cap)
                ),
                record(second),
            ],
        );
        assert_eq!(read_launch_time(&huge_first), None);

        let empty = write("empty.jsonl", &[]);
        assert_eq!(read_launch_time(&empty), None);
        assert_eq!(read_launch_time(&root.join("missing.jsonl")), None);
        let _ = fs::remove_dir_all(&root);
    }

    // --- the record: the read ----------------------------------------------------

    /// A session's subagents through injected readers, at [`now`]: each entry is
    /// `(id, parent, own write, tail ends the run)`, a `None` write being no
    /// regular transcript at all. Returns the record and every transcript whose
    /// TAIL the read opened, sorted.
    fn read_injected(
        entries: &[(&str, Option<&str>, Option<SystemTime>, bool)],
    ) -> (SubagentRecord, Vec<PathBuf>) {
        const LEN: u64 = 4_096;
        let session_file = Path::new("/store/-cwd/sess.jsonl");
        let dir = subagents_dir(session_file).expect("a session has a dir");
        let names: Vec<String> = entries
            .iter()
            .map(|(id, ..)| format!("agent-{id}.meta.json"))
            .collect();
        let metas: HashMap<PathBuf, String> = entries
            .iter()
            .map(|&(id, parent, ..)| {
                let parent = parent.map_or(String::new(), |p| format!(r#","parentAgentId":"{p}""#));
                (
                    dir.join(format!("agent-{id}.meta.json")),
                    format!(r#"{{"agentType":"t-{id}","toolUseId":"toolu_{id}"{parent}}}"#),
                )
            })
            .collect();
        let stats: HashMap<PathBuf, TranscriptStat> = entries
            .iter()
            .filter_map(|&(id, _, written, _)| {
                let stat = TranscriptStat {
                    written: Some(written?),
                    len: LEN,
                };
                Some((transcript_path(&dir, id), stat))
            })
            .collect();
        let ends: HashMap<PathBuf, bool> = entries
            .iter()
            .map(|&(id, _, _, ends)| (transcript_path(&dir, id), ends))
            .collect();
        let mut tails = Vec::new();
        let record = read_record_with(
            session_file,
            now(),
            |_| names.clone(),
            |path| metas.get(path).cloned(),
            |path| stats.get(path).copied(),
            |_| None,
            |path, len| {
                assert_eq!(len, LEN, "the tail is bounded by the stat's length");
                tails.push(path.to_path_buf());
                ends[path]
            },
        );
        tails.sort();
        (record, tails)
    }

    /// The subagent `id` in `record`.
    fn entry<'r>(record: &'r SubagentRecord, id: &str) -> &'r Subagent {
        record
            .subagents
            .iter()
            .find(|subagent| subagent.agent_id == id)
            .unwrap_or_else(|| panic!("{id} is listed: {record:?}"))
    }

    /// Whether `id` reads as running at [`now`].
    fn running(record: &SubagentRecord, id: &str) -> bool {
        let subagent = entry(record, id);
        is_running(subagent.ended, subagent.newest_write, now())
    }

    /// The read opens the TAIL of exactly the transcripts that can be running:
    /// those written within the window, and every parent above one at any depth,
    /// a cycle walked once. Every other transcript is stat and head only, its
    /// `ended` left `None`. A parent with no transcript of its own above a
    /// running child has no tail to open, and reads as unended.
    #[test]
    fn only_a_transcript_that_can_be_running_has_its_tail_read() {
        let (record, tails) = read_injected(&[
            ("lead", None, Some(ago(3_600)), false),
            ("reviewer", Some("lead"), Some(ago(3_000)), false),
            ("checker", Some("reviewer"), Some(ago(60)), false),
            ("sibling", None, Some(ago(3_600)), false),
            ("fresh", None, Some(ago(5)), true),
            ("cyc-a", Some("cyc-b"), Some(ago(7_200)), false),
            ("cyc-b", Some("cyc-a"), Some(ago(7_200)), false),
            ("cyc-c", Some("cyc-d"), Some(ago(7_200)), false),
            ("cyc-d", Some("cyc-c"), Some(ago(30)), false),
            ("hollow", None, None, false),
            ("orphan", Some("hollow"), Some(ago(10)), false),
        ]);
        let dir = subagents_dir(Path::new("/store/-cwd/sess.jsonl")).expect("a dir");
        let mut expected: Vec<PathBuf> = [
            "lead", "reviewer", "checker", "fresh", "cyc-c", "cyc-d", "orphan",
        ]
        .iter()
        .map(|id| transcript_path(&dir, id))
        .collect();
        expected.sort();
        assert_eq!(tails, expected);
        for id in ["sibling", "cyc-a", "cyc-b"] {
            assert_eq!(
                entry(&record, id).ended,
                None,
                "{id}: its tail was not read"
            );
        }
        assert_eq!(entry(&record, "fresh").ended, Some(true));
        assert_eq!(
            entry(&record, "hollow").ended,
            Some(false),
            "no transcript under a running child: an unended run"
        );
        assert!(running(&record, "hollow"));
    }

    /// A parent silent for an hour is RUNNING while a sub-subagent below it —
    /// at any depth — wrote within the window, and its newest write is that
    /// write. A write counts only for the agents ABOVE the writer, and an ended
    /// run stays not running whatever is still writing below it.
    #[test]
    fn a_sub_subagents_write_keeps_its_ancestors_running() {
        let (record, _) = read_injected(&[
            ("lead", None, Some(ago(3_600)), false),
            ("reviewer", Some("lead"), Some(ago(3_000)), false),
            ("checker", Some("reviewer"), Some(ago(60)), false),
            ("sibling", None, Some(ago(3_600)), false),
            ("done", None, Some(ago(3_600)), true),
            ("busy", Some("done"), Some(ago(30)), false),
        ]);
        for id in ["lead", "reviewer", "checker"] {
            assert!(running(&record, id), "{id}");
            assert_eq!(entry(&record, id).newest_write, Some(ago(60)), "{id}");
        }
        assert!(
            !running(&record, "sibling"),
            "another subagent's write never counts for it"
        );
        assert_eq!(entry(&record, "sibling").newest_write, Some(ago(3_600)));
        assert!(!running(&record, "done"), "its own run ended");
        assert!(running(&record, "busy"));
    }

    /// An unended subagent reads running exactly when its newest write — its own
    /// or a sub-subagent's — is within the window, the instant an age is stated
    /// from, so the two can never disagree; and its tail is read exactly then.
    #[test]
    fn an_unended_rows_age_and_status_never_disagree() {
        let window = RUNNING_WINDOW.as_secs();
        for own in [10, window, window + 1, 3_600] {
            for child in [None, Some(5), Some(window + 60)] {
                let mut entries = vec![("p", None, Some(ago(own)), false)];
                if let Some(secs) = child {
                    entries.push(("c", Some("p"), Some(ago(secs)), false));
                }
                let (record, _) = read_injected(&entries);
                for subagent in &record.subagents {
                    let within = within_window(subagent.newest_write, now());
                    assert_eq!(
                        is_running(subagent.ended, subagent.newest_write, now()),
                        within,
                        "own {own}s, child {child:?}: {subagent:?}"
                    );
                    assert_eq!(
                        subagent.ended.is_some(),
                        within,
                        "own {own}s, child {child:?}: {subagent:?}"
                    );
                }
            }
        }
    }

    /// A `parentAgentId` cycle — which no real store holds, but a hostile meta
    /// file can — is walked once, and a subagent with no readable transcript is
    /// not running, with no write.
    #[test]
    fn a_parent_cycle_terminates_and_a_missing_transcript_is_quiet() {
        let (record, _) = read_injected(&[
            ("a", Some("b"), Some(ago(3_600)), false),
            ("b", Some("a"), Some(ago(5)), false),
            ("gone", None, None, false),
        ]);
        for id in ["a", "b"] {
            assert!(running(&record, id), "{id}");
            assert_eq!(entry(&record, id).newest_write, Some(ago(5)), "{id}");
        }
        assert!(!running(&record, "gone"));
        assert_eq!(entry(&record, "gone").newest_write, None);
        assert_eq!(record.subagents.len(), 3);
    }

    /// A transcript ENDS within its last [`TAIL_MAX_BYTES`], however long it is:
    /// the cut first line is skipped. A transcript whose last record alone
    /// outgrows the tail reads as unended, and anything but a regular file has no
    /// stat at all.
    #[test]
    fn a_transcripts_stat_and_tail_read_only_a_regular_file() {
        let root = unique_temp_dir("tail");
        let cap = usize::try_from(TAIL_MAX_BYTES).expect("cap fits usize");

        let long = root.join("long.jsonl");
        let filler = format!(
            r#"{{"type":"user","message":{{"content":"{}"}}}}"#,
            "x".repeat(cap)
        );
        fs::write(&long, format!("{filler}\n{}\n", final_reply(TEXT))).expect("write long");
        let stat = stat_transcript(&long).expect("a regular file has a stat");
        assert!(stat.written.is_some(), "and its mtime is read");
        assert!(
            read_tail_ended(&long, stat.len),
            "the final reply sits in the tail"
        );
        assert!(
            read_tail(&long, stat.len).is_some_and(|(tail, cut)| cut && tail.len() == cap),
            "only the tail is read, and it says it is cut"
        );

        let huge_last = root.join("huge-last.jsonl");
        fs::write(&huge_last, format!("{}\n{filler}\n", final_reply(TEXT))).expect("write huge");
        let len = stat_transcript(&huge_last).expect("a stat").len;
        assert!(
            !read_tail_ended(&huge_last, len),
            "no complete record in the tail reads as unended"
        );

        assert_eq!(stat_transcript(&root.join("missing.jsonl")), None);
        assert!(!read_tail_ended(&root.join("missing.jsonl"), 10));
        let dir = root.join("dir.jsonl");
        fs::create_dir_all(&dir).expect("create dir");
        assert_eq!(stat_transcript(&dir), None);
        #[cfg(unix)]
        {
            let link = root.join("link.jsonl");
            std::os::unix::fs::symlink(&long, &link).expect("create symlink");
            assert_eq!(stat_transcript(&link), None);
        }
        let _ = fs::remove_dir_all(&root);
    }

    /// Write `agent-<id>.meta.json` and, given `transcript` bytes, its
    /// `agent-<id>.jsonl` written `written`.
    fn write_subagent(
        dir: &Path,
        id: &str,
        meta_json: &str,
        transcript: Option<&[u8]>,
        written: SystemTime,
    ) {
        fs::write(dir.join(format!("agent-{id}.meta.json")), meta_json).expect("write meta");
        if let Some(bytes) = transcript {
            let path = transcript_path(dir, id);
            fs::write(&path, bytes).expect("write transcript");
            File::options()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_modified(written))
                .expect("set mtime");
        }
    }

    /// The whole read over a real dir, through the status fixtures: the record
    /// comes in launch order (two transcripts sharing a first timestamp by agent
    /// id, one with no transcript last); a final reply written a minute ago was
    /// read and ended; a hand-back two hours old was never opened; a launch
    /// waiting on a sub-subagent is running while that sub-subagent writes, though
    /// it is itself an hour silent; and a forked skill's decoy file is no entry.
    #[test]
    fn read_record_lists_one_sessions_subagents_in_launch_order() {
        let root = unique_temp_dir("record");
        let session_file = root.join("cwd").join("sess.jsonl");
        let dir = subagents_dir(&session_file).expect("a session has a dir");
        fs::create_dir_all(&dir).expect("create subagents dir");
        let now = SystemTime::now();
        let ago = |secs| now - Duration::from_secs(secs);
        let meta_json = |kind: &str, tool: &str, parent: Option<&str>| match parent {
            Some(parent) => format!(
                r#"{{"agentType":"{kind}","description":"{kind} work","toolUseId":"{tool}","parentAgentId":"{parent}"}}"#
            ),
            None => format!(
                r#"{{"agentType":"{kind}","description":"{kind} work","toolUseId":"{tool}"}}"#
            ),
        };
        let handback = status_fixture("handback.jsonl");
        let final_reply = status_fixture("final-reply.jsonl");
        let unfinished = status_fixture("unfinished.jsonl");
        write_subagent(
            &dir,
            "hb",
            &meta_json("tester", "toolu_1", None),
            Some(handback.as_slice()),
            ago(7_200),
        );
        write_subagent(
            &dir,
            "fr",
            &meta_json("writer", "toolu_2", None),
            Some(final_reply.as_slice()),
            ago(60),
        );
        write_subagent(
            &dir,
            "rev",
            &meta_json("reviewer", "toolu_4", Some("lead")),
            Some(unfinished.as_slice()),
            ago(20),
        );
        write_subagent(
            &dir,
            "lead",
            &meta_json("lead", "toolu_3", None),
            Some(unfinished.as_slice()),
            ago(3_600),
        );
        write_subagent(
            &dir,
            "gone",
            &meta_json("ghost", "toolu_5", None),
            None,
            now,
        );
        fs::write(
            dir.join("agent-hb.forked-skill.json"),
            meta_json("decoy", "toolu_6", None),
        )
        .expect("write decoy");

        let record = read_record(&session_file, now);

        assert_eq!(ids(&record), ["fr", "hb", "lead", "rev", "gone"]);
        let state = |id: &str| {
            let subagent = entry(&record, id);
            (
                subagent.ended,
                is_running(subagent.ended, subagent.newest_write, now),
            )
        };
        assert_eq!(state("fr"), (Some(true), false), "read, and ended");
        assert_eq!(state("hb"), (None, false), "too old to be opened");
        assert_eq!(state("lead"), (Some(false), true), "its child writes");
        assert_eq!(state("rev"), (Some(false), true));
        assert_eq!(state("gone"), (None, false));
        let lead = entry(&record, "lead");
        assert_eq!(lead.agent_type, "lead");
        assert_eq!(lead.description.as_deref(), Some("lead work"));
        assert_eq!(
            lead.newest_write,
            entry(&record, "rev").newest_write,
            "its newest write is its sub-subagent's"
        );
        assert_eq!(
            entry(&record, "fr").launched,
            Some(rfc3339("2026-09-02T09:10:00.000Z")),
            "the fixture's first record"
        );
        assert_eq!(entry(&record, "gone").launched, None, "no transcript");
        assert_eq!(
            read_record(&root.join("cwd").join("other.jsonl"), now),
            SubagentRecord::default()
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The read's boundary: of two sessions side by side, each with subagents,
    /// the read of one lists, reads metas from, stats, reads heads in and reads
    /// tails in THAT session's own `subagents/` dir alone, and none of it is a
    /// path discovery returns.
    #[test]
    fn read_record_touches_only_its_own_sessions_subagents_dir() {
        let root = unique_temp_dir("record-boundary");
        let cwd = root.join("cwd");
        let mine = cwd.join("mine.jsonl");
        let theirs = cwd.join("theirs.jsonl");
        fs::create_dir_all(&cwd).expect("create cwd dir");
        for (session, id) in [(&mine, "m1"), (&theirs, "t1")] {
            fs::write(session, "{}\n").expect("write session");
            let dir = subagents_dir(session).expect("a dir");
            fs::create_dir_all(&dir).expect("create dir");
            write_subagent(
                &dir,
                id,
                &format!(r#"{{"agentType":"a","toolUseId":"toolu_{id}"}}"#),
                Some(status_fixture("final-reply.jsonl").as_slice()),
                SystemTime::now(),
            );
        }
        let own_dir = cwd.join("mine").join("subagents");
        let mut listed: Vec<PathBuf> = Vec::new();
        let mut metas_read: Vec<PathBuf> = Vec::new();
        let mut stated: Vec<PathBuf> = Vec::new();
        let mut heads_read: Vec<PathBuf> = Vec::new();
        let mut tails_read: Vec<PathBuf> = Vec::new();

        let record = read_record_with(
            &mine,
            SystemTime::now(),
            |dir| {
                listed.push(dir.to_path_buf());
                list_file_names(dir)
            },
            |path| {
                metas_read.push(path.to_path_buf());
                read_meta_file(path)
            },
            |path| {
                stated.push(path.to_path_buf());
                stat_transcript(path)
            },
            |path| {
                heads_read.push(path.to_path_buf());
                read_launch_time(path)
            },
            |path, len| {
                tails_read.push(path.to_path_buf());
                read_tail_ended(path, len)
            },
        );

        assert_eq!(
            ids(&record),
            ["m1"],
            "premise: its one subagent was read: {record:?}"
        );
        let transcript = own_dir.join("agent-m1.jsonl");
        for (what, paths) in [
            ("stat", &stated),
            ("head", &heads_read),
            ("tail", &tails_read),
        ] {
            assert_eq!(paths.as_slice(), [transcript.as_path()], "{what}");
        }
        let discovered: HashSet<PathBuf> = discover::discover(&root).into_iter().collect();
        assert!(
            discovered.contains(&theirs),
            "premise: the other session is a real one"
        );
        for path in listed
            .iter()
            .chain(&metas_read)
            .chain(&stated)
            .chain(&heads_read)
            .chain(&tails_read)
        {
            assert!(
                path.starts_with(&own_dir),
                "{path:?} lies outside {own_dir:?}"
            );
            assert!(!discovered.contains(path), "{path:?} is discoverable");
        }
        let _ = fs::remove_dir_all(&root);
    }
}
