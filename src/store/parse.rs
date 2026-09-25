//! Fail-soft JSONL parsing.
//!
//! Streams each session file line-by-line as `serde_json::Value` (never
//! hard-typed structs, so schema drift can never be fatal): unparseable lines
//! and non-object values are skipped, one bad file is skipped rather than
//! aborting the scan. Extracts `cwd` and `sessionId` from INSIDE the file
//! (never decoded from the folder name); falls back `session_id` to the file
//! stem. Any file with no `cwd` is dropped (sidecar agent-name/ai-title files
//! are not resumable).
//!
//! Skipping is never SILENT about its reason: [`parse_file`] answers a
//! three-way [`FileVerdict`], because "read it, and it is not a session" and
//! "could not read it" are different facts and only the first is a statement
//! about the file. Fail-soft is unchanged either way — both yield no row and
//! neither can panic.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde_json::Value;

use super::label;

/// SAFETY CEILING on the per-session searchable transcript text — a bound
/// against one pathological file, NOT a working memory budget. 1 MB is ~4x the
/// largest readable transcript measured in a real store, so no genuine session
/// reaches it.
///
/// It WAS a 64 KB budget, and the difference is not academic: this buffer keeps
/// the OLDEST bytes, so whatever the bound cuts is the most RECENT work. At
/// 64 KB the cut was routine rather than pathological — it landed on the p90 of
/// the corpus and hid ~15% of everything ever typed from content search. Sizing
/// the bound off the corpus is what makes it affordable: total indexed bytes are
/// bounded by the CONTENT, never by `sessions x cap`. Distribution and the
/// memory it costs: `docs/agents/DOMAIN.md#content-index-storeparse`.
///
/// KEEP the ceiling. Without one a single runaway file would define the store's
/// memory. If the store grows into the thousands of sessions, that is the
/// boundary to move to an on-disk cache.
pub const CONTENT_INDEX_CAP: usize = 1024 * 1024;

/// The `sessionKind` discriminant Claude Code stamps on the records of a
/// BACKGROUND session's transcript.
///
/// `sessionKind` is a TOP-LEVEL envelope key on ordinary records (`user`,
/// `assistant`, `attachment`, `system`) — NOT a record type of its own. Across the
/// records observed in a real store, `"bg"` is the ONLY value it ever takes; a
/// foreground session simply omits the key. That is an OBSERVATION, not a contract:
/// the key is undocumented upstream, so the read stays FAIL-SOFT — anything that is
/// not exactly this string (absent, null, a number, a different word) leaves the
/// file non-background rather than producing a verdict.
pub const SESSION_KIND_BACKGROUND: &str = "bg";

/// The `origin.kind` Claude Code stamps on the `user` record it SYNTHESIZES to
/// deliver a background task's `<task-notification>` into the session that
/// launched it.
///
/// `origin` is undocumented upstream. It is an object whose `kind` names who
/// wrote the record; the other kinds observed in a real store are
/// [`ORIGIN_KIND_HUMAN`] and `"peer"` (a message from another agent). The read is
/// FAIL-SOFT: see [`OriginKind`].
const ORIGIN_KIND_TASK_NOTIFICATION: &str = "task-notification";

/// The `origin.kind` of a record a PERSON wrote into the session.
///
/// The only kind that may clear the failed-task flag besides an absent `origin`:
/// every other kind is a machine speaking (see [`task_signal`]).
const ORIGIN_KIND_HUMAN: &str = "human";

/// The `<status>` a task notification carries when the background task FAILED —
/// the one value that raises the flag.
///
/// Compared EXACTLY, like [`SESSION_KIND_BACKGROUND`]: `completed`, `stopped`,
/// `killed`, a missing tag and any other spelling raise nothing. `stopped` and
/// `killed` are deliberately out of scope, and a `completed` notice does not
/// CLEAR the flag either — only the user writing does.
const TASK_STATUS_FAILED: &str = "failed";

/// The `promptSource` values that mean the USER wrote the record: `typed` at the
/// prompt, `sdk` from a `claude -p` quick reply.
///
/// NEVER sufficient on its own: nearly a third of the task notifications in a real
/// store (51 of 169) ALSO carry `sdk`, which is why [`task_signal`] rules on `origin`
/// before it ever reads this.
const USER_PROMPT_SOURCES: [&str; 2] = ["typed", "sdk"];

/// The `turnOrigin` values that mean the USER wrote the record: `human` at the
/// prompt, `sdk` from a quick reply.
///
/// Read because `promptSource` is not always there: a slash command sent as a
/// quick reply (`/pr-squash`, `/handoff-to-lead`, `/cr-review` in a real store)
/// carries ONLY `turnOrigin: "sdk"`, and it is still the user engaging. Claude Code
/// writes that marker from 2.1.278 on; one sent by an older version carries none,
/// so it does not clear — a leftover marker, the direction the flag accepts.
const USER_TURN_ORIGINS: [&str; 2] = ["human", "sdk"];

/// The `<status>` tag of a task notification's body, as its `(open, close)` pair.
const STATUS_TAG: (&str, &str) = ("<status>", "</status>");

/// The `<summary>` tag of a task notification's body — claude's own one-line
/// account of what happened, which the board quotes WORD FOR WORD. Optional: a
/// `failed` notice without one still raises the flag ([`failed_notice`]).
const SUMMARY_TAG: (&str, &str) = ("<summary>", "</summary>");

/// What one candidate file turned out to be — the three answers a fail-soft read
/// can give, kept apart because only TWO of them are statements about the file.
///
/// [`NotASession`](Self::NotASession) is a verdict about CONTENT: bytes were read
/// end to end and carried no `cwd`. It stays true for exactly as long as those
/// bytes do not move, so a caller that keys on the bytes may CACHE it.
///
/// [`Unreadable`](Self::Unreadable) is a fact about the read ATTEMPT — EMFILE, a
/// permissions blip, a network home directory that blinked — and says nothing
/// whatever about the file. It must NEVER be cached: a cache keyed on
/// `(mtime, len)` would re-serve it for as long as the file sits still, and a
/// finished transcript's stamp never moves again, so one transient error would
/// become a session missing for the life of the process. Collapsing the two into
/// an `Option` is what makes that state representable at all, which is why this
/// type exists rather than a flag threaded alongside one.
///
/// Generic over the payload so the distinction SURVIVES the derivation step above
/// it (`ParsedFile` -> `store::Session`) instead of being flattened back to an
/// `Option` and re-invented one layer up.
pub enum FileVerdict<T> {
    /// Read end to end, and it carries a `cwd`: a resumable session.
    Session(T),
    /// Read end to end, and it carries NO `cwd`: a sidecar agent-name/ai-title
    /// file, which is not resumable. CACHEABLE — it describes the bytes.
    NotASession,
    /// The bytes could not be read. NOT cacheable, at any level.
    Unreadable,
}

impl<T> FileVerdict<T> {
    /// Re-wrap the payload, carrying both non-session verdicts through unchanged.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> FileVerdict<U> {
        match self {
            FileVerdict::Session(payload) => FileVerdict::Session(f(payload)),
            FileVerdict::NotASession => FileVerdict::NotASession,
            FileVerdict::Unreadable => FileVerdict::Unreadable,
        }
    }

    /// The payload, if the file is a session.
    ///
    /// For callers that cannot ACT on the distinction: the hand-off re-reads
    /// (`resume::read_authoritative`, `send::plan_send`) refuse on either
    /// non-session verdict, and refusing is the fail-soft direction for both.
    /// They cache nothing, so there is nothing there to latch.
    pub fn session(self) -> Option<T> {
        match self {
            FileVerdict::Session(payload) => Some(payload),
            FileVerdict::NotASession | FileVerdict::Unreadable => None,
        }
    }
}

