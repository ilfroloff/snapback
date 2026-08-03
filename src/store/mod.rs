//! Session store: the framework-independent data core.
//!
//! Owns the `Session` model and the top-level [`SessionStore`] reload pipeline
//! (discover -> parse -> derive label/repo/content_index), returning sessions
//! sorted repo -> branch -> timestamp-desc. Every historical correctness
//! constraint (subagent exclusion, resume-from-inside-file `cwd`, fail-soft
//! parsing) lives here and is covered by unit tests under `tests/`.
//!
//! The store is INCREMENTAL: it keeps an in-memory `path -> (stamp, session)`
//! cache and re-parses only the files whose `(mtime, len)` moved, which is what
//! keeps a watcher reload proportional to what a `claude` child just wrote
//! rather than to the whole store. Discovery is NEVER cached, and neither is
//! anything but a COMPLETED read — see [`SessionStore::reload`].

pub mod discover;
pub mod group;
pub mod label;
pub mod lineage;
pub mod parse;
pub mod preview;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rayon::prelude::*;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// How old a file's mtime must be before a parse taken from it may be REMEMBERED
/// as keyed on `(mtime, len)`.
///
/// The pair can only notice a rewrite that moved one of its two halves, and
/// filesystem timestamp GRANULARITY is what breaks that: HFS+ records whole
/// SECONDS, and SMB/FAT volumes round to TWO — both of which the store may sit
/// on (a network home directory, an external or case-insensitive volume).
/// Within one granule a file can be rewritten to the same length and still
/// report the same mtime, so a parse taken inside that window read bytes that
/// may already be gone with nothing in the stamp to say so.
///
/// Two seconds is the coarsest granularity in that set — the same window
/// `make`-style tools leave for the same reason — so a file whose mtime is
/// younger than this is ALWAYS re-parsed, and the parse is discarded rather than
/// cached. The window is spent ONCE, on the way in ([`cacheable`]), against the
/// PARSE instant; checking it against the reuse instant instead would let wall
/// time alone promote a doomed parse to trusted. It costs almost nothing: the
/// files it excludes are exactly the handful being written right now, which is
/// where a cache is worth least anyway.
///
/// Visible to the crate ONLY so a test elsewhere can wait out the window by
/// name rather than by a duplicated literal; nothing outside this module reads
/// it to make a decision.
pub(crate) const MTIME_SETTLE_WINDOW: Duration = Duration::from_secs(2);

/// A file's cheap change key: its last-modified time AND its byte length, which
/// are only ever compared TOGETHER (see [`can_reuse`]). Neither half is
/// sufficient alone — an append moves both, but an in-place rewrite may move
/// only the mtime and a same-second truncate-and-rewrite only the length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    /// Last-modified time as the filesystem reports it.
    mtime: SystemTime,
    /// Size in bytes.
    len: u64,
}

/// One cached file: the stamp its parse was taken at, and what that parse said.
///
/// Only a VERDICT ABOUT CONTENT ever gets here — see the insert site in
/// [`SessionStore::reload_at`], which is the single place the two gates
/// ([`parse::FileVerdict`] and [`cacheable`]) are spent.
struct CacheEntry {
    /// The stamp the file carried when [`Session::from_file`] last ran on it.
    /// Settled at that instant, never merely at the instant it is reused.
    stamp: FileStamp,
    /// `None` records that this file is NOT a resumable session: it was read end
    /// to end and carried no `cwd` (a sidecar). Cached like any other answer so a
    /// sidecar is not re-read on every reload; it says nothing about which files
    /// EXIST, which only discovery decides.
    ///
    /// A file that could not be READ never reaches this field — that is not an
    /// answer about the file, and caching it would make the session missing
    /// rather than stale.
    session: Option<Session>,
}

/// One discovered file's contribution to a reload.
enum Loaded {
    /// The cached parse was reused; the cache entry is moved across untouched.
    Reused {
        path: PathBuf,
        session: Option<Session>,
    },
    /// The file was read from disk this reload.
    Parsed {
        path: PathBuf,
        /// `None` when the metadata could not be read, in which case nothing is
        /// cached and the next reload reads the file again.
        stamp: Option<FileStamp>,
        /// What the read found — including whether it got to finish, which is
        /// what decides if any of it may be remembered.
        verdict: parse::FileVerdict<Session>,
    },
}

/// The result of one [`SessionStore::reload`]: the whole board, plus which of
/// its rows were actually re-read from disk.
pub struct Reload {
    /// Every resumable session, sorted repo asc / branch asc / timestamp desc —
    /// byte-for-byte what a full re-parse would have produced.
    pub sessions: Vec<Session>,
    /// The `session_id`s this reload re-read from disk (new, changed, or simply
    /// too freshly written to trust the cache for).
    ///
    /// A SUPERSET of what actually differs, never a subset: a file re-read
    /// inside [`MTIME_SETTLE_WINDOW`] lands here even when its bytes did not
    /// move. Consumers use it to drop derived caches, so over-reporting costs a
    /// re-render while under-reporting would show stale text — the safe
    /// direction is the one taken.
    pub changed: HashSet<String>,
}

impl Reload {
    /// A reload in which EVERY session counts as changed.
    ///
    /// What a caller states when its sessions did not come through the cache at
    /// all (a test's synthetic list), so nothing derived from them may be reused.
    #[must_use]
    // Reached through `App::apply_sessions`, whose only callers are tests — the
    // binary-crate `dead_code` quirk (PATTERNS §9), not an unused API.
    #[allow(dead_code)]
    pub fn everything(sessions: Vec<Session>) -> Self {
        let changed = sessions.iter().map(|s| s.session_id.clone()).collect();
        Reload { sessions, changed }
    }
}

