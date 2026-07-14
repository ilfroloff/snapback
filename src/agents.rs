//! Live-agent detection via `claude agents --json`.
//!
//! `claude -r <id>` REFUSES to plain-resume a session that is currently running
//! as a background/interactive agent ("Session <id> is currently running as a
//! background agent (bg). Use `claude agents` to find and attach to it, or add
//! --fork-session to branch off a copy."). The authoritative signal for "is this
//! session live right now" is `claude agents --json`, which prints a JSON ARRAY
//! of the ACTIVE agents and exits without needing a TTY ("for scripting; does
//! not require a TTY").
//!
//! This module shells out to that command and parses it FAIL-SOFT: a missing
//! binary, a non-zero exit, non-JSON output, or schema drift all collapse to an
//! empty live set — never a panic — so the board degrades to plain behavior when
//! the signal is unavailable. The shell-out itself ([`live_agents`]) MUST only
//! run OFF the UI thread (see [`crate::watch::EventLoop::spawn_agents_poller`]);
//! the pure parser ([`parse_agents_json`]) is unit tested without spawning
//! anything.

use std::collections::HashMap;
use std::process::Command;

use serde_json::Value;

/// The slice of a live agent the board UI needs, joined to a session by the
/// full `sessionId`.
///
/// Kept intentionally small and read all-optional: only `kind` is required to
/// render a badge, and every field is pulled out fail-soft so schema drift in
/// `claude agents --json` never discards the whole record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveAgent {
    /// `kind` field: `"background"` | `"interactive"` (rendered `bg` / `live`).
    pub kind: String,
    /// `id` field: the agent-view JOB id — the SHORT id (`claude agents --json`'s
    /// own `id`, e.g. `ca56b543`), NOT the full `sessionId`. This is what
    /// `claude attach <id>` matches. Only BACKGROUND agents carry it; an
    /// INTERACTIVE session has no attachable job, so `id` is `None` there. Read
    /// fail-soft like every other optional field.
    pub id: Option<String>,
    /// `state` field (e.g. `"blocked"` for a background agent), if present.
    pub state: Option<String>,
    /// `status` field (e.g. `"idle"`), if present.
    pub status: Option<String>,
    /// `name` field, if present.
    pub name: Option<String>,
}

impl LiveAgent {
    /// Compact kind label for the badge: `bg` for a background agent, `live` for
    /// an interactive one, else the raw kind (so schema drift still shows
    /// *something* rather than nothing).
    #[must_use]
    pub fn kind_label(&self) -> &str {
        match self.kind.as_str() {
            "background" => "bg",
            "interactive" => "live",
            other => other,
        }
    }

    /// A short dim qualifier from `state` (preferred) or `status`, if any — e.g.
    /// `blocked` / `idle` — shown after the kind label.
    #[must_use]
    pub fn qualifier(&self) -> Option<&str> {
        self.state.as_deref().or(self.status.as_deref())
    }
}

/// Parse the raw stdout of `claude agents --json` into a map keyed by full
/// `sessionId`.
///
/// FAIL-SOFT by construction: non-JSON or a non-array top level yields an empty
/// map; an element without a string `sessionId` is skipped; every other field is
/// read with a default/optional so an unexpected shape never discards the record
/// or panics. This is the ONLY place the wire shape is interpreted.
#[must_use]
pub fn parse_agents_json(raw: &str) -> HashMap<String, LiveAgent> {
    let mut live = HashMap::new();
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return live; // Not JSON at all -> no signal.
    };
    let Some(array) = value.as_array() else {
        return live; // Top level is not the documented array -> no signal.
    };
    for element in array {
        let Some(session_id) = element.get("sessionId").and_then(Value::as_str) else {
            continue; // No join key -> unusable record, skip (never fatal).
        };
        let str_field = |key: &str| element.get(key).and_then(Value::as_str).map(str::to_owned);
        live.insert(
            session_id.to_owned(),
            LiveAgent {
                kind: str_field("kind").unwrap_or_default(),
                id: str_field("id"),
                state: str_field("state"),
                status: str_field("status"),
                name: str_field("name"),
            },
        );
    }
    live
}