/// The raw fields extracted from one JSONL file in a single streaming pass.
///
/// Derivation (label, repo, timestamp parsing) happens above this in
/// `SessionStore`; this struct only carries what a single fail-soft scan can
/// read straight out of the file.
pub struct ParsedFile {
    /// `cwd` read from inside the file (first non-null). Guaranteed present:
    /// files with no `cwd` answer [`FileVerdict::NotASession`] instead.
    pub cwd: String,
    /// `sessionId` from inside the file (first non-null), else the file stem.
    pub session_id: String,
    /// `gitBranch` from inside the file (last non-null).
    pub git_branch: Option<String>,
    /// `timestamp` from inside the file (last non-null), unparsed RFC 3339.
    pub timestamp_raw: Option<String>,
    /// Latest `type:"summary"` title, if any.
    pub summary: Option<String>,
    /// First "real" user prompt, if any (see [`label::user_prompt_text`]).
    pub first_user: Option<String>,
    /// `uuid` of the record whose `parentUuid` is JSON `null` — the transcript
    /// TREE's root, which identifies a fork lineage (a background hand-off copies
    /// the whole leading prefix, root included, into the new session file).
    ///
    /// NOT the first `user`/`assistant` uuid: the root is an `attachment` in the
    /// large majority of real files (hook-injected context precedes the first
    /// prompt), and anchoring on the first message misses real forks whose
    /// leading prompt differs while the conversation is identical. `None` when
    /// the file has no null-parent record — fail-soft, meaning "no lineage",
    /// never a dropped session.
    pub root_uuid: Option<String>,
    /// How many conversation TURNS the file holds: records typed `user` or
    /// `assistant`.
    ///
    /// Counted in the streaming pass, NEVER derived from [`content_index`]:
    /// that buffer stops at [`CONTENT_INDEX_CAP`], so a long session's turns
    /// would silently stop being counted at that ceiling. This is a real counter
    /// over every record, so the ceiling cannot reach it.
    ///
    /// Deliberately a NARROWER set than the four tree types [`root_uuid`]
    /// reasons about — do not unify the two. See the counting site in
    /// [`parse_file`] for why.
    ///
    /// 0 when the file holds no turns — fail-soft, like every field here.
    ///
    /// [`content_index`]: ParsedFile::content_index
    /// [`root_uuid`]: ParsedFile::root_uuid
    pub msg_count: usize,
    /// Capped, readable transcript text for content search.
    pub content_index: String,
    /// Whether ANY record carried [`SESSION_KIND_BACKGROUND`] — i.e. this
    /// transcript belongs to a background job rather than an interactive session.
    ///
    /// Presence, not the value: the key has exactly one observed value, so
    /// carrying the string would store the same byte 8000 times per file to answer
    /// a yes/no question.
    pub background: bool,
    /// Whether the file carried an `agent-name` record naming a non-blank title.
    ///
    /// `agentName` is the background job's own name and is FREE-FORM — measured
    /// over a real store it holds things like `"bugsnag nextjs ssr integration"`,
    /// never an agent handle. It therefore says an agent was NAMED here, never
    /// which one, which is precisely why it cannot decide a flag by itself.
    pub has_agent_name: bool,
    /// Whether the file carried an `agent-setting` record naming a non-blank
    /// handle.
    ///
    /// `agentSetting` is the interactive BIND and is a clean handle (`"lead"`,
    /// `"technical-brainstormer"` in a real store). This is the binding whose LOSS
    /// on a background fork is the anthropics/claude-code#80811 signature.
    pub has_agent_setting: bool,
    /// The last `failed` background-task notice the user has NOT written into this
    /// session since, or `None`.
    ///
    /// Decided record by record in file order by [`task_signal`]: a `failed`
    /// notice raises it (a later one REPLACES an earlier one), the user's next
    /// prompt — typed or a quick reply — clears it, and every other record leaves
    /// it alone. A fact about the BYTES like every other field here, so the parse
    /// cache carries it unchanged.
    pub failed_task: Option<FailedTaskNotice>,
}

/// A background task's `failed` notice, as the pass found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedTaskNotice {
    /// The notice's `<summary>`, VERBATIM: the tag's inner text, neither trimmed
    /// nor rewritten, because the board quotes claude's words rather than
    /// paraphrasing them.
    ///
    /// EMPTY when the notice carried no readable `<summary>` (absent, or never
    /// closed) as well as when it carried an empty one: the two are the same fact
    /// to the board — nothing to quote — so one representation serves both.
    pub summary: String,
    /// The notice record's own `timestamp`, unparsed RFC 3339 like
    /// [`ParsedFile::timestamp_raw`] — parsing is derivation, and derivation
    /// happens above this module. `None` when the record carries none.
    pub timestamp_raw: Option<String>,
}

/// Stream one JSONL file fail-soft and say what it is.
///
/// Three-way on purpose (see [`FileVerdict`]): a file that carries no `cwd` is
/// NOT a session and that is a durable fact about it, whereas a file that could
/// not be read yields no fact at all. Both produce no row and neither can panic
/// — the distinction exists for what a CALLER may remember, not for what it
/// shows.
pub fn parse_file(path: &Path) -> FileVerdict<ParsedFile> {
    let Ok(file) = File::open(path) else {
        return FileVerdict::Unreadable;
    };
    let reader = BufReader::new(file);

    let mut cwd: Option<String> = None;
    let mut session_id: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut timestamp_raw: Option<String> = None;
    let mut summary: Option<String> = None;
    let mut first_user: Option<String> = None;
    let mut root_uuid: Option<String> = None;
    let mut msg_count: usize = 0;
    let mut content_index = String::new();
    let mut background = false;
    let mut has_agent_name = false;
    let mut has_agent_setting = false;
    let mut failed_task: Option<FailedTaskNotice> = None;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            // Two very different failures arrive on this one arm, and telling
            // them apart is the SAME distinction [`FileVerdict`] draws, one
            // level down.
            //
            // `BufRead::lines` reports non-UTF-8 bytes as `InvalidData`. That is
            // a fact about the CONTENT — one malformed line — so it is skipped,
            // exactly like an unparseable JSON line below. Bailing here would
            // drop a whole real transcript over one bad byte sequence, which is
            // the fail-soft rule inverted.
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => continue,
            // Anything else is the READ failing part-way: EIO/ESTALE on a
            // network home directory, EISDIR on a path that is not a file. That
            // is no verdict about content at all, so skipping the line would
            // hand back a TRUNCATED parse whose `msg_count` / `timestamp` /
            // `content_index` the caller would then cache as the session's
            // authoritative shape. Skipping is also unbounded: a reader that
            // errors persistently never reaches EOF, so `continue` spins
            // forever. Give no verdict instead.
            Err(_) => return FileVerdict::Unreadable,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // One malformed line is skipped; the rest of the file still parses.
        let record: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !record.is_object() {
            continue;
        }

        // cwd + sessionId: first non-null (authoritative, from inside the file).
        if cwd.is_none() {
            if let Some(c) = record.get("cwd").and_then(Value::as_str) {
                cwd = Some(c.to_string());
            }
        }
        if session_id.is_none() {
            if let Some(s) = record.get("sessionId").and_then(Value::as_str) {
                session_id = Some(s.to_string());
            }
        }
        // Lineage root: the FIRST record in file order whose `parentUuid` is JSON
        // null. The transcript is a TREE and this is its root, so it is copied
        // verbatim into every fork of the session — which is what makes it a
        // stable lineage identity. Deliberately NOT filtered by `type`: the root
        // is usually an `attachment` (hook-injected context), only sometimes a
        // `user` message.
        //
        // `get` distinguishes the two cases this depends on: an ABSENT
        // `parentUuid` yields `None` (records outside the tree — `last-prompt`,
        // `mode`, `agent-setting`, `permission-mode`, `file-history-snapshot` —
        // carry no such key and must never be mistaken for the root), whereas the
        // root yields `Some(Value::Null)`. A null-parent record with no readable
        // `uuid` is skipped rather than latching, so a later one can still win
        // (fail-soft). A file may carry more than one null-parent record (a
        // forest); the first wins, deterministically.
        if root_uuid.is_none() && record.get("parentUuid").is_some_and(Value::is_null) {
            if let Some(u) = record.get("uuid").and_then(Value::as_str) {
                root_uuid = Some(u.to_string());
            }
        }
        // Conversation TURNS: `user` + `assistant` records, counted here in the
        // one existing pass — no second read, no allocation, and nothing to
        // invalidate.
        //
        // This is deliberately a DIFFERENT set from the four types the root
        // logic above reasons about (`user`, `assistant`, `attachment`,
        // `system`). Those four are what carry `uuid`/`parentUuid` and so form
        // the TREE; roughly a quarter of it is not conversation at all —
        // hook-injected `attachment` context and `system` notices, which nobody
        // typed and claude did not answer. Counting them would inflate a stub
        // that holds no work into something that looks like it does, which is
        // the exact question this number exists to answer. The two notions are
        // separate on purpose: do NOT unify them into one "tree record" test.
        //
        // `Value` access throughout, so a missing or non-string `type` simply
        // does not count rather than panicking (FAIL-SOFT).
        if matches!(
            record.get("type").and_then(Value::as_str),
            Some("user") | Some("assistant")
        ) {
            msg_count += 1;
        }
        // The three facts the #80811 board badge is derived from, read in THIS
        // pass rather than a second one over the same bytes.
        //
        // All three are PRESENCE flags, and all three are undocumented, so each
        // read is FAIL-SOFT by construction: `get` + `as_str` yields `None` for an
        // absent, null, or wrongly-typed key, and no arm can panic or reject the
        // file. An unrecognised shape therefore costs the badge, never the session.
        //
        // `background` latches on the ENVELOPE key `sessionKind`, which rides on
        // ordinary records and is compared against the one observed discriminant
        // ([`SESSION_KIND_BACKGROUND`]) rather than merely tested for presence —
        // a future second value must not silently read as "background".
        if !background
            && record.get("sessionKind").and_then(Value::as_str) == Some(SESSION_KIND_BACKGROUND)
        {
            background = true;
        }
        // The other two are RECORD TYPES carrying one field each. Blank-after-trim
        // counts as absent, matching how `preview::trimmed_field` already reads
        // these exact two fields — the surfaces must not disagree about whether an
        // agent was named.
        match record.get("type").and_then(Value::as_str) {
            Some("agent-name") if !has_agent_name => {
                has_agent_name = has_trimmed_field(&record, "agentName");
            }
            Some("agent-setting") if !has_agent_setting => {
                has_agent_setting = has_trimmed_field(&record, "agentSetting");
            }
            _ => {}
        }
        // The failed-background-task flag, decided in THIS pass and in FILE ORDER,
        // which is what "later" means for both halves of the rule: a later failed
        // notice replaces an earlier one, and the user's next prompt clears
        // whatever is standing. The whole decision is the pure `task_signal`; this
        // arm only applies it.
        match task_signal(&record) {
            TaskSignal::Failed(notice) => failed_task = Some(notice),
            TaskSignal::UserTurn => failed_task = None,
            TaskSignal::Neither => {}
        }
        // gitBranch + timestamp: last non-null (most-recent activity wins).
        if let Some(b) = record.get("gitBranch").and_then(Value::as_str) {
            git_branch = Some(b.to_string());
        }
        if let Some(t) = record.get("timestamp").and_then(Value::as_str) {
            timestamp_raw = Some(t.to_string());
        }
        // Label sources: latest summary, first real user prompt.
        if let Some(s) = label::summary_text(&record) {
            summary = Some(s);
        }
        if first_user.is_none() {
            if let Some(u) = label::user_prompt_text(&record) {
                first_user = Some(u);
            }
        }
        // Searchable transcript text, accumulated up to the cap.
        if content_index.len() < CONTENT_INDEX_CAP {
            append_readable(&record, &mut content_index);
        }
    }

    // Read end to end, and no cwd anywhere in it => not a resumable session.
    // A VERDICT about this file, reached only on the path that saw every line.
    let Some(cwd) = cwd else {
        return FileVerdict::NotASession;
    };
    let session_id = session_id.unwrap_or_else(|| file_stem(path));
    truncate_on_char_boundary(&mut content_index, CONTENT_INDEX_CAP);

    FileVerdict::Session(ParsedFile {
        cwd,
        session_id,
        git_branch,
        timestamp_raw,
        summary,
        first_user,
        root_uuid,
        msg_count,
        content_index,
        background,
        has_agent_name,
        has_agent_setting,
        failed_task,
    })
}