/// Whether `mtime` is far enough in the past for a `(mtime, len)` stamp taken
/// against it to be a trustworthy change key.
///
/// The two clocks are DIFFERENT clocks: `mtime` comes from the filesystem's
/// (which, on a network volume, is the server's) while `now` is the local system
/// clock. Skew in either direction is handled by failing toward a re-parse —
/// the cost of that is a parse, whereas the cost of the other direction is a
/// stale row. So an `mtime` in the FUTURE (`duration_since` errors, which is
/// also what a forward clock jump looks like) is never settled, and a file is
/// settled only once it is at least `window` old.
fn stamp_settled(mtime: SystemTime, now: SystemTime, window: Duration) -> bool {
    match now.duration_since(mtime) {
        Ok(age) => age >= window,
        Err(_) => false,
    }
}

/// Whether a parse just taken from a file stamped `stamp` may be CACHED.
///
/// The settle window is spent HERE, at the PARSE instant, because that is the
/// only instant it is sound at. The question the window answers is "could the
/// bytes this parse just read still be replaced without moving either half of
/// the stamp?", and that is a question about when the parse happened — not about
/// when someone later asks to reuse it, an instant that only grows more
/// permissive with time.
///
/// Judging it at the reuse instant instead admits exactly the race the window
/// exists to rule out. On a 1 s-granularity volume: mtime floors to `100`, a
/// reload at `100.3` parses (age `0.3`) and caches, a write at `100.7` to the
/// same length leaves the mtime still floored at `100`, and a reload at `102.5`
/// then sees age `2.5` with a matching stamp and trusts a parse of bytes that
/// were gone before it was ever asked about. Refusing to cache the `100.3` parse
/// closes it: the entry never exists, so nothing can promote it.
///
/// Refusing costs one re-parse per reload for the handful of transcripts being
/// written RIGHT NOW, which is unchanged from before — those files were already
/// re-read every reload — and is where a cache is worth least anyway.
///
/// Pure, and takes `now` as a PARAMETER rather than reading the clock, so the
/// decision is testable without a filesystem or a sleep (PATTERNS §3).
fn cacheable(stamp: FileStamp, now: SystemTime) -> bool {
    stamp_settled(stamp.mtime, now, MTIME_SETTLE_WINDOW)
}

/// Whether a parse taken when the file was stamped `cached` may be reused for a
/// file now stamped `current`.
///
/// The two stamps and nothing else. The settle window is NOT re-checked here,
/// and its absence is the load-bearing part: an entry exists only because
/// [`cacheable`] already found its mtime settled at the instant the parse was
/// taken, so that mtime's granule was closed by then and any write since must
/// have moved one half of the stamp. A second check against the REUSE instant
/// would read like the soundness condition while being strictly weaker than the
/// one already spent.
fn can_reuse(cached: FileStamp, current: FileStamp) -> bool {
    cached == current
}

/// Read one file's change key. `None` when the metadata is unreadable or carries
/// no mtime — fail-soft toward re-parsing AND toward not caching, so an
/// unstampable file is simply read fresh every reload.
fn file_stamp(path: &Path) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(FileStamp {
        mtime: meta.modified().ok()?,
        len: meta.len(),
    })
}

/// A resumable Claude Code session, derived fail-soft from one JSONL file.
#[derive(Debug, Clone)]
pub struct Session {
    /// Absolute path to the `<session-id>.jsonl` transcript (drives resume).
    pub file: PathBuf,
    /// `sessionId` read from inside the file (else the file stem).
    pub session_id: String,
    /// `cwd` read from inside the file (authoritative for resume).
    pub cwd: PathBuf,
    /// `gitBranch` from inside the file; `None` renders as `(detached)`.
    pub git_branch: Option<String>,
    /// Most-recent activity timestamp, parsed from RFC 3339 (`None` if absent
    /// or unparseable).
    pub timestamp: Option<OffsetDateTime>,
    /// Derived repo grouping label (see [`group::repo_of`]).
    pub repo: String,
    /// Derived display label (see [`label::finalize_label`]).
    pub label: String,
    /// `uuid` of the transcript tree's root record (see [`parse::ParsedFile`]).
    /// Shared verbatim by every member of a background-fork lineage, so it is
    /// what identifies one (read by [`lineage::lineage_key`]). `None` (no
    /// derivable root) means "no lineage", and is never folded.
    pub root_uuid: Option<String>,
    /// How many conversation turns the transcript holds (see
    /// [`parse::ParsedFile::msg_count`]).
    ///
    /// Drawn on an expanded lineage CHILD row, and it is the one field there
    /// that carries real information: every member of a lineage shares a label
    /// BY CONSTRUCTION (a background hand-off copies the conversation), so the
    /// label cannot tell them apart and neither a timestamp nor an id says
    /// which member is a stalled stub and which holds the work. `6` beside
    /// `171` says it at a glance.
    pub msg_count: usize,
    /// Capped, readable transcript text for content search.
    pub content_index: String,
}

impl Session {
    /// The branch label for grouping/display, defaulting to `(detached)`.
    pub fn branch_display(&self) -> &str {
        self.git_branch.as_deref().unwrap_or(group::DETACHED)
    }

    /// Read one candidate file and say what it is: a resumable session, a file
    /// that is not one, or a file that could not be read.
    ///
    /// The three-way answer is [`parse::parse_file`]'s and is CARRIED THROUGH
    /// rather than flattened, because the caller's next decision — whether to
    /// remember this — turns on which of the two non-session arms it was.
    fn from_file(path: &Path) -> parse::FileVerdict<Session> {
        parse::parse_file(path).map(|parsed| {
            let cwd = PathBuf::from(&parsed.cwd);
            let mut repo = group::repo_of(&cwd);
            if repo.is_empty() {
                repo = "(unknown)".to_string();
            }
            let label = label::finalize_label(
                parsed.summary.as_deref(),
                parsed.first_user.as_deref(),
                &parsed.session_id,
            );
            let timestamp = parsed
                .timestamp_raw
                .as_deref()
                .and_then(|t| OffsetDateTime::parse(t, &Rfc3339).ok());

            Session {
                file: path.to_path_buf(),
                session_id: parsed.session_id,
                cwd,
                git_branch: parsed.git_branch,
                timestamp,
                repo,
                label,
                root_uuid: parsed.root_uuid,
                msg_count: parsed.msg_count,
                content_index: parsed.content_index,
            }
        })
    }
}

