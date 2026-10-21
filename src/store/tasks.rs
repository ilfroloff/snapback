//! Task-notification ledger: the LATEST `<status>` each background task's
//! `task-notification` records reported, keyed by `<task-id>`.
//!
//! Claude Code delivers a background task's end into the session that launched it
//! as a synthesized `user` record whose `origin.kind` is
//! [`TASK_NOTIFICATION_ORIGIN`], carrying `<task-id>` and `<status>` tags. Those
//! records sit in the session file `store::parse` already streams, so this reads
//! NO new file kind: `<id>/subagents/` stays excluded by depth and the
//! SUBAGENT-EXCLUSION rule is untouched.
//!
//! This is the SETTLEMENT half only. The board's `[task failed]` flag
//! (`store::parse::task_signal`) reads the same records through the same
//! [`is_task_notification`] and [`tag_inner`]; what it keeps (the last `failed`
//! notice the user has not answered) and what this keeps (every task's latest
//! status, whatever it was) are different facts about the same bytes.
//!
//! # Latest status wins, and an unknown token still counts
//!
//! A task id can be notified twice (one measured), so [`scan_record`] overwrites:
//! file order decides. The `<status>` tokens are undocumented, so a token outside
//! the four observed ones is recorded as [`TaskStatus::Unknown`] rather than
//! skipped. Skipping it would leave an EARLIER `failed` standing after a later,
//! unrecognised notice superseded it; recording it lets the later notice decide,
//! and [`TaskLedger::is_failed`] then reads it as not failed (an unknown token
//! marks nothing).
//!
//! # Measured evidence (2026-10-21, a read-only scan of a local `~/.claude/projects`)
//!
//! - 15 subagents' latest notification is `failed`, across 13 sessions: 10 are
//!   "Agent stalled: no progress for 600s" and 5 are auth or API errors. A
//!   subagent transcript's own tail reads only the 5 as failed.
//! - A notification's `<task-id>` always names the subagent's own id, and an
//!   `agent-<task-id>.meta.json` exists for every AGENT notice. `<tool-use-id>` is
//!   absent on builds 2.1.261 to 2.1.296, so the join is by `<task-id>`.
//! - `origin.kind == "task-notification"` is present on every measured
//!   notification record (402 of 402 also carry `type: "user"`; counted
//!   2026-10-21 over the 2,715 `.jsonl` files under a local `~/.claude/projects`,
//!   sessions and subagent transcripts alike); detection keys on that marker,
//!   never on the notification's prose.
//!
//! # Fail-soft direction: toward NOT failed
//!
//! Every read is `serde_json::Value` + `Option`. A body that is not a string, a
//! notice naming no task id, or an unreadable `<status>` records nothing. The
//! direction is the display's: a wrong `failed` accuses a healthy subagent, so a
//! shape this module cannot read marks nothing.
//!
//! Pure: no I/O, no ambient state, no clock. [`scan_record`] is fed one already
//! parsed record at a time by the single streaming pass in `store::parse`.

use std::collections::HashMap;

use serde_json::Value;

/// `origin.kind` of a task-completion record: the STRUCTURAL marker both the
/// ledger and the board's `[task failed]` flag key on, never the notification's
/// prose, which is undocumented and varies per build.
pub const TASK_NOTIFICATION_ORIGIN: &str = "task-notification";

/// Terminal `<status>`: the task finished and reported back.
const STATUS_COMPLETED: &str = "completed";

/// Terminal `<status>`: the task was stopped.
const STATUS_STOPPED: &str = "stopped";

/// Terminal `<status>`: the task errored out, or stalled. The one token the
/// display reads.
///
/// Compared EXACTLY, never trimmed or case-folded: the board's `[task failed]`
/// flag compares the same way, so the two cannot disagree about a spelling.
pub const STATUS_FAILED: &str = "failed";

/// Terminal `<status>`: the task was killed.
const STATUS_KILLED: &str = "killed";

/// A task notification's `<status>` tag, as its `(open, close)` pair.
pub const TAG_STATUS: (&str, &str) = ("<status>", "</status>");

/// A task notification's `<summary>` tag: claude's own one-line account, which
/// the board quotes word for word.
pub const TAG_SUMMARY: (&str, &str) = ("<summary>", "</summary>");