/// What one record says about the failed-background-task flag
/// ([`ParsedFile::failed_task`]).
#[derive(Debug, PartialEq, Eq)]
enum TaskSignal {
    /// A background task's `failed` notice: raise the flag, REPLACING any earlier
    /// notice.
    Failed(FailedTaskNotice),
    /// The user wrote into this session — typed at the prompt or sent as a quick
    /// reply: clear the flag.
    UserTurn,
    /// Anything else leaves the flag exactly as it was.
    Neither,
}

/// A `user` record's `origin.kind`, read FAIL-SOFT.
#[derive(Debug)]
enum OriginKind<'a> {
    /// No `origin` key at all — the shape of a quick reply, and of the tool
    /// results and slash-command wrappers claude writes for itself.
    Absent,
    /// `origin` is an object carrying a string `kind`.
    Kind(&'a str),
    /// `origin` is present but not in that shape: `null`, not an object, or an
    /// object with no string `kind`. Neither raises nor clears anything.
    Unrecognised,
}

/// Read `record.origin.kind` into an [`OriginKind`].
fn origin_kind(record: &Value) -> OriginKind<'_> {
    match record.get("origin") {
        None => OriginKind::Absent,
        Some(Value::Object(origin)) => match origin.get("kind").and_then(Value::as_str) {
            Some(kind) => OriginKind::Kind(kind),
            None => OriginKind::Unrecognised,
        },
        Some(_) => OriginKind::Unrecognised,
    }
}

/// Classify one record for the failed-background-task flag. Pure.
///
/// Only a `user` record that is the session's OWN turn ([`label::is_sidechain`],
/// the store's one "not from a subagent" rule) can say anything. Then `origin`
/// is ruled on FIRST, before any other marker is read:
///
/// - [`ORIGIN_KIND_TASK_NOTIFICATION`] is a notice. It raises the flag when its
///   `<status>` is exactly [`TASK_STATUS_FAILED`], whether or not it carries a
///   `<summary>` ([`failed_notice`]); otherwise it is [`TaskSignal::Neither`]. A
///   notice NEVER clears — not even a `completed` one.
/// - An ABSENT `origin`, or [`ORIGIN_KIND_HUMAN`], MAY be the user writing. It is
///   when the record is not an `isMeta` injection and not a tool result, and it
///   carries at least one of the three markers: `origin.kind` [`ORIGIN_KIND_HUMAN`]
///   itself, a `promptSource` in [`USER_PROMPT_SOURCES`], or a `turnOrigin` in
///   [`USER_TURN_ORIGINS`].
/// - Any other kind (`"peer"` — another agent's message) and any unrecognised
///   `origin` shape are machine speech: [`TaskSignal::Neither`].
///
/// Why `origin` goes first: task notifications carry `promptSource: "sdk"` too —
/// the very value a quick reply carries — so a `promptSource`-only rule would let a
/// later notice count as the user's reply and silently clear the flag. Why
/// `turnOrigin` counts: a slash command sent as a quick reply carries ONLY
/// `turnOrigin: "sdk"` — from the Claude Code version [`USER_TURN_ORIGINS`] names
/// on; one sent by an older version carries nothing. What else carries NONE of the
/// markers, and so never clears, is exactly what claude writes on its own: a
/// slash-command wrapper with no marker (`/exit`), its `<local-command-stdout>`
/// output, and the `[Request interrupted…]` line.
///
/// FAIL-SOFT throughout: an unrecognised shape neither raises nor clears. So a
/// degraded record can leave a stale marker behind but cannot hide a failure that
/// is standing — the direction the feature's trade-off picked — with two
/// exceptions, both erring the other way. `isSidechain` is read fail-OPEN: a
/// non-bool value is the session's own turn ([`label::is_sidechain`]), so a record
/// malformed there is judged on its other fields and, if it carries a user marker,
/// DOES clear. The real store never writes one: sub-agent turns live in files
/// discovery never reaches, and the depth-2 records it does reach carry a bool.
/// And a notice whose body is not a string, or whose `<status>` cannot be read,
/// raises nothing ([`failed_notice`]), so that failure goes unflagged. A missing
/// `<summary>` is not such a case: the status alone raises the flag.
fn task_signal(record: &Value) -> TaskSignal {
    if record.get("type").and_then(Value::as_str) != Some("user") || label::is_sidechain(record) {
        return TaskSignal::Neither;
    }
    match origin_kind(record) {
        OriginKind::Kind(ORIGIN_KIND_TASK_NOTIFICATION) => {
            failed_notice(record).map_or(TaskSignal::Neither, TaskSignal::Failed)
        }
        OriginKind::Absent => user_turn_signal(record, false),
        OriginKind::Kind(ORIGIN_KIND_HUMAN) => user_turn_signal(record, true),
        OriginKind::Kind(_) | OriginKind::Unrecognised => TaskSignal::Neither,
    }
}

/// The clearing half of [`task_signal`], for a record whose `origin` is absent
/// (`human == false`) or [`ORIGIN_KIND_HUMAN`] (`human == true`).
fn user_turn_signal(record: &Value, human: bool) -> TaskSignal {
    if is_meta(record) || is_tool_result(record) {
        return TaskSignal::Neither;
    }
    let marked = human
        || has_marker(record, "promptSource", &USER_PROMPT_SOURCES)
        || has_marker(record, "turnOrigin", &USER_TURN_ORIGINS);
    if marked {
        TaskSignal::UserTurn
    } else {
        TaskSignal::Neither
    }
}

/// Whether `record[key]` is a string naming one of `values`. FAIL-SOFT: an
/// absent, null or non-string value is no marker.
fn has_marker(record: &Value, key: &str, values: &[&str]) -> bool {
    record
        .get(key)
        .and_then(Value::as_str)
        .is_some_and(|value| values.contains(&value))
}

/// Whether `record` is an `isMeta` injection — context claude adds on the user's
/// behalf (skill bodies, caveats), which nobody typed.
///
/// FAIL-SOFT toward "meta": only an ABSENT `isMeta` or a literal `false` reads
/// as an ordinary record, so an unrecognised value can never clear the flag.
fn is_meta(record: &Value) -> bool {
    !matches!(record.get("isMeta"), None | Some(Value::Bool(false)))
}

/// Whether `record` is a TOOL RESULT: a `user` record whose `message.content` is
/// a block array holding a `tool_result` block. Claude writes these for itself
/// after every tool call, so one never counts as the user engaging.
fn is_tool_result(record: &Value) -> bool {
    record
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        })
}

