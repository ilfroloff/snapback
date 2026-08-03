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

/// Cap the per-session searchable transcript text at ~64 KB. Keeps the in-memory
/// content index a few MB across the whole store at current scale; if the store
/// grows into the thousands this is the boundary to move to an on-disk cache.
pub const CONTENT_INDEX_CAP: usize = 64 * 1024;

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
    /// would silently stop being counted at ~64 KB. This is a real counter over
    /// every record, so the cap cannot reach it.
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
    })
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
/// readable and small); breadth is bounded by [`CONTENT_INDEX_CAP`].
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
        // `content_index`-as-a-proxy idea does not apply to it. Each turn below
        // carries ~1 KB of text, so the index fills and stops long before the
        // records do; the count must not stop with it.
        let body = "x".repeat(1024);
        let mut lines: Vec<String> = vec![
            r#"{"type":"attachment","sessionId":"s","cwd":"/w","uuid":"att-1","parentUuid":null,"attachment":{}}"#
                .to_string(),
        ];
        let turns = 200;
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