/// The data core entry point: one store root plus the parses already taken from
/// it.
///
/// Stateful on purpose. A full parse of a real store costs ~0.4s of CPU, and the
/// recursive watcher fires whenever ANY `claude` process anywhere writes a
/// transcript — several times a second while agents run — so re-reading every
/// file per reload burned CPU continuously to re-derive answers that had not
/// changed. The cache keys each discovered file's CONTENT by `(mtime, len)`; it
/// never keys which files exist.
///
/// The cache is IN-MEMORY ONLY and is never written to disk: the one persistent
/// thing `snapback` owns is the hidden-session id set (see [`crate::hidden`]).
pub struct SessionStore {
    /// The store root every reload discovers from. Owned rather than passed per
    /// call because the cache below is keyed by paths under it, so a cache and
    /// the root it was built from must not be able to drift apart.
    root: PathBuf,
    /// Absolute file path -> the parse last taken from it, and the stamp it was
    /// taken at. Rebuilt from the DISCOVERED set on every reload, which is what
    /// prunes deleted files.
    cache: HashMap<PathBuf, CacheEntry>,
    /// How many discovered files the last reload actually read from disk.
    /// Instrumented so the partial-reload behaviour is observable (and
    /// unit-testable), mirroring [`crate::search::SearchIndex::last_rebuilt`].
    last_parsed: usize,
    /// How many files the last reload discovered — the denominator for
    /// [`last_parsed`](Self::last_parsed).
    last_discovered: usize,
}

impl SessionStore {
    /// An empty store over `root`. The first [`reload`](Self::reload) reads
    /// every discovered file; later ones read only what changed.
    #[must_use]
    pub fn new(root: &Path) -> Self {
        SessionStore {
            root: root.to_path_buf(),
            cache: HashMap::new(),
            last_parsed: 0,
            last_discovered: 0,
        }
    }

    /// The store root this instance discovers from.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Re-read the store, parsing ONLY the files whose `(mtime, len)` moved.
    ///
    /// Pipeline: discover (subagent-excluding) -> parse or reuse (fail-soft, per
    /// file in parallel) -> derive (label, repo, timestamp, content_index) ->
    /// sort by repo asc, branch asc, timestamp desc. The result is identical to
    /// a full re-parse; only the work is smaller.
    ///
    /// **Discovery always runs.** The cache answers what a discovered file
    /// CONTAINS and never which files exist, so a newly created session is
    /// parsed the first reload that sees it and a deleted one leaves the
    /// board — and takes its cache entry with it, since the cache is rebuilt
    /// from the discovered set rather than edited in place.
    ///
    /// That alone does not bound the damage, because a cached ANSWER can hide a
    /// file just as well as a cached listing: only a completed read is
    /// remembered here (see the insert site below), so a read that failed costs
    /// this one reload and is retried on the next, exactly as it would have
    /// without a cache. Between them, the failure mode a cache can have here is
    /// a briefly stale ROW, never a permanently missing one.
    pub fn reload(&mut self) -> Reload {
        self.reload_at(SystemTime::now())
    }

    /// [`reload`](Self::reload) against a stated `now`.
    ///
    /// The clock is a parameter for the usual reason (PATTERNS §3): the settle
    /// window is a decision, and a test must be able to state the instant it is
    /// judged against instead of sleeping through it.
    fn reload_at(&mut self, now: SystemTime) -> Reload {
        let files = discover::discover(&self.root);
        // Take the cache so a reused entry can be MOVED back into the new one
        // rather than cloned; the parallel pass below only reads it.
        let mut previous = std::mem::take(&mut self.cache);

        let loaded: Vec<Loaded> = {
            let previous = &previous;
            files
                .into_par_iter()
                .map(|path| {
                    let stamp = file_stamp(&path);
                    let reusable = stamp.and_then(|current| {
                        previous
                            .get(&path)
                            .filter(|entry| can_reuse(entry.stamp, current))
                    });
                    match reusable {
                        Some(entry) => Loaded::Reused {
                            session: entry.session.clone(),
                            path,
                        },
                        None => {
                            let verdict = Session::from_file(&path);
                            Loaded::Parsed {
                                path,
                                stamp,
                                verdict,
                            }
                        }
                    }
                })
                .collect()
        };

        let discovered = loaded.len();
        let mut parsed = 0usize;
        let mut cache = HashMap::with_capacity(discovered);
        let mut sessions = Vec::with_capacity(discovered);
        let mut changed = HashSet::new();

        for entry in loaded {
            let session = match entry {
                Loaded::Reused { path, session } => {
                    // Carry the untouched entry across; anything NOT carried
                    // (a file that left the store) is dropped with `previous`.
                    if let Some(cached) = previous.remove(&path) {
                        cache.insert(path, cached);
                    }
                    session
                }
                Loaded::Parsed {
                    path,
                    stamp,
                    verdict,
                } => {
                    parsed += 1;
                    // The ONE place a parse is remembered, and it takes two
                    // gates because there are two ways to be wrong about it.
                    let session = match verdict {
                        // The read failed. That is no answer about this file, so
                        // there is none to keep: no row this reload, no entry,
                        // and the next reload reads it again — which is what
                        // stops a blip becoming a permanently missing session.
                        parse::FileVerdict::Unreadable => None,
                        // The read finished, so its answer describes the bytes —
                        // but only bytes the stamp can still vouch for. A parse
                        // taken inside the settle window read bytes that could
                        // be replaced without moving either half of it.
                        readable => {
                            let session = readable.session();
                            if let Some(stamp) = stamp.filter(|stamp| cacheable(*stamp, now)) {
                                cache.insert(
                                    path,
                                    CacheEntry {
                                        stamp,
                                        session: session.clone(),
                                    },
                                );
                            }
                            session
                        }
                    };
                    if let Some(session) = &session {
                        changed.insert(session.session_id.clone());
                    }
                    session
                }
            };
            if let Some(session) = session {
                sessions.push(session);
            }
        }

        self.cache = cache;
        self.last_parsed = parsed;
        self.last_discovered = discovered;
        sessions.sort_by(session_ordering);
        Reload { sessions, changed }
    }