/// The `failed` notice a task-notification record carries, or `None`.
///
/// `None` unless `message.content` is a STRING whose `<status>` is exactly
/// [`TASK_STATUS_FAILED`] — a missing or unclosed `<status>` or a non-string body
/// reads as an unrecognised shape and raises nothing, the flag's one SET-side miss
/// (see [`task_signal`]).
///
/// The status ALONE decides; the `<summary>` is only the words the banner quotes.
/// A notice with no readable summary — the tag absent, or opened and never closed
/// — keeps the EMPTY summary, exactly what an empty `<summary></summary>` already
/// gives, so it still raises the flag and the banner simply quotes nothing.
/// Dropping a failure whose status was read, over a missing account of it, would
/// err toward a hidden failure — the one direction the flag refuses.
fn failed_notice(record: &Value) -> Option<FailedTaskNotice> {
    let body = record
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)?;
    if tag_inner(body, STATUS_TAG)? != TASK_STATUS_FAILED {
        return None;
    }
    Some(FailedTaskNotice {
        summary: tag_inner(body, SUMMARY_TAG).unwrap_or_default().to_string(),
        timestamp_raw: record
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// The text between the FIRST `open` tag in `body` and the first `close` after
/// it, exactly as written — or `None` when either is missing.
///
/// First wins because claude writes the envelope's own tags (`<status>`,
/// `<summary>`) AHEAD of the free-form `<result>` it closes with, so a result
/// that happens to quote a tag cannot take precedence.
fn tag_inner<'a>(body: &'a str, (open, close): (&str, &str)) -> Option<&'a str> {
    let start = body.find(open)? + open.len();
    let rest = &body[start..];
    Some(&rest[..rest.find(close)?])
}

/// Whether `record[key]` is a string with non-whitespace content.
///
/// The presence half of `preview::trimmed_field`, which reads these same two
/// undocumented agent fields for the preview's `@handle`. Kept to the identical
/// rule — a string, trimmed, non-empty — so the badge and the preview can never
/// disagree about whether a record named an agent at all. FAIL-SOFT: an absent,
/// null, or non-string value is simply `false`.
fn has_trimmed_field(record: &Value, key: &str) -> bool {
    record
        .get(key)
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
}

/// The filename without its `.jsonl` extension (the session id in the store
/// layout `<encoded-cwd>/<session-id>.jsonl`). This is the *filename*, never the
/// encoded folder name.
fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Append the readable text of a user/assistant/summary record to the search
/// index. Text blocks only (tool params/thinking are omitted to keep the index
/// readable — and that omission, not the ceiling, is what keeps it a small
/// fraction of the raw bytes); breadth is bounded by [`CONTENT_INDEX_CAP`].
fn append_readable(record: &Value, buf: &mut String) {
    let text = match record.get("type").and_then(Value::as_str) {
        Some("user") | Some("assistant") => record
            .get("message")
            .and_then(|m| m.get("content"))
            .map(readable_text)
            .unwrap_or_default(),
        Some("summary") => record
            .get("summary")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_default(),
        _ => String::new(),
    };
    if text.is_empty() {
        return;
    }
    if !buf.is_empty() {
        buf.push('\n');
    }
    buf.push_str(&text);
}