/// A task notification's `<task-id>` tag: the task's own id, which for an agent
/// is the subagent's id and the join key that works on every build.
const TAG_TASK_ID: (&str, &str) = ("<task-id>", "</task-id>");

/// What a task notification's `<status>` said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// [`STATUS_COMPLETED`].
    Completed,
    /// [`STATUS_STOPPED`].
    Stopped,
    /// [`STATUS_FAILED`].
    Failed,
    /// [`STATUS_KILLED`].
    Killed,
    /// A token outside the four observed ones, or no readable `<status>`.
    Unknown,
}

impl TaskStatus {
    /// The status a `<status>` token names, exactly as written.
    fn from_token(token: Option<&str>) -> Self {
        match token {
            Some(STATUS_COMPLETED) => Self::Completed,
            Some(STATUS_STOPPED) => Self::Stopped,
            Some(STATUS_FAILED) => Self::Failed,
            Some(STATUS_KILLED) => Self::Killed,
            _ => Self::Unknown,
        }
    }
}

/// What one transcript's task notifications said: each task id's LATEST status.
///
/// A FACT about the bytes, asserting nothing about liveness. Held per session and
/// never unioned across a fork lineage here: nothing reads a lineage's union yet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskLedger {
    /// `<task-id>` -> the latest notified status, in file order.
    latest: HashMap<String, TaskStatus>,
}

impl TaskLedger {
    /// The latest status a notification gave `task_id`, or `None` when none named
    /// it.
    #[must_use]
    pub fn status_of(&self, task_id: &str) -> Option<TaskStatus> {
        self.latest.get(task_id).copied()
    }

    /// Whether `task_id`'s latest notification said `failed`. An unknown token, or
    /// no notification, is `false`.
    #[must_use]
    pub fn is_failed(&self, task_id: &str) -> bool {
        self.status_of(task_id) == Some(TaskStatus::Failed)
    }
}

/// Collect one already-parsed JSONL record into `ledger`.
///
/// The single per-record collector, called from the ONE streaming pass in
/// `store::parse::parse_file`: no second read. A record that is not a task
/// notification costs one `get` chain and allocates nothing.
pub fn scan_record(record: &Value, ledger: &mut TaskLedger) {
    if !is_task_notification(record) {
        return;
    }
    let Some(body) = notification_body(record) else {
        return;
    };
    let Some(task_id) = tag_inner(body, TAG_TASK_ID)
        .map(str::trim)
        .filter(|id| !id.is_empty())
    else {
        return;
    };
    let status = TaskStatus::from_token(tag_inner(body, TAG_STATUS));
    ledger.latest.insert(task_id.to_string(), status);
}

/// Whether this record is a task-completion notification.
///
/// Keyed on [`TASK_NOTIFICATION_ORIGIN`] ALONE, never also on `type == "user"`:
/// every notification on the wire carries it, but a drift in `type` must not hide
/// a real notice. The board's `[task failed]` flag asks this same question before
/// it applies its own `user` and sidechain gates.
pub fn is_task_notification(record: &Value) -> bool {
    record
        .get("origin")
        .and_then(|origin| origin.get("kind"))
        .and_then(Value::as_str)
        == Some(TASK_NOTIFICATION_ORIGIN)
}

/// A notification record's body: `message.content` when it is a STRING, else
/// `None`.
pub fn notification_body(record: &Value) -> Option<&str> {
    record
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
}

