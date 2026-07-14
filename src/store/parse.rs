//! Fail-soft JSONL parsing.
//!
//! Streams each session file line-by-line as `serde_json::Value` (never
//! hard-typed structs, so schema drift can never be fatal): unparseable lines
//! and non-object values are skipped, one bad file is skipped rather than
//! aborting the scan. Extracts `cwd` and `sessionId` from INSIDE the file
//! (never decoded from the folder name); falls back `session_id` to the file
//! stem. Any file with no `cwd` is dropped (sidecar agent-name/ai-title files
//! are not resumable).

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde_json::Value;

use super::label;

/// Cap the per-session searchable transcript text at ~64 KB. Keeps the in-memory
/// content index a few MB across the whole store at current scale; if the store
/// grows into the thousands this is the boundary to move to an on-disk cache.
pub const CONTENT_INDEX_CAP: usize = 64 * 1024;

/// The raw fields extracted from one JSONL file in a single streaming pass.
///
/// Derivation (label, repo, timestamp parsing) happens above this in
/// `SessionStore`; this struct only carries what a single fail-soft scan can
/// read straight out of the file.
pub struct ParsedFile {
    /// `cwd` read from inside the file (first non-null). Guaranteed present:
    /// files with no `cwd` return `None` from [`parse_file`].
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
    /// Capped, readable transcript text for content search.
    pub content_index: String,
}

/// Stream one JSONL file fail-soft and extract its metadata.
///
/// Returns `None` when the file cannot be opened or carries no `cwd` (a sidecar
/// agent-name/ai-title file, which is not a resumable session).
pub fn parse_file(path: &Path) -> Option<ParsedFile> {
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);

    let mut cwd: Option<String> = None;
    let mut session_id: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut timestamp_raw: Option<String> = None;
    let mut summary: Option<String> = None;
    let mut first_user: Option<String> = None;
    let mut content_index = String::new();

    for line in reader.lines() {
        // A read error on a single line is skipped, never fatal.
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
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

    // No cwd => not a resumable session; drop the file entirely.
    let cwd = cwd?;
    let session_id = session_id.unwrap_or_else(|| file_stem(path));
    truncate_on_char_boundary(&mut content_index, CONTENT_INDEX_CAP);

    Some(ParsedFile {
        cwd,
        session_id,
        git_branch,
        timestamp_raw,
        summary,
        first_user,
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