/// Extract plain readable text (string, or the `text` blocks of a typed-block
/// array joined with newlines) from a `message.content` value.
fn readable_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Truncate `s` to at most `max` bytes without splitting a UTF-8 codepoint.
fn truncate_on_char_boundary(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// An isolated temp dir for a hand-written transcript (PATTERNS: never touch
    /// the real `~/.claude/projects`).
    fn unique_temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the unix epoch")
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "snapback-parse-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Write `lines` as a `<id>.jsonl` transcript and run the real streaming
    /// parse over it, so these tests exercise `parse_file`'s loop (including its
    /// first-wins ordering) rather than a re-implementation of it.
    fn parse_lines(tag: &str, lines: &[&str]) -> Option<ParsedFile> {
        let dir = unique_temp_dir(tag);
        let file = dir.join(format!("sess-{tag}.jsonl"));
        std::fs::write(&file, lines.join("\n")).expect("write transcript");
        let parsed = parse_file(&file).session();
        std::fs::remove_dir_all(&dir).ok();
        parsed
    }

    #[test]
    fn a_file_with_no_cwd_is_not_a_session_rather_than_unreadable() {
        // The sidecar verdict, stated as itself. This is the ONLY non-session
        // answer a caller may remember, so it must not be reachable from a file
        // that simply could not be read.
        let dir = unique_temp_dir("sidecar-verdict");
        let file = dir.join("sidecar.jsonl");
        std::fs::write(&file, r#"{"type":"summary","summary":"Sidecar title"}"#)
            .expect("write sidecar");

        assert!(
            matches!(parse_file(&file), FileVerdict::NotASession),
            "a file read end to end with no cwd is a verdict about the file"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- the three #80811 facts --------------------------------------------

    /// The real on-disk shape, as measured in a live store: `sessionKind` is a
    /// TOP-LEVEL key on an ordinary record (not a record type), while the two
    /// agent facts are their own record types carrying one field each.
    #[test]
    fn reads_the_background_and_agent_facts_from_one_pass() {
        let parsed = parse_lines(
            "agent-facts",
            &[
                r#"{"type":"user","cwd":"/repo","sessionId":"s","sessionKind":"bg"}"#,
                r#"{"type":"agent-name","agentName":"pr #152 isolated review","sessionId":"s"}"#,
                r#"{"type":"agent-setting","agentSetting":"lead","sessionId":"s"}"#,
            ],
        )
        .expect("a file with a cwd is a session");

        assert!(parsed.background, "sessionKind:bg marks a background job");
        assert!(parsed.has_agent_name, "an agent-name record names a job");
        assert!(
            parsed.has_agent_setting,
            "an agent-setting record binds one"
        );
    }

    /// A foreground transcript omits `sessionKind` entirely, and a job that never
    /// bound an agent carries no `agent-setting`. Absence must read as `false`,
    /// not as "unknown, assume yes".
    #[test]
    fn absent_background_and_agent_keys_read_as_false() {
        let parsed = parse_lines(
            "agent-facts-absent",
            &[r#"{"type":"user","cwd":"/repo","sessionId":"s"}"#],
        )
        .expect("a file with a cwd is a session");

        assert!(!parsed.background);
        assert!(!parsed.has_agent_name);
        assert!(!parsed.has_agent_setting);
    }

    /// FAIL-SOFT over every malformed shape these three undocumented keys can
    /// take — null, a number, an empty/blank string, a wrong-typed `sessionKind`,
    /// and the key simply missing from its own record type. Each must yield NO
    /// fact and NO panic: an unrecognised shape costs the badge, never the
    /// session. Mirrors the malformed-agent fixtures in `preview.rs`.
    #[test]
    fn malformed_background_and_agent_records_yield_no_fact() {
        let malformed = [
            // sessionKind: wrong type, wrong value, null.
            r#"{"type":"user","cwd":"/repo","sessionKind":42}"#,
            r#"{"type":"user","cwd":"/repo","sessionKind":null}"#,
            r#"{"type":"user","cwd":"/repo","sessionKind":""}"#,
            r#"{"type":"user","cwd":"/repo","sessionKind":"BG"}"#,
            r#"{"type":"user","cwd":"/repo","sessionKind":"interactive"}"#,
            r#"{"type":"user","cwd":"/repo","sessionKind":["bg"]}"#,
            // agent-name: key missing, null, a number, empty, blank.
            r#"{"type":"agent-name","cwd":"/repo","sessionId":"s"}"#,
            r#"{"type":"agent-name","cwd":"/repo","agentName":null}"#,
            r#"{"type":"agent-name","cwd":"/repo","agentName":42}"#,
            r#"{"type":"agent-name","cwd":"/repo","agentName":""}"#,
            r#"{"type":"agent-name","cwd":"/repo","agentName":"   "}"#,
            // agent-setting: the same five shapes.
            r#"{"type":"agent-setting","cwd":"/repo","sessionId":"s"}"#,
            r#"{"type":"agent-setting","cwd":"/repo","agentSetting":null}"#,
            r#"{"type":"agent-setting","cwd":"/repo","agentSetting":42}"#,
            r#"{"type":"agent-setting","cwd":"/repo","agentSetting":""}"#,
            r#"{"type":"agent-setting","cwd":"/repo","agentSetting":"   "}"#,
            // A null/absent `type`: the agent facts key off the record type, so
            // neither can latch, and the agent field is ignored wherever it sits.
            r#"{"type":null,"cwd":"/repo","agentName":"x","agentSetting":"lead"}"#,
            r#"{"cwd":"/repo","agentName":"x","agentSetting":"lead"}"#,
        ];

        for line in malformed {
            let parsed = parse_lines("agent-facts-malformed", &[line])
                .unwrap_or_else(|| panic!("a file with a cwd is a session: {line}"));
            assert!(
                !parsed.background && !parsed.has_agent_name && !parsed.has_agent_setting,
                "a malformed record must produce no agent fact: {line}"
            );
        }
    }

    /// `sessionKind` is an ENVELOPE key, so it is read independently of `type` —
    /// a record with a null/absent `type` still says the transcript is a
    /// background job, while its stray agent fields are correctly ignored
    /// (they belong to record types, not to the envelope).
    #[test]
    fn background_is_read_independently_of_the_record_type() {
        let parsed = parse_lines(
            "bg-envelope",
            &[r#"{"type":null,"cwd":"/repo","sessionKind":"bg","agentName":"x"}"#],
        )
        .expect("a file with a cwd is a session");

        assert!(parsed.background, "the envelope key stands on its own");
        assert!(
            !parsed.has_agent_name,
            "an agentName outside an agent-name record names nothing"
        );
    }

    /// `sessionKind` rides on ordinary records, so the fact must latch from
    /// ANYWHERE in the file — including a record that carries neither `cwd` nor
    /// any agent field — and must survive later records that omit the key.
    #[test]
    fn background_latches_from_any_record_in_the_file() {
        let parsed = parse_lines(
            "bg-latch",
            &[
                r#"{"type":"user","cwd":"/repo","sessionId":"s"}"#,
                r#"{"type":"attachment","sessionKind":"bg"}"#,
                r#"{"type":"assistant","sessionId":"s"}"#,
            ],
        )
        .expect("a file with a cwd is a session");

        assert!(
            parsed.background,
            "one bg-stamped record anywhere makes the transcript a background job"
        );
    }

    // --- the failed-background-task flag -------------------------------------

    /// A task-notification BODY in claude's real envelope shape: the envelope's
    /// own tags first, the free-form `<result>` last.
    fn notification_body(status: &str, summary: &str) -> String {
        format!(
            "<task-notification>\n<task-id>af4404196ddbcd945</task-id>\n\
             <status>{status}</status>\n<summary>{summary}</summary>\n\
             <result>Now let me verify the internals the review cites.</result>\n\
             </task-notification>"
        )
    }

    /// A `user` record delivering a task notification, as claude writes it
    /// (`promptSource: "system"` is the common case; 51 of 169 real ones say
    /// `"sdk"`, which is the point of several tests below).
    fn notification(status: &str, summary: &str, ts: &str) -> Value {
        serde_json::json!({
            "type": "user", "cwd": "/repo", "sessionId": "s", "timestamp": ts,
            "isSidechain": false, "origin": {"kind": "task-notification"},
            "promptSource": "system",
            "message": {"role": "user", "content": notification_body(status, summary)}
        })
    }

    /// A plain text `user` record carrying exactly the `extra` keys given — the
    /// base every marker test below varies one key of.
    fn user_record(extra: Value) -> Value {
        let mut record = serde_json::json!({
            "type": "user", "cwd": "/repo", "sessionId": "s",
            "timestamp": "2026-09-21T16:00:00.000Z", "isSidechain": false,
            "message": {"role": "user", "content": "keep going"}
        });
        let (Value::Object(base), Value::Object(extra)) = (&mut record, extra) else {
            panic!("both sides of a test record are JSON objects");
        };
        base.extend(extra);
        record
    }

    #[test]
    fn a_failed_notice_raises_the_flag_with_its_summary_verbatim_and_its_time() {
        // Padding, quotes and a doubled space: none of it may be trimmed or
        // rewritten, because the board QUOTES claude rather than paraphrasing.
        let summary = "  Agent \"Fix  the build\" failed: Agent stalled: no progress for 600s  ";
        assert_eq!(
            task_signal(&notification("failed", summary, "2026-09-21T15:30:32.799Z")),
            TaskSignal::Failed(FailedTaskNotice {
                summary: summary.to_string(),
                timestamp_raw: Some("2026-09-21T15:30:32.799Z".to_string()),
            })
        );
    }

    /// Only the exact `failed` status raises the flag. `stopped` and `killed`
    /// are out of scope, `completed` is success, and a near-miss spelling or a
    /// missing tag is an unrecognised shape.
    #[test]
    fn every_status_but_failed_raises_nothing() {
        for status in ["completed", "stopped", "killed", "FAILED", " failed", ""] {
            assert_eq!(
                task_signal(&notification(status, "Agent \"x\" did something", "t")),
                TaskSignal::Neither,
                "status {status:?} must not raise the flag"
            );
        }
        let mut no_status = notification("failed", "Agent \"x\" failed", "t");
        no_status["message"]["content"] = Value::String(
            "<task-notification>\n<summary>Agent \"x\" failed</summary>\n</task-notification>"
                .to_string(),
        );
        assert_eq!(
            task_signal(&no_status),
            TaskSignal::Neither,
            "no <status> tag"
        );
    }

    /// The `<status>` alone raises the flag: a `failed` notice whose `<summary>`
    /// is ABSENT, or opened and never closed, raises it exactly as one carrying an
    /// EMPTY `<summary></summary>` does — with the empty summary, so the banner has
    /// nothing to quote, and with the notice's own time. Reading the status and
    /// then dropping the failure over a missing account of it would err toward a
    /// hidden failure, the one direction the flag's trade-off refuses.
    #[test]
    fn a_failed_notice_without_a_summary_still_raises_the_flag() {
        let body = |summary_part: &str| {
            format!(
                "<task-notification>\n<task-id>af4404196ddbcd945</task-id>\n\
                 <status>failed</status>\n{summary_part}</task-notification>"
            )
        };
        let expected = TaskSignal::Failed(FailedTaskNotice {
            summary: String::new(),
            timestamp_raw: Some("2026-09-21T15:30:32.799Z".to_string()),
        });
        for summary_part in ["", "<summary>cut off\n", "<summary></summary>\n"] {
            let mut record = notification("failed", "x", "2026-09-21T15:30:32.799Z");
            record["message"]["content"] = Value::String(body(summary_part));
            assert_eq!(
                task_signal(&record),
                expected,
                "a failed status must raise the flag whatever {summary_part:?} leaves of \
                 its summary"
            );
        }
    }

    /// FAIL-SOFT on the notice's own body: an unclosed `<status>`, a typed-block
    /// body, a null body and a missing message are all unrecognised shapes — none
    /// raises the flag, and none clears it. (A readable `failed` status with no
    /// readable `<summary>` is NOT one of them: see the test above.)
    #[test]
    fn a_failed_notice_in_an_unrecognised_shape_raises_nothing() {
        let bodies = [
            Value::String("<task-notification>\n<status>failed".to_string()),
            serde_json::json!([{"type": "text", "text": notification_body("failed", "x")}]),
            Value::Null,
        ];
        for body in bodies {
            let mut record = notification("failed", "x", "t");
            record["message"]["content"] = body.clone();
            assert_eq!(
                task_signal(&record),
                TaskSignal::Neither,
                "body {body} must not raise the flag"
            );
        }
        let mut no_message = notification("failed", "x", "t");
        no_message
            .as_object_mut()
            .expect("an object record")
            .remove("message");
        assert_eq!(task_signal(&no_message), TaskSignal::Neither);
    }

    /// Each of the three markers, alone, makes a record the user writing: the
    /// typed prompt (`origin.kind: human`, `promptSource: typed`,
    /// `turnOrigin: human`) and the quick reply (`promptSource: sdk`,
    /// `turnOrigin: sdk`).
    #[test]
    fn each_user_marker_alone_clears_the_flag() {
        for marker in [
            serde_json::json!({"origin": {"kind": "human"}}),
            serde_json::json!({"promptSource": "typed"}),
            serde_json::json!({"promptSource": "sdk"}),
            serde_json::json!({"turnOrigin": "human"}),
            serde_json::json!({"turnOrigin": "sdk"}),
            // `human` with a promptSource that is not a user value is still human.
            serde_json::json!({"origin": {"kind": "human"}, "promptSource": "queued"}),
        ] {
            assert_eq!(
                task_signal(&user_record(marker.clone())),
                TaskSignal::UserTurn,
                "{marker} marks the user writing"
            );
        }
    }

    /// Measured detail 1, pinned at the record level: the SAME `promptSource:
    /// "sdk"` record clears the flag with no `origin` and does NOT once `origin`
    /// names a notification or another agent. A rule that read `promptSource`
    /// first would clear on all three.
    #[test]
    fn origin_is_ruled_on_before_prompt_source() {
        let quick_reply = user_record(serde_json::json!({"promptSource": "sdk"}));
        assert_eq!(task_signal(&quick_reply), TaskSignal::UserTurn);

        for kind in ["task-notification", "peer"] {
            let mut machine = quick_reply.clone();
            machine["origin"] = serde_json::json!({"kind": kind});
            assert_eq!(
                task_signal(&machine),
                TaskSignal::Neither,
                "origin {kind:?} is machine speech whatever promptSource says"
            );
        }
    }

    /// The Never-clears list. Everything claude writes into a session by itself
    /// leaves the flag standing — including the records that carry a user-looking
    /// marker beside the one that disqualifies them.
    #[test]
    fn nothing_claude_writes_on_its_own_clears_the_flag() {
        let never = [
            // A tool result, bare and with a quick-reply marker beside it.
            user_record(serde_json::json!({
                "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}
                ]}
            })),
            user_record(serde_json::json!({
                "promptSource": "sdk",
                "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}
                ]}
            })),
            // An `isMeta` injection, bare and marked.
            user_record(serde_json::json!({"isMeta": true})),
            user_record(serde_json::json!({"isMeta": true, "promptSource": "sdk"})),
            // A later notification — completed, and carrying promptSource: sdk.
            {
                let mut n = notification("completed", "Agent \"y\" completed", "t");
                n["promptSource"] = Value::String("sdk".to_string());
                n
            },
            // Another agent's message, as a real store holds it.
            user_record(serde_json::json!({
                "origin": {"kind": "peer"}, "promptSource": "sdk", "isMeta": true
            })),
            user_record(serde_json::json!({
                "origin": {"kind": "peer"}, "promptSource": "system", "turnOrigin": "peer"
            })),
            // A slash-command wrapper and its output, with no marker.
            user_record(serde_json::json!({
                "message": {"role": "user",
                    "content": "<command-name>/exit</command-name>\n<command-message>exit</command-message>"}
            })),
            user_record(serde_json::json!({
                "message": {"role": "user",
                    "content": "<local-command-stdout>(no content)</local-command-stdout>"}
            })),
            // The interrupt line claude writes by itself.
            user_record(serde_json::json!({
                "message": {"role": "user", "content": [
                    {"type": "text", "text": "[Request interrupted by user]"}
                ]}
            })),
            // A subagent's turn, even one marked as a quick reply.
            user_record(serde_json::json!({"isSidechain": true, "promptSource": "sdk"})),
        ];
        for record in never {
            assert_eq!(
                task_signal(&record),
                TaskSignal::Neither,
                "must neither raise nor clear: {record}"
            );
        }
    }

    /// FAIL-SOFT: every unrecognised shape of the four undocumented fields — and
    /// of the record itself — neither raises nor clears, so a record degraded in
    /// any of these ways can leave a stale marker but never hide a standing
    /// failure. The one field that can, a non-bool `isSidechain`, is
    /// [`task_signal`]'s documented exception.
    #[test]
    fn unrecognised_shapes_neither_raise_nor_clear() {
        let sdk = serde_json::json!("sdk");
        let shapes = [
            // `origin` present but not an object carrying a string `kind`.
            user_record(serde_json::json!({"origin": null, "promptSource": sdk})),
            user_record(serde_json::json!({"origin": "human", "promptSource": sdk})),
            user_record(serde_json::json!({"origin": {}, "promptSource": sdk})),
            user_record(serde_json::json!({"origin": {"kind": 42}, "promptSource": sdk})),
            user_record(serde_json::json!({"origin": {"kind": "HUMAN"}})),
            // `isMeta` that is neither absent nor a literal false.
            user_record(serde_json::json!({"isMeta": "yes", "promptSource": sdk})),
            user_record(serde_json::json!({"isMeta": null, "promptSource": sdk})),
            // Markers of the wrong type or value.
            user_record(serde_json::json!({"promptSource": 42})),
            user_record(serde_json::json!({"promptSource": "SDK"})),
            user_record(serde_json::json!({"promptSource": "system"})),
            user_record(serde_json::json!({"turnOrigin": ["sdk"]})),
            user_record(serde_json::json!({"turnOrigin": "task_notification"})),
            // No marker at all.
            user_record(serde_json::json!({})),
            // Not a `user` record: absent, null and non-string `type`, an
            // assistant turn carrying a user marker, and the `queue-operation`
            // copy claude writes of every notification's body.
            user_record(serde_json::json!({"type": null, "promptSource": sdk})),
            user_record(serde_json::json!({"type": 42, "promptSource": sdk})),
            user_record(serde_json::json!({"type": "assistant", "promptSource": sdk})),
            serde_json::json!({
                "type": "queue-operation", "operation": "enqueue", "sessionId": "s",
                "content": notification_body("failed", "Agent \"x\" failed")
            }),
            serde_json::json!({"cwd": "/repo", "promptSource": sdk}),
        ];
        for record in shapes {
            assert_eq!(
                task_signal(&record),
                TaskSignal::Neither,
                "an unrecognised shape must neither raise nor clear: {record}"
            );
        }
    }

    /// "First occurrence wins" ([`tag_inner`]): claude writes the envelope's own
    /// `<status>` AHEAD of the free-form `<result>`, so a result that happens to
    /// QUOTE a status tag must never take precedence — in either direction.
    #[test]
    fn the_first_status_tag_wins_over_one_quoted_later_in_the_result() {
        let quoting = |status: &str, quoted: &str| {
            format!(
                "<task-notification>\n<status>{status}</status>\n<summary>s</summary>\n\
                 <result>the log said <status>{quoted}</status></result>\n\
                 </task-notification>"
            )
        };
        assert_eq!(
            tag_inner(&quoting("completed", "failed"), STATUS_TAG),
            Some("completed")
        );
        assert_eq!(
            tag_inner(&quoting("failed", "completed"), STATUS_TAG),
            Some("failed")
        );
        // And through the classifier: a completed notice whose result quotes a
        // failure raises nothing.
        let mut record = notification("completed", "s", "t");
        record["message"]["content"] = Value::String(quoting("completed", "failed"));
        assert_eq!(task_signal(&record), TaskSignal::Neither);
    }

    /// Serialize test records as the lines of a transcript.
    fn lines_of(records: &[Value]) -> Vec<String> {
        records.iter().map(Value::to_string).collect()
    }

    /// Run the real streaming pass over `records` and return its flag.
    fn failed_task_after(tag: &str, records: &[Value]) -> Option<FailedTaskNotice> {
        let lines = lines_of(records);
        let borrowed: Vec<&str> = lines.iter().map(String::as_str).collect();
        parse_lines(tag, &borrowed)
            .expect("a file with a cwd is a session")
            .failed_task
    }

    /// "A later failed notice replaces an earlier one" — both the summary AND the
    /// timestamp come from the later notice.
    #[test]
    fn a_later_failed_notice_replaces_an_earlier_one() {
        let flag = failed_task_after(
            "failed-twice",
            &[
                notification(
                    "failed",
                    "Agent \"first\" failed: stalled",
                    "2026-09-21T15:00:00Z",
                ),
                notification(
                    "failed",
                    "Agent \"second\" failed: 403",
                    "2026-09-21T16:00:00Z",
                ),
            ],
        );
        assert_eq!(
            flag,
            Some(FailedTaskNotice {
                summary: "Agent \"second\" failed: 403".to_string(),
                timestamp_raw: Some("2026-09-21T16:00:00Z".to_string()),
            })
        );
    }

    /// A `completed` notice after a failure does not clear it: only the user
    /// writing does (the rejected "clear on a later completion" rule).
    #[test]
    fn a_completed_notice_after_a_failure_leaves_it_standing() {
        let flag = failed_task_after(
            "failed-then-completed",
            &[
                notification(
                    "failed",
                    "Agent \"a\" failed: stalled",
                    "2026-09-21T15:00:00Z",
                ),
                notification("completed", "Agent \"b\" completed", "2026-09-21T16:00:00Z"),
            ],
        );
        assert_eq!(
            flag.map(|notice| notice.summary),
            Some("Agent \"a\" failed: stalled".to_string())
        );
    }

    /// FILE ORDER decides: a prompt clears what stands BEFORE it, and a failure
    /// that arrives after the prompt raises the flag again.
    #[test]
    fn the_users_prompt_clears_only_what_came_before_it() {
        let typed = user_record(serde_json::json!({"promptSource": "typed"}));
        let failed = |summary: &str| notification("failed", summary, "2026-09-21T15:00:00Z");

        assert_eq!(
            failed_task_after("fail-then-prompt", &[failed("a"), typed.clone()]),
            None,
            "the prompt clears the failure before it"
        );
        assert_eq!(
            failed_task_after("prompt-then-fail", &[typed.clone(), failed("b")])
                .map(|notice| notice.summary),
            Some("b".to_string()),
            "a failure after the prompt stands"
        );
        assert_eq!(
            failed_task_after(
                "fail-prompt-fail",
                &[failed("a"), typed.clone(), failed("c")]
            )
            .map(|notice| notice.summary),
            Some("c".to_string()),
            "and a later failure raises the flag again"
        );
        assert_eq!(
            failed_task_after("no-notice", &[typed]),
            None,
            "a transcript with no notice carries no flag"
        );
    }

    #[test]
    fn a_file_that_cannot_be_opened_is_unreadable_not_a_verdict() {
        // The open failure: EMFILE, a permissions blip, a home directory that
        // blinked. Indistinguishable from "no cwd" as an `Option`, and the two
        // must never be confused — one is cacheable and this one is not.
        let dir = unique_temp_dir("open-fails");

        assert!(
            matches!(
                parse_file(&dir.join("not-here.jsonl")),
                FileVerdict::Unreadable
            ),
            "a file that could not be opened says nothing about its content"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The mid-file case, which is the worse one: the open SUCCEEDS and the read
    /// fails part-way, so a skip-the-line policy hands back a truncated parse
    /// whose `msg_count` / `timestamp` the caller would take as authoritative.
    ///
    /// Unix-only because it needs a read that genuinely fails after a successful
    /// open: `open(2)` on a directory succeeds and the first `read(2)` returns
    /// `EISDIR`, which is the portable stand-in for the `EIO`/`ESTALE` a network
    /// home directory gives mid-transcript. (Discovery only ever yields regular
    /// files, so this path is reached from a real store by I/O errors alone.)
    #[cfg(unix)]
    #[test]
    fn a_read_that_fails_mid_file_is_unreadable_not_a_truncated_session() {
        let dir = unique_temp_dir("mid-file-read-error");

        assert!(
            matches!(parse_file(&dir), FileVerdict::Unreadable),
            "a read that died part-way through is not a verdict about content"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_non_utf8_line_is_skipped_rather_than_costing_the_transcript() {
        // The other half of the mid-file distinction, and the reason it is not
        // simply "any line error bails". `BufRead::lines` reports non-UTF-8 bytes
        // as `InvalidData`, which is a fact about the CONTENT — one malformed
        // line — and the fail-soft rule is to skip it. Bailing here would drop a
        // whole real transcript over one bad byte sequence.
        let dir = unique_temp_dir("non-utf8-line");
        let file = dir.join("sess.jsonl");
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(
            br#"{"type":"user","sessionId":"s","cwd":"/w","uuid":"u-1","parentUuid":null,"message":{"content":"first"}}"#,
        );
        bytes.push(b'\n');
        bytes.extend_from_slice(&[0xff, 0xfe, 0xfd]); // not valid UTF-8
        bytes.push(b'\n');
        bytes.extend_from_slice(
            br#"{"type":"assistant","sessionId":"s","cwd":"/w","uuid":"a-1","parentUuid":"u-1","message":{"content":"second"}}"#,
        );
        std::fs::write(&file, bytes).expect("write transcript");

        let parsed = match parse_file(&file) {
            FileVerdict::Session(parsed) => parsed,
            _ => panic!("one non-UTF-8 line must never cost the whole transcript"),
        };
        assert_eq!(
            parsed.msg_count, 2,
            "the records either side of the bad line still count"
        );
        assert_eq!(parsed.cwd, "/w", "and the session itself survives");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The real leading shape of a session file: out-of-tree bookkeeping records
    /// (no `uuid`, no `parentUuid` key at all) ahead of the tree's root.
    const OUT_OF_TREE_PRELUDE: [&str; 2] = [
        r#"{"type":"last-prompt","sessionId":"s","leafUuid":"lp-1"}"#,
        r#"{"type":"mode","sessionId":"s","mode":"default"}"#,
    ];

    #[test]
    fn root_uuid_is_the_null_parent_record_even_when_it_is_an_attachment() {
        // The real shape: hook-injected `attachment` context is the tree root and
        // PRECEDES the first user message, which is why the root uuid — not the
        // first message uuid — is the lineage identity. The two uuids differ here
        // on purpose: a fixture where they agree passes against either key and so
        // cannot distinguish them.
        let mut lines = OUT_OF_TREE_PRELUDE.to_vec();
        lines.extend([
            r#"{"type":"attachment","sessionId":"s","cwd":"/w","uuid":"root-attachment","parentUuid":null,"attachment":{}}"#,
            r#"{"type":"attachment","sessionId":"s","cwd":"/w","uuid":"att-2","parentUuid":"root-attachment","attachment":{}}"#,
            r#"{"type":"user","sessionId":"s","cwd":"/w","uuid":"first-user","parentUuid":"att-2","message":{"role":"user","content":"hi"}}"#,
        ]);
        let parsed = parse_lines("attachment-root", &lines).expect("file has a cwd");

        assert_eq!(parsed.root_uuid.as_deref(), Some("root-attachment"));
        // Pin the correction explicitly: anchoring on the first message is the
        // key this replaces, and it would answer `first-user` here.
        assert_ne!(parsed.root_uuid.as_deref(), Some("first-user"));
    }

    #[test]
    fn a_record_with_no_parent_uuid_key_is_not_the_root() {
        // `parentUuid: null` (the root) vs an ABSENT `parentUuid` (a record
        // outside the tree) are different cases, and `get` is what tells them
        // apart: absent => `None`, root => `Some(Value::Null)`.
        //
        // The two real out-of-tree records below catch an implementation that
        // LATCHES onto the first absent-parent record (root would degrade to
        // `None`). They cannot catch one that treats absent as null and CAPTURES,
        // because no out-of-tree record in the observed store carries a `uuid` to
        // capture — the bug is real but currently latent behind that. The third
        // record models exactly that drift, so the distinction is pinned by
        // behaviour rather than by a record that happens to be unreachable today.
        let mut lines = OUT_OF_TREE_PRELUDE.to_vec();
        lines.extend([
            r#"{"type":"file-history-snapshot","sessionId":"s","uuid":"out-of-tree-uuid"}"#,
            r#"{"type":"attachment","sessionId":"s","cwd":"/w","uuid":"real-root","parentUuid":null,"attachment":{}}"#,
            r#"{"type":"user","sessionId":"s","cwd":"/w","uuid":"u-1","parentUuid":"real-root","message":{"role":"user","content":"hi"}}"#,
        ]);
        let parsed = parse_lines("absent-vs-null", &lines).expect("file has a cwd");

        assert_eq!(parsed.root_uuid.as_deref(), Some("real-root"));
        assert_ne!(
            parsed.root_uuid.as_deref(),
            Some("out-of-tree-uuid"),
            "a record with no parentUuid key sits outside the tree and can never be its root"
        );
    }

    #[test]
    fn the_first_null_parent_record_wins_in_a_forest() {
        // A small minority of real files carry more than one null-parent record.
        // Ordering by file position keeps the answer deterministic.
        let lines = [
            r#"{"type":"attachment","sessionId":"s","cwd":"/w","uuid":"root-first","parentUuid":null,"attachment":{}}"#,
            r#"{"type":"user","sessionId":"s","cwd":"/w","uuid":"u-1","parentUuid":"root-first","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"attachment","sessionId":"s","cwd":"/w","uuid":"root-second","parentUuid":null,"attachment":{}}"#,
        ];
        let parsed = parse_lines("forest", &lines).expect("file has a cwd");

        assert_eq!(parsed.root_uuid.as_deref(), Some("root-first"));
    }

    #[test]
    fn no_null_parent_record_yields_none_not_a_drop() {
        // FAIL-SOFT: no derivable root means "no lineage" (never folded), NEVER a
        // dropped session. The file is still a resumable session.
        let lines = [
            r#"{"type":"user","sessionId":"sess-rootless","cwd":"/w","uuid":"u-1","parentUuid":"gone","message":{"role":"user","content":"orphaned"}}"#,
            r#"{"type":"assistant","sessionId":"sess-rootless","cwd":"/w","uuid":"a-1","parentUuid":"u-1","message":{"role":"assistant","content":"ok"}}"#,
        ];
        let parsed = parse_lines("rootless", &lines).expect("a rootless file is still a session");

        assert_eq!(parsed.root_uuid, None);
        assert_eq!(parsed.cwd, "/w", "the session itself must survive");
        assert_eq!(parsed.session_id, "sess-rootless");
    }

    #[test]
    fn msg_count_counts_conversation_turns_not_tree_records() {
        // The fixture is built so the two candidate rules give VISIBLY different
        // answers: 6 turns sit inside 15 tree records. Count "every record that
        // carries uuid + parentUuid" — the set the ROOT logic uses — and this
        // file reports 15. A pair of near-identical fixtures could not tell the
        // rules apart at all, which is the point of the 9 non-conversation
        // records below.
        let mut lines = OUT_OF_TREE_PRELUDE.to_vec();
        lines.extend([
            // Hook-injected context: in the tree, but nobody said it.
            r#"{"type":"attachment","sessionId":"s","cwd":"/w","uuid":"att-1","parentUuid":null,"attachment":{}}"#,
            r#"{"type":"attachment","sessionId":"s","cwd":"/w","uuid":"att-2","parentUuid":"att-1","attachment":{}}"#,
            r#"{"type":"attachment","sessionId":"s","cwd":"/w","uuid":"att-3","parentUuid":"att-2","attachment":{}}"#,
            r#"{"type":"system","sessionId":"s","cwd":"/w","uuid":"sys-1","parentUuid":"att-3","content":"hook ran"}"#,
            r#"{"type":"system","sessionId":"s","cwd":"/w","uuid":"sys-2","parentUuid":"sys-1","content":"hook ran"}"#,
            // The conversation itself: 3 user + 3 assistant = 6 turns.
            r#"{"type":"user","sessionId":"s","cwd":"/w","uuid":"u-1","parentUuid":"sys-2","message":{"role":"user","content":"one"}}"#,
            r#"{"type":"assistant","sessionId":"s","cwd":"/w","uuid":"a-1","parentUuid":"u-1","message":{"role":"assistant","content":[{"type":"text","text":"1"}]}}"#,
            r#"{"type":"attachment","sessionId":"s","cwd":"/w","uuid":"att-4","parentUuid":"a-1","attachment":{}}"#,
            r#"{"type":"user","sessionId":"s","cwd":"/w","uuid":"u-2","parentUuid":"att-4","message":{"role":"user","content":"two"}}"#,
            r#"{"type":"assistant","sessionId":"s","cwd":"/w","uuid":"a-2","parentUuid":"u-2","message":{"role":"assistant","content":[{"type":"text","text":"2"}]}}"#,
            r#"{"type":"system","sessionId":"s","cwd":"/w","uuid":"sys-3","parentUuid":"a-2","content":"hook ran"}"#,
            r#"{"type":"system","sessionId":"s","cwd":"/w","uuid":"sys-4","parentUuid":"sys-3","content":"hook ran"}"#,
            r#"{"type":"attachment","sessionId":"s","cwd":"/w","uuid":"att-5","parentUuid":"sys-4","attachment":{}}"#,
            r#"{"type":"user","sessionId":"s","cwd":"/w","uuid":"u-3","parentUuid":"att-5","message":{"role":"user","content":"three"}}"#,
            r#"{"type":"assistant","sessionId":"s","cwd":"/w","uuid":"a-3","parentUuid":"u-3","message":{"role":"assistant","content":[{"type":"text","text":"3"}]}}"#,
        ]);
        let parsed = parse_lines("turn-count", &lines).expect("file has a cwd");

        assert_eq!(
            parsed.msg_count, 6,
            "only `user`/`assistant` records are turns"
        );
        assert_ne!(
            parsed.msg_count, 15,
            "counting every TREE record (the four types the root logic considers) \
             inflates this file's 6 turns to 15: `attachment` context is injected \
             by hooks and `system` records are notices, so neither is a turn"
        );
        // And the out-of-tree bookkeeping records are not turns either — a
        // `last-prompt` is a pointer, not something anybody said.
        assert_ne!(parsed.msg_count, 6 + OUT_OF_TREE_PRELUDE.len());
    }

    #[test]
    fn a_file_with_no_turns_counts_zero_rather_than_failing() {
        // FAIL-SOFT: a file with nothing said in it counts 0 and stays a
        // session. The last two records model schema drift — a `type` that is
        // absent, and one that is not a string — which `Value` access must
        // shrug off rather than panic on.
        let lines = [
            r#"{"type":"attachment","sessionId":"sess-quiet","cwd":"/w","uuid":"att-1","parentUuid":null,"attachment":{}}"#,
            r#"{"type":"system","sessionId":"sess-quiet","cwd":"/w","uuid":"sys-1","parentUuid":"att-1","content":"hook ran"}"#,
            r#"{"sessionId":"sess-quiet","cwd":"/w","uuid":"no-type-key"}"#,
            r#"{"type":42,"sessionId":"sess-quiet","cwd":"/w","uuid":"type-is-a-number"}"#,
        ];
        let parsed = parse_lines("no-turns", &lines).expect("a quiet file is still a session");

        assert_eq!(parsed.msg_count, 0);
        assert_eq!(parsed.session_id, "sess-quiet", "the session must survive");
    }

    #[test]
    fn msg_count_keeps_counting_past_the_content_index_cap() {
        // The counter is its own pass over every record, NOT a read of
        // `content_index` — which is exactly why the cap objection to the old
        // `content_index`-as-a-proxy idea does not apply to it. The index fills
        // and stops long before the records do; the count must not stop with it.
        const TURN_BYTES: usize = 1024;
        // Sized OFF the ceiling, deliberately: a hardcoded turn count stops
        // reaching the ceiling the moment `CONTENT_INDEX_CAP` is retuned, and
        // the assertion below then fails for the uninteresting reason instead of
        // pinning anything. The margin puts the overrun beyond one turn.
        let turns = CONTENT_INDEX_CAP / TURN_BYTES + 64;
        let body = "x".repeat(TURN_BYTES);
        let mut lines: Vec<String> = vec![
            r#"{"type":"attachment","sessionId":"s","cwd":"/w","uuid":"att-1","parentUuid":null,"attachment":{}}"#
                .to_string(),
        ];
        for i in 0..turns {
            lines.push(format!(
                r#"{{"type":"user","sessionId":"s","cwd":"/w","uuid":"u-{i}","parentUuid":"att-1","message":{{"role":"user","content":"{body}"}}}}"#
            ));
        }
        let borrowed: Vec<&str> = lines.iter().map(String::as_str).collect();
        let parsed = parse_lines("past-the-cap", &borrowed).expect("file has a cwd");

        assert_eq!(
            parsed.content_index.len(),
            CONTENT_INDEX_CAP,
            "the fixture must actually reach the cap, or it proves nothing"
        );
        assert_eq!(
            parsed.msg_count, turns,
            "the cap truncates the searchable text, never the turn count — a \
             count that stopped at the cap would silently understate exactly the \
             long sessions worth telling apart"
        );
    }

    #[test]
    fn the_most_recent_turn_of_a_real_sized_session_stays_findable() {
        // WHY THIS EXISTS: the index keeps the OLDEST bytes, so whatever
        // `CONTENT_INDEX_CAP` cuts is the most RECENT work — the end of the
        // transcript, which is the part people search for. The reported bug had
        // exactly this shape: a marker typed near the end of an ordinary
        // session sat past the old 64 KB bound and content search could not
        // reach it, while every test still passed.
        //
        // WHAT A RETUNE WOULD BREAK: this fixture is sized off a LITERAL, never
        // off `CONTENT_INDEX_CAP`, so lowering the ceiling back under a real
        // session's size turns it red. Its neighbour above sizes off the
        // constant deliberately and therefore holds at ANY value — between them,
        // that one proves the turn count outruns the ceiling and this one proves
        // the ceiling clears a real transcript. Neither substitutes for the
        // other.
        //
        // 200 KB is ORDINARY, not extreme: measured over a real store the
        // readable text ran to a p99 of 175 KB and a maximum of 252 KB
        // (`docs/agents/DOMAIN.md#content-index-storeparse`), so a ceiling that
        // cannot hold this much is re-introducing the bug rather than tuning it.
        const ORDINARY_SESSION_BYTES: usize = 200 * 1024;
        // Padding turn size; only its ratio to the total matters.
        const TURN_BYTES: usize = 1024;
        // A token the padding cannot produce, so a hit can only be the LAST
        // turn. Shaped after the marker in the session that surfaced this, which
        // sat at readable-text offsets ~187-248 KB — past the old bound.
        const RECENT_WORK_MARKER: &str = "MRK-2805-last-turn";

        let body = "x".repeat(TURN_BYTES);
        let mut lines: Vec<String> = vec![
            r#"{"type":"attachment","sessionId":"s","cwd":"/w","uuid":"att-1","parentUuid":null,"attachment":{}}"#
                .to_string(),
        ];
        for i in 0..ORDINARY_SESSION_BYTES / TURN_BYTES {
            lines.push(format!(
                r#"{{"type":"user","sessionId":"s","cwd":"/w","uuid":"u-{i}","parentUuid":"att-1","message":{{"role":"user","content":"{body}"}}}}"#
            ));
        }
        // Said LAST, where a truncation lands.
        lines.push(format!(
            r#"{{"type":"user","sessionId":"s","cwd":"/w","uuid":"u-latest","parentUuid":"att-1","message":{{"role":"user","content":"{RECENT_WORK_MARKER}"}}}}"#
        ));
        let borrowed: Vec<&str> = lines.iter().map(String::as_str).collect();
        let parsed = parse_lines("recent-work", &borrowed).expect("file has a cwd");

        assert!(
            parsed.content_index.contains(RECENT_WORK_MARKER),
            "the last thing said in an ordinary {} KB session must stay \
             searchable, but the index stopped after {} bytes",
            ORDINARY_SESSION_BYTES / 1024,
            parsed.content_index.len()
        );
        assert!(
            parsed.content_index.len() > ORDINARY_SESSION_BYTES,
            "the fixture must really carry {} KB of readable text, or it proves \
             nothing about a real session",
            ORDINARY_SESSION_BYTES / 1024
        );
    }

    #[test]
    fn agent_records_never_enter_the_content_index() {
        // `agent-setting` / `agent-name` carry no `message`/`summary`, so
        // `append_readable` ignores them: an agent name — or a free-form job title
        // in `agentName` — must never become a false search hit. The user turn's
        // text still indexes, proving the file parsed and the exclusion is the
        // record type, not an empty file.
        let lines = [
            r#"{"type":"attachment","sessionId":"s","cwd":"/w","uuid":"att-1","parentUuid":null,"attachment":{}}"#,
            r#"{"type":"agent-setting","agentSetting":"technical-brainstormer","sessionId":"s"}"#,
            r#"{"type":"agent-name","agentName":"Plan Node.js and Nest.js upgrade migration","sessionId":"s"}"#,
            r#"{"type":"user","sessionId":"s","cwd":"/w","uuid":"u-1","parentUuid":"att-1","message":{"role":"user","content":"please refactor"}}"#,
        ];
        let parsed = parse_lines("agent-records-index", &lines).expect("file has a cwd");

        assert!(
            parsed.content_index.contains("please refactor"),
            "the user turn must index: {:?}",
            parsed.content_index
        );
        assert!(
            !parsed.content_index.contains("technical-brainstormer"),
            "an agent-setting name must never enter the search index: {:?}",
            parsed.content_index
        );
        assert!(
            !parsed.content_index.contains("Node.js"),
            "an agent-name job title must never enter the search index: {:?}",
            parsed.content_index
        );
    }

    #[test]
    fn readable_text_handles_string_and_blocks() {
        assert_eq!(readable_text(&serde_json::json!("hello")), "hello");
        let blocks = serde_json::json!([
            {"type": "text", "text": "a"},
            {"type": "tool_use", "name": "Bash"},
            {"type": "text", "text": "b"}
        ]);
        assert_eq!(readable_text(&blocks), "a\nb");
    }

    #[test]
    fn truncate_respects_char_boundary() {
        let mut s = "é".repeat(40); // 2 bytes each => 80 bytes
        truncate_on_char_boundary(&mut s, 41);
        // 41 is mid-codepoint; must back off to 40.
        assert_eq!(s.len(), 40);
        assert!(s.chars().all(|c| c == 'é'));
    }
}