/// The text between the FIRST `open` tag in `body` and the first `close` after
/// it, exactly as written, or `None` when either is missing.
///
/// First wins because claude writes the envelope's own tags (`<status>`,
/// `<summary>`, `<task-id>`) AHEAD of the free-form `<result>` it closes with, so
/// a result that happens to quote a tag cannot take precedence. The ONE tag
/// reader for every notification field, the ledger's and the board flag's alike.
pub fn tag_inner<'a>(body: &'a str, (open, close): (&str, &str)) -> Option<&'a str> {
    let start = body.find(open)? + open.len();
    let rest = &body[start..];
    Some(&rest[..rest.find(close)?])
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    /// A notification record whose body is `body`.
    fn notice(body: &str) -> Value {
        json!({
            "type": "user",
            "origin": { "kind": TASK_NOTIFICATION_ORIGIN },
            "message": { "role": "user", "content": body },
        })
    }

    fn body(task_id: &str, status: &str) -> String {
        format!(
            "<task-notification>\n<task-id>{task_id}</task-id>\n<status>{status}</status>\n\
             <summary>Agent \"x\" ended</summary>\n<result>done</result>\n</task-notification>"
        )
    }

    fn ledger_of(records: &[Value]) -> TaskLedger {
        let mut ledger = TaskLedger::default();
        for record in records {
            scan_record(record, &mut ledger);
        }
        ledger
    }

    #[test]
    fn each_observed_token_reads_as_its_own_status() {
        let ledger = ledger_of(&[
            notice(&body("a", "completed")),
            notice(&body("b", "stopped")),
            notice(&body("c", "failed")),
            notice(&body("d", "killed")),
        ]);
        assert_eq!(ledger.status_of("a"), Some(TaskStatus::Completed));
        assert_eq!(ledger.status_of("b"), Some(TaskStatus::Stopped));
        assert_eq!(ledger.status_of("c"), Some(TaskStatus::Failed));
        assert_eq!(ledger.status_of("d"), Some(TaskStatus::Killed));
        assert_eq!(ledger.status_of("e"), None);
        assert!(ledger.is_failed("c"));
        for id in ["a", "b", "d", "e"] {
            assert!(!ledger.is_failed(id), "{id}");
        }
    }

    #[test]
    fn the_latest_notice_per_task_id_decides_in_either_order() {
        let failed_then_unknown =
            ledger_of(&[notice(&body("t", "failed")), notice(&body("t", "paused"))]);
        assert_eq!(
            failed_then_unknown.status_of("t"),
            Some(TaskStatus::Unknown)
        );
        assert!(!failed_then_unknown.is_failed("t"));

        let unknown_then_failed =
            ledger_of(&[notice(&body("t", "paused")), notice(&body("t", "failed"))]);
        assert!(unknown_then_failed.is_failed("t"));

        let failed_then_completed = ledger_of(&[
            notice(&body("t", "failed")),
            notice(&body("t", "completed")),
        ]);
        assert!(!failed_then_completed.is_failed("t"));
    }

    #[test]
    fn a_spelling_other_than_failed_marks_nothing() {
        for token in ["Failed", " failed", "failed ", "error", ""] {
            let ledger = ledger_of(&[notice(&body("t", token))]);
            assert!(!ledger.is_failed("t"), "{token:?}");
        }
    }

    #[test]
    fn a_notice_that_cannot_be_read_records_nothing() {
        let no_status = "<task-id>t</task-id>";
        let no_id = "<status>failed</status>";
        let blank_id = "<task-id>  </task-id><status>failed</status>";
        let unclosed = "<task-id>t</task-id><status>failed";
        for text in [no_id, blank_id] {
            assert_eq!(ledger_of(&[notice(text)]), TaskLedger::default(), "{text}");
        }
        // A readable id with no readable status is still a notice, and the latest
        // one: it supersedes, and reads as unknown.
        for text in [no_status, unclosed] {
            let ledger = ledger_of(&[notice(&body("t", "failed")), notice(text)]);
            assert!(!ledger.is_failed("t"), "{text}");
        }
        let mut array_body = notice("");
        array_body["message"]["content"] = json!([{ "type": "text", "text": body("t", "failed") }]);
        assert_eq!(ledger_of(&[array_body]), TaskLedger::default());
        assert_eq!(ledger_of(&[json!({})]), TaskLedger::default());
        assert_eq!(ledger_of(&[json!("text")]), TaskLedger::default());
    }

    #[test]
    fn only_the_notification_origin_is_a_notice() {
        let mut human = notice(&body("t", "failed"));
        human["origin"] = json!({ "kind": "human" });
        let mut bare = notice(&body("t", "failed"));
        bare.as_object_mut().unwrap().remove("origin");
        assert_eq!(ledger_of(&[human, bare]), TaskLedger::default());

        let mut drifted = notice(&body("t", "failed"));
        drifted["type"] = json!("system");
        assert!(ledger_of(&[drifted]).is_failed("t"));
    }

    #[test]
    fn the_first_tag_wins_over_one_quoted_in_the_result() {
        let text = "<task-id>t</task-id><status>completed</status>\
                    <result><status>failed</status><task-id>other</task-id></result>";
        let ledger = ledger_of(&[notice(text)]);
        assert_eq!(ledger.status_of("t"), Some(TaskStatus::Completed));
        assert_eq!(ledger.status_of("other"), None);
    }
}