/// Shell out to `claude agents --json` and return the live set, or EMPTY on any
/// failure (missing binary, non-zero exit, unreadable / non-JSON output).
///
/// MUST be called off the UI thread — it spawns a child process. Output is
/// captured (no TTY inherited), so it never contends with an interactive
/// `claude` on the terminal. Never panics; every error path returns an empty map.
#[must_use]
pub fn live_agents() -> HashMap<String, LiveAgent> {
    let output = match Command::new("claude").args(["agents", "--json"]).output() {
        Ok(output) => output,
        Err(_) => return HashMap::new(), // `claude` not on PATH, spawn failed, etc.
    };
    if !output.status.success() {
        return HashMap::new(); // Non-zero exit -> treat as "no signal".
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    parse_agents_json(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Task VERIFY-1: garbage / empty / invalid / non-array JSON must all yield
    /// an EMPTY live set and never panic.
    #[test]
    fn garbage_empty_and_invalid_json_yield_empty_live_set() {
        for raw in [
            "",
            "   ",
            "not json at all",
            "{",
            "null",
            "42",
            "\"a bare string\"",
            "{\"sessionId\":\"x\"}", // a JSON OBJECT, not the documented array
            "[1, 2, 3]",             // array of non-objects
        ] {
            let live = parse_agents_json(raw);
            assert!(
                live.is_empty(),
                "expected an empty live set for {raw:?}, got {live:?}"
            );
        }
    }

    /// Task VERIFY-2 (parse side): active agents are keyed by their FULL
    /// `sessionId`, with kind/state/status extracted.
    #[test]
    fn parses_active_agents_keyed_by_full_session_id() {
        let raw = r#"[
            {"sessionId":"11111111-2222-3333-4444-555555555555","kind":"background","state":"blocked","status":"idle","pid":42,"id":"11111111","name":"bg-one"},
            {"sessionId":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee","kind":"interactive","status":"running"}
        ]"#;
        let live = parse_agents_json(raw);
        assert_eq!(live.len(), 2);

        let bg = live
            .get("11111111-2222-3333-4444-555555555555")
            .expect("background agent present under its full sessionId");
        // Full struct equality also exercises every field (incl. `name` and the
        // short agent-view `id` that `claude attach` matches).
        assert_eq!(
            bg,
            &LiveAgent {
                kind: "background".to_string(),
                id: Some("11111111".to_string()),
                state: Some("blocked".to_string()),
                status: Some("idle".to_string()),
                name: Some("bg-one".to_string()),
            }
        );
        assert_eq!(bg.kind_label(), "bg");
        assert_eq!(bg.qualifier(), Some("blocked"));

        let inter = live
            .get("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
            .expect("interactive agent present under its full sessionId");
        assert_eq!(inter.kind_label(), "live");
        // No `state` -> qualifier falls back to `status`.
        assert_eq!(inter.qualifier(), Some("running"));
        // An interactive session exposes no agent-view job `id` -> not
        // attachable (the gate the Attach hand-off relies on).
        assert_eq!(inter.id, None);
    }

    /// Schema drift (missing optional fields, unknown extra fields, or a
    /// sessionId-less element) never drops the WHOLE parse.
    #[test]
    fn schema_drift_never_fails_the_whole_parse() {
        let raw = r#"[
            {"kind":"background"},
            {"sessionId":"kept","future":"field","kind":42}
        ]"#;
        let live = parse_agents_json(raw);
        // The sessionId-less element is skipped; the other survives.
        assert_eq!(live.len(), 1);
        let kept = live.get("kept").expect("record with a sessionId survives");
        // `kind` was a NUMBER (not a string) -> fail-soft to empty, no panic.
        assert_eq!(kept.kind, "");
        assert_eq!(kept.qualifier(), None);
        assert_eq!(kept.name, None);
    }
}