    /// Drop every cached parse, so the next [`reload`](Self::reload) reads the
    /// whole store again.
    ///
    /// The escape hatch, and the reason a wedged cache can never be a permanent
    /// wrong answer: `Ctrl-X r` on the board calls this and reloads (see
    /// `tui::update::chord_key`), so a user who suspects a stale row can force a
    /// full re-read without restarting. It costs one full parse and nothing else
    /// — no state is discarded but the cache.
    pub fn invalidate(&mut self) {
        self.cache.clear();
    }

    /// How many files the last reload read from disk (vs. reused).
    #[allow(dead_code)] // Instrumentation: read by the reload tests.
    pub fn last_parsed(&self) -> usize {
        self.last_parsed
    }

    /// How many files the last reload discovered.
    #[allow(dead_code)] // Instrumentation: read by the reload tests.
    pub fn last_discovered(&self) -> usize {
        self.last_discovered
    }

    /// One-shot load from the default store root (`$CLAUDE_PROJECTS_DIR` or
    /// `~/.claude/projects`), keeping no cache. Used by `--print-list`, which
    /// loads once and exits.
    pub fn load() -> Vec<Session> {
        Self::load_from(&discover::store_root())
    }

    /// One-shot load from an explicit store `root`, keeping no cache.
    ///
    /// The same pipeline [`reload`](Self::reload) runs, over an empty cache, so
    /// every file is read. For anything that reloads more than once, hold a
    /// [`SessionStore`] instead — that is the whole point of the cache.
    pub fn load_from(root: &Path) -> Vec<Session> {
        Self::new(root).reload().sessions
    }
}

