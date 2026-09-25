//! Session label derivation.
//!
//! Label preference:
//! 1. latest `type:"summary"` string, else
//! 2. the first "real" user prompt (skip `isSidechain` turns and
//!    `<...>`-wrapped command/system prompts; handle both string and
//!    typed-block `message.content`), else
//! 3. the `session_id`.
//!
//! Result is truncated to a display cap.
//!
//! NOTE: an inline `type:"ai-title"` `aiTitle` tier is deliberately NOT
//! considered — the summary and first real user prompt are the only sources.

use serde_json::Value;

/// Truncate labels to keep list lines short/atomic.
pub const LABEL_MAX: usize = 180;

/// Extract a `type:"summary"` line's title, if this record is one.
///
/// Empty / whitespace-only summaries are treated as absent so they never win
/// over a real user prompt.
pub fn summary_text(record: &Value) -> Option<String> {
    if record.get("type").and_then(Value::as_str) != Some("summary") {
        return None;
    }
    let s = record.get("summary").and_then(Value::as_str)?;
    if s.trim().is_empty() {
        return None;
    }
    Some(s.to_string())
}

/// Whether this record is a SUB-AGENT's turn (`isSidechain: true`) rather than
/// the session's own.
///
/// The store's one "not from a subagent" rule for a record, shared by the label
/// pick below and by `parse`'s failed-background-task pass, so the two cannot
/// disagree about whose turn a record is. It is the rule's SHARED home, not its
/// ONLY one: `store::preview` still holds an inline copy of the same read for its
/// user turns, and a change here must be mirrored there.
///
/// FAIL-SOFT: an absent, null, or non-bool `isSidechain` reads as `false` — the
/// session's own turn, which is what every depth-2 `user` record observed in a
/// real store carries (subagent turns live in their own files, which discovery
/// never reaches).
pub fn is_sidechain(record: &Value) -> bool {
    record
        .get("isSidechain")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Extract the text of a "real" user prompt from this record, or `None`.
///
/// A record qualifies when it is `type:"user"`, is not an `isSidechain` turn,
/// yields non-empty text, and is not a `<...>`-wrapped command/system prompt.
/// Both string and typed-block (`[{type:"text", text:..}]`) `message.content`
/// shapes are handled.
pub fn user_prompt_text(record: &Value) -> Option<String> {
    if record.get("type").and_then(Value::as_str) != Some("user") {
        return None;
    }
    if is_sidechain(record) {
        return None;
    }
    let content = record.get("message").and_then(|m| m.get("content"))?;
    let text = user_content_text(content);
    let head = text.trim_start();
    if head.is_empty() || head.starts_with('<') {
        return None;
    }
    Some(text)
}

/// Join a user record's `message.content` into a single string (text blocks
/// joined with a space).
fn user_content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// Apply the label preference and sanitize/truncate for display.
pub fn finalize_label(summary: Option<&str>, first_user: Option<&str>, session_id: &str) -> String {
    let raw = summary.or(first_user).unwrap_or(session_id);
    sanitize_and_truncate(raw, LABEL_MAX)
}

/// Replace tab/newline/carriage-return with spaces and truncate to `max`
/// characters (codepoint-indexed).
fn sanitize_and_truncate(s: &str, max: usize) -> String {
    s.chars()
        .map(|c| match c {
            '\t' | '\n' | '\r' => ' ',
            other => other,
        })
        .take(max)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summary_wins_over_user_prompt() {
        let label = finalize_label(Some("A title"), Some("a prompt"), "sid");
        assert_eq!(label, "A title");
    }

    #[test]
    fn falls_back_to_user_then_session_id() {
        assert_eq!(finalize_label(None, Some("a prompt"), "sid"), "a prompt");
        assert_eq!(finalize_label(None, None, "sid"), "sid");
    }

    #[test]
    fn empty_summary_is_ignored() {
        let record = json!({"type": "summary", "summary": "   "});
        assert_eq!(summary_text(&record), None);
    }

    #[test]
    fn user_prompt_skips_sidechain_and_wrapped() {
        let sidechain = json!({
            "type": "user",
            "isSidechain": true,
            "message": {"content": "hidden tool turn"}
        });
        assert_eq!(user_prompt_text(&sidechain), None);

        let wrapped = json!({
            "type": "user",
            "message": {"content": "<command-name>/clear</command-name>"}
        });
        assert_eq!(user_prompt_text(&wrapped), None);
    }

    #[test]
    fn is_sidechain_reads_only_a_true_bool_as_a_subagent_turn() {
        assert!(is_sidechain(&json!({"type": "user", "isSidechain": true})));
        // Everything else is the session's own turn: false, absent, and the
        // fail-soft shapes a drifted schema could carry.
        for own in [
            json!({"type": "user", "isSidechain": false}),
            json!({"type": "user"}),
            json!({"type": "user", "isSidechain": null}),
            json!({"type": "user", "isSidechain": "true"}),
            json!({"type": "user", "isSidechain": 1}),
        ] {
            assert!(!is_sidechain(&own), "not a subagent turn: {own}");
        }
    }

    #[test]
    fn user_prompt_handles_string_and_typed_blocks() {
        let string_turn = json!({
            "type": "user",
            "message": {"content": "plain question"}
        });
        assert_eq!(
            user_prompt_text(&string_turn).as_deref(),
            Some("plain question")
        );

        let typed_turn = json!({
            "type": "user",
            "message": {"content": [
                {"type": "text", "text": "first part"},
                {"type": "tool_result", "content": "ignored"},
                {"type": "text", "text": "second part"}
            ]}
        });
        assert_eq!(
            user_prompt_text(&typed_turn).as_deref(),
            Some("first part second part")
        );
    }

    #[test]
    fn tabs_and_newlines_are_flattened_and_truncated() {
        let raw = format!("line one\nline two\t{}", "x".repeat(300));
        let label = finalize_label(Some(&raw), None, "sid");
        assert_eq!(label.chars().count(), LABEL_MAX);
        assert!(!label.contains('\n'));
        assert!(!label.contains('\t'));
        assert!(label.starts_with("line one line two "));
    }
}