/// repo asc, branch asc, timestamp desc.
/// Sessions with no timestamp sort last within their group.
fn session_ordering(a: &Session, b: &Session) -> std::cmp::Ordering {
    a.repo
        .cmp(&b.repo)
        .then_with(|| a.branch_display().cmp(b.branch_display()))
        .then_with(|| b.timestamp.cmp(&a.timestamp))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::UNIX_EPOCH;

    /// Path to the committed fixture store root.
    fn fixtures_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("store")
    }

    fn load() -> Vec<Session> {
        SessionStore::load_from(&fixtures_root())
    }

    fn find<'a>(sessions: &'a [Session], id: &str) -> &'a Session {
        sessions
            .iter()
            .find(|s| s.session_id == id)
            .unwrap_or_else(|| panic!("session {id} not loaded"))
    }

    #[test]
    fn discover_excludes_nested_subagents() {
        let files = discover::discover(&fixtures_root());
        assert!(
            !files
                .iter()
                .any(|p| p.components().any(|c| c.as_os_str() == "subagents")),
            "discovery must never descend into a subagents/ directory: {files:?}"
        );
        // The eight depth-2 `.jsonl` files, none of the nested subagent file.
        assert_eq!(files.len(), 8, "unexpected discovered set: {files:?}");
    }

    #[test]
    fn subagent_transcript_is_not_a_session() {
        let sessions = load();
        assert!(
            !sessions
                .iter()
                .any(|s| s.file.components().any(|c| c.as_os_str() == "subagents")),
            "a subagent transcript leaked in as a session"
        );
    }

    #[test]
    fn sidecar_without_cwd_is_dropped() {
        let sessions = load();
        // The sidecar carries no `cwd` and no `sessionId`; it must not appear.
        assert!(
            !sessions.iter().any(|s| s.label.contains("Sidecar title")),
            "a sidecar file with no cwd was surfaced as a session"
        );
        // Exactly seven resumable sessions survive (8 discovered - 1 sidecar).
        assert_eq!(sessions.len(), 7, "unexpected session count");
    }

    #[test]
    fn fork_pair_shares_one_root_uuid_through_the_session_model() {
        let sessions = load();
        let fg = find(&sessions, "sess-fork-fg-1");
        let bg = find(&sessions, "sess-fork-bg-1");

        // The lineage identity survives `Session::from_file`, and it is the
        // SHARED tree root copied verbatim into the background fork — an
        // `attachment`, ahead of either file's first user message.
        assert_eq!(fg.root_uuid.as_deref(), Some("fork-root-att"));
        assert_eq!(bg.root_uuid.as_deref(), Some("fork-root-att"));

        // Why the pair is indistinguishable on the board today: same repo, same
        // branch, same label. This is the "double session" the fold addresses.
        assert_eq!(fg.label, bg.label);
        assert_eq!(fg.repo, bg.repo);
        assert_eq!(fg.branch_display(), bg.branch_display());
        assert_ne!(fg.session_id, bg.session_id, "two distinct session files");

        // The background copy is the newer one (D1's head), and it kept growing
        // after the fork while the ancestor stalled.
        assert!(bg.timestamp > fg.timestamp);
    }

    #[test]
    fn fork_pair_members_report_different_turn_counts() {
        let sessions = load();
        let fg = find(&sessions, "sess-fork-fg-1");
        let bg = find(&sessions, "sess-fork-bg-1");

        // The exact shape of the reported bug: everything a board row normally
        // shows is IDENTICAL across the pair (asserted above), so the turn count
        // is the only field that separates the stalled ancestor from the copy
        // that kept working. If these two ever agreed, the column would be
        // decoration — a fixture that cannot distinguish the members cannot
        // test the thing they are distinguished BY.
        assert_ne!(
            fg.msg_count, bg.msg_count,
            "the fork pair must differ in turns or it pins nothing"
        );
        // The ancestor stalled at the fork point with one exchange; the bg copy
        // carries that same exchange plus the work it did afterwards.
        assert_eq!(fg.msg_count, 2);
        assert_eq!(bg.msg_count, 4);
        assert!(
            bg.msg_count > fg.msg_count,
            "the member that kept going is the one holding the work"
        );

        // Both files carry THREE copied `attachment` records ahead of their
        // first prompt, so counting tree records instead of turns would report
        // 5 and 7. The turns are what the count means.
        assert_ne!(fg.msg_count, 5);
        assert_ne!(bg.msg_count, 7);
    }

    #[test]
    fn a_session_with_no_null_parent_record_survives_with_no_root() {
        let sessions = load();
        let s = find(&sessions, "sess-rootless-1");
        // FAIL-SOFT: no derivable root degrades to "no lineage" (never folded),
        // never to a dropped session.
        assert_eq!(s.root_uuid, None);
        assert_eq!(s.cwd, PathBuf::from("/Users/me/project-delta"));
        assert_eq!(s.label, "Rewrite the changelog entry");
    }

    #[test]
    fn normal_session_reads_fields_from_inside_the_file() {
        let sessions = load();
        let s = find(&sessions, "sess-normal-1");
        // cwd + sessionId come from INSIDE the file, not the encoded folder.
        assert_eq!(s.cwd, PathBuf::from("/Users/me/project-alpha"));
        assert_eq!(s.git_branch.as_deref(), Some("main"));
        assert_eq!(s.repo, "project-alpha");
        // Summary wins the label preference.
        assert_eq!(s.label, "Fix the payment webhook retries");
        assert!(s.timestamp.is_some(), "timestamp should parse");
        // Content index captured readable transcript text for search.
        assert!(s.content_index.contains("webhook"));
    }

    #[test]
    fn malformed_line_does_not_break_the_rest() {
        let sessions = load();
        let s = find(&sessions, "sess-malformed-1");
        assert_eq!(s.cwd, PathBuf::from("/Users/me/project-gamma"));
        assert_eq!(s.git_branch.as_deref(), Some("dev"));
        // The valid user prompt after the malformed line is still the label.
        assert_eq!(s.label, "Add retry logic to the client");
    }

    #[test]
    fn worktree_cwd_collapses_to_parent_base_repo() {
        let sessions = load();
        let s = find(&sessions, "sess-worktree-1");
        assert_eq!(
            s.cwd,
            PathBuf::from("/Users/me/acme/web-worktrees/feature-x")
        );
        assert_eq!(s.repo, "acme/web");
        assert_eq!(s.branch_display(), "feature-x");
    }

    #[test]
    fn no_summary_falls_back_to_first_real_user_prompt() {
        let sessions = load();
        let s = find(&sessions, "sess-nosummary-1");
        // The `<command-name>`-wrapped turn is skipped; the first REAL prompt
        // (a typed-block user message) becomes the label.
        assert_eq!(s.label, "Implement the login flow");
    }

    #[test]
    fn missing_branch_defaults_to_detached() {
        let sessions = load();
        let s = find(&sessions, "sess-nosummary-1");
        assert_eq!(s.git_branch, None);
        assert_eq!(s.branch_display(), "(detached)");
    }

    #[test]
    fn sessions_are_sorted_repo_then_branch() {
        let sessions = load();
        let repos: Vec<&str> = sessions.iter().map(|s| s.repo.as_str()).collect();
        let mut sorted = repos.clone();
        sorted.sort();
        assert_eq!(repos, sorted, "sessions must be grouped by repo");
    }

    #[test]
    fn preview_renders_readable_turns() {
        let sessions = load();
        let s = find(&sessions, "sess-normal-1");
        // Preview now yields a `RenderedPreview` (styled `Text` + link regions);
        // flatten the text back to plain text (span contents joined) to assert the
        // structural markers survive. Width is the table shrink-to-fit budget; a
        // comfortable 80 columns here.
        let rendered = preview::render(s, 80, &std::collections::HashSet::new());
        let plain: String = rendered
            .text
            .lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|sp| sp.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            plain.contains("\u{25b6} you"),
            "missing user marker: {plain}"
        );
        assert!(
            plain.contains("\u{25cf} claude"),
            "missing claude marker: {plain}"
        );
        // Styling lives in ratatui `Style`, never embedded ANSI escapes.
        assert!(
            !plain.contains('\u{1b}'),
            "preview must not contain ANSI escapes"
        );
    }

    // --- incremental reload -------------------------------------------------

    /// An isolated store root under the system temp dir. Never touches the real
    /// `~/.claude/projects`.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "snapback-store-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp store dir");
        dir
    }

    /// Write a one-turn session at `<root>/<folder>/<id>.jsonl`, returning its path.
    fn write_session(root: &Path, folder: &str, id: &str, prompt: &str) -> PathBuf {
        let dir = root.join(folder);
        std::fs::create_dir_all(&dir).expect("create encoded-cwd dir");
        let path = dir.join(format!("{id}.jsonl"));
        std::fs::write(&path, session_line(id, prompt, "2026-01-01T00:00:00Z")).expect("write");
        path
    }

    /// One `user` record: enough for a resumable session (it carries a `cwd`).
    fn session_line(id: &str, prompt: &str, timestamp: &str) -> String {
        format!(
            r#"{{"type":"user","cwd":"/Users/me/project-alpha","sessionId":"{id}","gitBranch":"main","timestamp":"{timestamp}","uuid":"u-{id}","parentUuid":null,"message":{{"content":"{prompt}"}}}}"#
        )
    }

    /// A `now` far enough past every file the test just wrote for
    /// [`MTIME_SETTLE_WINDOW`] to have elapsed — stated rather than slept for.
    fn settled_now() -> SystemTime {
        SystemTime::now() + MTIME_SETTLE_WINDOW * 2
    }

    /// Every field of every session, in order, as a comparable shape.
    fn shape(sessions: &[Session]) -> Vec<String> {
        sessions.iter().map(|s| format!("{s:?}")).collect()
    }

    fn stamp(secs_ago: u64, len: u64) -> (FileStamp, SystemTime) {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        (
            FileStamp {
                mtime: now - Duration::from_secs(secs_ago),
                len,
            },
            now,
        )
    }

    /// The settle window itself: a stamp is only trustworthy once the window has
    /// elapsed, and the boundary is inclusive.
    #[test]
    fn a_stamp_is_settled_only_once_the_window_has_elapsed() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let window = MTIME_SETTLE_WINDOW;

        assert!(!stamp_settled(now, now, window), "written this instant");
        assert!(
            !stamp_settled(now - Duration::from_millis(1_999), now, window),
            "still inside the window"
        );
        assert!(
            stamp_settled(now - window, now, window),
            "exactly at the window is settled"
        );
        assert!(stamp_settled(now - Duration::from_secs(60), now, window));
    }

    /// The two clocks are different clocks (the filesystem's, possibly a
    /// server's, vs. the local one), so an mtime in the FUTURE is a real case.
    /// It must fail toward re-parsing, never toward trusting the cache.
    #[test]
    fn a_future_mtime_is_never_settled() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert!(!stamp_settled(
            now + Duration::from_secs(1),
            now,
            MTIME_SETTLE_WINDOW
        ));
        assert!(
            !stamp_settled(now + Duration::from_secs(86_400), now, MTIME_SETTLE_WINDOW),
            "a badly skewed clock must not license a reuse"
        );
    }

    /// BOTH halves of the stamp are load-bearing: an in-place rewrite can move
    /// only the mtime, and a same-instant rewrite only the length.
    #[test]
    fn can_reuse_demands_both_halves_of_the_stamp() {
        let (cached, _now) = stamp(60, 4_096);

        assert!(can_reuse(cached, cached), "an untouched file is reusable");

        let grown = FileStamp {
            len: 8_192,
            ..cached
        };
        assert!(!can_reuse(cached, grown), "the length moved");

        let touched = FileStamp {
            mtime: cached.mtime + Duration::from_secs(1),
            ..cached
        };
        assert!(!can_reuse(cached, touched), "the mtime moved");
    }

    /// The same-tick rewrite race, decided where it is decidable: on the way IN.
    /// A coarse-granularity filesystem cannot distinguish a rewrite inside the
    /// current granule from no write at all, so a parse taken inside the window
    /// is never worth remembering — and the stamp comparison alone would happily
    /// have said yes, which is why the window cannot live there.
    #[test]
    fn a_parse_of_a_freshly_written_file_is_never_cached() {
        let (fresh, now) = stamp(0, 4_096);
        assert!(
            !cacheable(fresh, now),
            "a parse taken inside the settle window must not be remembered"
        );
        assert!(
            can_reuse(fresh, fresh),
            "the stamps match — the window is the only thing between them, and \
             it has to be spent before the entry exists"
        );

        let (settled, now) = stamp(60, 4_096);
        assert!(cacheable(settled, now), "a settled parse is worth keeping");
    }

    /// The point of the whole change: a reload over an unchanged store reads
    /// NOTHING from disk, while still reporting every session.
    #[test]
    fn a_second_reload_reads_nothing_when_the_store_did_not_move() {
        let root = unique_temp_dir("steady");
        write_session(&root, "-Users-me-project-alpha", "s1", "first");
        write_session(&root, "-Users-me-project-alpha", "s2", "second");

        let mut store = SessionStore::new(&root);
        let first = store.reload_at(settled_now());
        assert_eq!(store.last_parsed(), 2, "the cold reload reads every file");
        assert_eq!(store.last_discovered(), 2);

        let second = store.reload_at(settled_now());
        assert_eq!(
            store.last_parsed(),
            0,
            "an unchanged store must cost no parses at all"
        );
        assert_eq!(
            store.last_discovered(),
            2,
            "discovery still runs on every reload"
        );
        assert!(second.changed.is_empty(), "nothing was re-read");
        assert_eq!(
            shape(&first.sessions),
            shape(&second.sessions),
            "a reused board must be identical to the parsed one"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The failure mode that must be IMPOSSIBLE: a session on disk that never
    /// reaches the board. The cache keys a discovered file's CONTENT, so a file
    /// it has never seen is parsed the first reload that discovers it.
    #[test]
    fn a_new_file_always_reaches_the_board() {
        let root = unique_temp_dir("new-file");
        write_session(&root, "-Users-me-project-alpha", "s1", "first");

        let mut store = SessionStore::new(&root);
        store.reload_at(settled_now());

        write_session(&root, "-Users-me-project-alpha", "s2", "second");
        // A brand-new folder too — discovery walks both levels every time.
        write_session(&root, "-Users-me-project-beta", "s3", "third");
        let reload = store.reload_at(settled_now());

        let ids: Vec<&str> = reload
            .sessions
            .iter()
            .map(|s| s.session_id.as_str())
            .collect();
        assert!(
            ids.contains(&"s2"),
            "a new session must reach the board: {ids:?}"
        );
        assert!(
            ids.contains(&"s3"),
            "including one in a new folder: {ids:?}"
        );
        assert_eq!(store.last_parsed(), 2, "only the two new files were read");
        assert_eq!(reload.changed.len(), 2);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A deleted file leaves the board — the cache can never resurrect it,
    /// because the new cache is built from the DISCOVERED set rather than edited.
    #[test]
    fn a_deleted_file_leaves_the_board_and_the_cache() {
        let root = unique_temp_dir("deleted");
        write_session(&root, "-Users-me-project-alpha", "s1", "first");
        let doomed = write_session(&root, "-Users-me-project-alpha", "s2", "second");

        let mut store = SessionStore::new(&root);
        store.reload_at(settled_now());

        std::fs::remove_file(&doomed).expect("remove session file");
        let reload = store.reload_at(settled_now());

        let ids: Vec<&str> = reload
            .sessions
            .iter()
            .map(|s| s.session_id.as_str())
            .collect();
        assert_eq!(ids, ["s1"], "the deleted session must be gone");
        assert_eq!(store.last_discovered(), 1);
        assert!(
            !store.cache.contains_key(&doomed),
            "its cache entry must be pruned with it"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A grown transcript is re-read, reported as changed, and shows its new
    /// content — the reason the cache is keyed rather than kept.
    #[test]
    fn a_changed_file_is_re_read_and_reported_changed() {
        let root = unique_temp_dir("changed");
        let growing = write_session(&root, "-Users-me-project-alpha", "s1", "first");
        write_session(&root, "-Users-me-project-alpha", "s2", "steady");

        let mut store = SessionStore::new(&root);
        store.reload_at(settled_now());

        let mut grown = std::fs::read_to_string(&growing).expect("read");
        grown.push('\n');
        grown.push_str(&session_line("s1", "a later turn", "2026-02-02T00:00:00Z"));
        std::fs::write(&growing, grown).expect("append a turn");

        let reload = store.reload_at(settled_now());
        assert_eq!(store.last_parsed(), 1, "only the grown file was re-read");
        assert_eq!(
            reload.changed.iter().collect::<Vec<_>>(),
            vec!["s1"],
            "the changed set names exactly the re-read session"
        );
        let s1 = find(&reload.sessions, "s1");
        assert_eq!(s1.msg_count, 2, "the appended turn is on the board");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// An incremental reload is not merely close to a full one — it is the same
    /// board. Pinned against the one-shot `load_from`, which shares the pipeline
    /// but starts from an empty cache.
    #[test]
    fn an_incremental_reload_matches_a_full_one() {
        let root = unique_temp_dir("equivalence");
        write_session(&root, "-Users-me-project-alpha", "s1", "first");
        write_session(&root, "-Users-me-project-beta", "s2", "second");

        let mut store = SessionStore::new(&root);
        store.reload_at(settled_now());
        write_session(&root, "-Users-me-project-alpha", "s3", "third");
        let incremental = store.reload_at(settled_now());

        assert_eq!(
            shape(&incremental.sessions),
            shape(&SessionStore::load_from(&root)),
            "a partly-reused board must equal a fully-parsed one, field for field"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The boundary claim, verified rather than assumed: a reused session is
    /// byte-identical enough that the search index reuses its haystacks too, so
    /// the saved parse is not handed straight back to a rebuild one layer up.
    #[test]
    fn reused_sessions_let_the_search_index_reuse_them_too() {
        use crate::search::SearchIndex;

        let root = unique_temp_dir("index-reuse");
        write_session(&root, "-Users-me-project-alpha", "s1", "first");
        write_session(&root, "-Users-me-project-alpha", "s2", "second");

        let mut store = SessionStore::new(&root);
        let first = store.reload_at(settled_now());
        let mut index = SearchIndex::build(&first.sessions);
        assert_eq!(index.last_rebuilt(), 2, "the cold build builds every entry");

        let second = store.reload_at(settled_now());
        index.refresh(&second.sessions);
        assert_eq!(
            index.last_rebuilt(),
            0,
            "reused sessions must carry identical searchable fields"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The escape hatch: a wedged cache is never a permanent wrong answer,
    /// because a full re-read is one call away.
    #[test]
    fn invalidate_forces_a_full_re_read() {
        let root = unique_temp_dir("invalidate");
        write_session(&root, "-Users-me-project-alpha", "s1", "first");
        write_session(&root, "-Users-me-project-alpha", "s2", "second");

        let mut store = SessionStore::new(&root);
        let first = store.reload_at(settled_now());
        store.reload_at(settled_now());
        assert_eq!(store.last_parsed(), 0, "steady state reuses everything");

        store.invalidate();
        let rescanned = store.reload_at(settled_now());
        assert_eq!(store.last_parsed(), 2, "every file is read again");
        assert_eq!(
            shape(&first.sessions),
            shape(&rescanned.sessions),
            "a forced rescan must land on the same board"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Pin `path`'s mtime to `at`, modelling a filesystem GRANULE floor: two
    /// writes inside one granule report the same mtime, which is the premise
    /// [`MTIME_SETTLE_WINDOW`] exists to answer. Stated outright rather than
    /// slept for, so the test is deterministic whatever the temp volume's real
    /// timestamp granularity is.
    fn pin_mtime(path: &Path, at: SystemTime) {
        let file = std::fs::File::options()
            .write(true)
            .open(path)
            .expect("open the transcript to re-stamp it");
        file.set_times(std::fs::FileTimes::new().set_modified(at))
            .expect("pin the mtime");
    }

    /// The race the settle window exists to rule out, reproduced end to end.
    ///
    /// The window is only sound applied at the PARSE instant. A parse taken
    /// while the file was still inside its granule read bytes that could still
    /// be replaced without moving either half of the stamp; judging the window
    /// at the REUSE instant instead lets that doomed parse be promoted to
    /// trusted the moment enough wall time passes around it.
    ///
    /// The counterexample, on a 1 s-granularity volume:
    ///
    /// - `100.0` the file is written; its mtime floors to `100`
    /// - `100.3` a reload parses it — age `0.3`, inside the window
    /// - `100.7` it is rewritten to the SAME length; the mtime still floors to `100`
    /// - `102.5` a reload sees age `2.5` and a matching stamp
    #[test]
    fn a_parse_taken_inside_the_settle_window_is_never_promoted_to_trusted() {
        let root = unique_temp_dir("granule");
        let path = write_session(&root, "-Users-me-project-alpha", "s1", "the first turn");

        // The granule floor that BOTH writes below report.
        pin_mtime(&path, SystemTime::now());
        let stamped = std::fs::metadata(&path).expect("stat");
        let (mtime, len) = (stamped.modified().expect("mtime"), stamped.len());

        // `100.3`: the parse lands inside the window.
        let mut store = SessionStore::new(&root);
        let first = store.reload_at(mtime + Duration::from_millis(300));
        assert_eq!(find(&first.sessions, "s1").content_index, "the first turn");

        // `100.7`: rewritten to the same length, inside the same granule, so
        // NEITHER half of the stamp moves. The replacement prompt is the same
        // width as the original on purpose — a fixture that moved the length
        // would be re-read for the ordinary reason and prove nothing.
        std::fs::write(
            &path,
            session_line("s1", "the later turn", "2026-01-01T00:00:00Z"),
        )
        .expect("rewrite inside the granule");
        pin_mtime(&path, mtime);
        let rewritten = std::fs::metadata(&path).expect("stat");
        assert_eq!(rewritten.len(), len, "the fixture must not move the length");
        assert_eq!(
            rewritten.modified().expect("mtime"),
            mtime,
            "nor the mtime — the granule is what hides this write"
        );

        // `102.5`: the window has elapsed AROUND the doomed parse.
        let second = store.reload_at(mtime + Duration::from_millis(2_500));
        assert_eq!(
            find(&second.sessions, "s1").content_index,
            "the later turn",
            "a parse taken inside the window must never become trusted later: \
             the stamp cannot vouch for bytes it was already unable to see change"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The failure mode a cached parse must never create: a session that is on
    /// disk, readable, and permanently absent from the board.
    ///
    /// A read that FAILED is not a verdict about the file's content — EMFILE, a
    /// permissions blip, a network home directory that blinked — so it must not
    /// be cached. Cached, it would be re-served for as long as the stamp sits
    /// still, and a FINISHED transcript's stamp never moves again: the session
    /// would be gone for the life of the process.
    ///
    /// Unix-only, because the test needs a read that genuinely fails. `chmod
    /// 000` is the portable way to arrange one, and it is also the exact shape
    /// that latches: it moves NEITHER half of the stamp, so nothing but a
    /// refusal to cache the failure can bring the row back.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_is_never_latched_into_a_missing_session() {
        use std::os::unix::fs::PermissionsExt;

        let root = unique_temp_dir("unreadable");
        write_session(&root, "-Users-me-project-alpha", "s1", "readable");
        let blocked = write_session(&root, "-Users-me-project-alpha", "s2", "blocked");

        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000))
            .expect("drop every permission bit");
        // A run with the privilege to read it anyway would arrange the failure
        // away entirely, and a fixture that hides the bug reads as coverage.
        assert!(
            std::fs::File::open(&blocked).is_err(),
            "the fixture must actually be unreadable, or it proves nothing"
        );

        let mut store = SessionStore::new(&root);
        let first = store.reload_at(settled_now());
        let ids: Vec<&str> = first
            .sessions
            .iter()
            .map(|s| s.session_id.as_str())
            .collect();
        assert_eq!(ids, ["s1"], "an unreadable file yields no row, fail-soft");

        // The blip passes. `chmod` touches neither mtime nor length, so the
        // stamp is byte-identical to the one the failed read was taken at.
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o644))
            .expect("restore the mode");
        let second = store.reload_at(settled_now());
        let ids: Vec<&str> = second
            .sessions
            .iter()
            .map(|s| s.session_id.as_str())
            .collect();
        assert!(
            ids.contains(&"s2"),
            "a session lost to a transient read error must come back on the next \
             reload, not wait for a restart: {ids:?}"
        );
        assert_eq!(
            store.last_parsed(),
            1,
            "and only the failed file is re-read — the readable one is still reused"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The other half of the negative cache: "no `cwd`, so not a session" IS a
    /// verdict about content, so it is cached (a sidecar must not cost a read
    /// per reload) — and, being about content, it dies with the bytes it was
    /// read from.
    #[test]
    fn a_file_cached_as_not_a_session_reaches_the_board_once_it_gains_a_cwd() {
        let root = unique_temp_dir("sidecar-grows-a-cwd");
        let dir = root.join("-Users-me-project-alpha");
        std::fs::create_dir_all(&dir).expect("create encoded-cwd dir");
        let path = dir.join("s1.jsonl");
        // A real sidecar: parses fine, carries no `cwd` anywhere.
        std::fs::write(&path, r#"{"type":"summary","summary":"Sidecar title"}"#).expect("write");

        let mut store = SessionStore::new(&root);
        assert!(
            store.reload_at(settled_now()).sessions.is_empty(),
            "no cwd, no session"
        );
        assert_eq!(store.last_parsed(), 1, "the cold reload reads it");

        assert!(store.reload_at(settled_now()).sessions.is_empty());
        assert_eq!(
            store.last_parsed(),
            0,
            "the verdict is cached — not re-reading a sidecar is what it buys"
        );

        // The file then becomes a real session: claude writes the first record
        // carrying a `cwd`.
        std::fs::write(
            &path,
            session_line("s1", "now a session", "2026-01-01T00:00:00Z"),
        )
        .expect("rewrite as a session");
        let reload = store.reload_at(settled_now());
        let ids: Vec<&str> = reload
            .sessions
            .iter()
            .map(|s| s.session_id.as_str())
            .collect();
        assert_eq!(
            ids,
            ["s1"],
            "a cached not-a-session verdict must never outlive the bytes it read"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The settle window, end to end on the real clock: files written this
    /// instant are re-read on EVERY reload, however well their stamps match.
    #[test]
    fn a_file_written_this_instant_is_re_read_on_the_next_reload() {
        let root = unique_temp_dir("settle");
        write_session(&root, "-Users-me-project-alpha", "s1", "first");

        let mut store = SessionStore::new(&root);
        store.reload();
        assert_eq!(store.last_parsed(), 1);

        // No sleep, no write: the file is simply still inside the window.
        store.reload();
        assert_eq!(
            store.last_parsed(),
            1,
            "a file younger than the settle window is never reused"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
