//! Snapback-owned persistent state: the hidden-session id set.
//!
//! This is the FIRST persistent state snapback writes, and it lives in a
//! SEPARATE, snapback-owned directory — NEVER inside the read-only Claude store
//! under `~/.claude/projects/`. A session id in this set is a VISIBILITY
//! preference (hide the row from the board), not a status claim about the agent:
//! a hidden live session still reports live in the show-hidden view.
//!
//! The split follows the pure-core / thin-driver rule (PATTERNS §3):
//! [`parse_hidden`] / [`serialize_hidden`] are pure and fully unit-tested;
//! [`load_hidden`] / [`save_hidden`] are thin, fail-soft FS wrappers that delegate
//! to them and spawn no process. Reads fail SOFT to an empty set (a missing or
//! unreadable file is simply "nothing hidden yet", never a panic), matching the
//! JSONL fail-soft discipline (PATTERNS §1). Writes are ATOMIC (temp file +
//! rename) so a crash mid-write, or a second instance writing concurrently, can
//! never leave a half-written set behind — the target is always a complete set.

use std::collections::HashSet;
use std::path::Path;

/// File name of the newline-delimited hidden-id set inside snapback's state dir
/// ([`crate::config::state_dir`], `~/.config/snapback/state` by default). A
/// plain, greppable name so a user can inspect (or delete) it by hand.
const HIDDEN_FILE_NAME: &str = "hidden_sessions";

/// Suffix for the sibling temp file used by the atomic write. The final file is
/// produced by renaming this over the target, so a reader never observes a
/// partial write. It is written in the SAME dir as the target so the rename
/// stays within one filesystem (a cross-device rename is not atomic).
const TMP_SUFFIX: &str = ".tmp";

/// Parse a newline-delimited hidden-id file body into a set. Fail-soft: each line
/// is trimmed, blank lines are skipped, and duplicates collapse into the set — a
/// whitespace-noisy or partially-corrupt file can never panic, it just yields
/// whatever ids it could recover.
pub fn parse_hidden(body: &str) -> HashSet<String> {
    body.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Serialize a hidden-id set to a newline-delimited body in DETERMINISTIC SORTED
/// order, so re-saving an unchanged set produces byte-identical output (stable
/// diffs, no write churn). An empty set serializes to an empty string; both
/// round-trip through [`parse_hidden`].
pub fn serialize_hidden(ids: &HashSet<String>) -> String {
    let mut sorted: Vec<&String> = ids.iter().collect();
    sorted.sort_unstable();
    let mut out = String::new();
    for id in sorted {
        out.push_str(id);
        out.push('\n');
    }
    out
}

/// Load the hidden-id set from `dir/hidden_sessions`. Fail-soft: a missing or
/// unreadable file yields an EMPTY set (nothing hidden yet), never an error —
/// hidden state is a convenience, so a read failure must never block the board.
/// Delegates the body shape to [`parse_hidden`].
pub fn load_hidden(dir: &Path) -> HashSet<String> {
    match std::fs::read_to_string(dir.join(HIDDEN_FILE_NAME)) {
        Ok(body) => parse_hidden(&body),
        Err(_) => HashSet::new(),
    }
}

/// Persist the hidden-id set to `dir/hidden_sessions`, creating `dir` if absent.
/// Writes ATOMICALLY: the serialized body goes to a sibling temp file which is
/// then renamed over the target, so a concurrent reader (or a crash) never sees a
/// half-written set. The temp name carries the pid so two instances writing at
/// once do not clobber each other's temp file (last writer wins on the rename).
/// Delegates the body shape to [`serialize_hidden`]; spawns no process.
pub fn save_hidden(dir: &Path, ids: &HashSet<String>) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let target = dir.join(HIDDEN_FILE_NAME);
    let tmp = dir.join(format!(
        "{HIDDEN_FILE_NAME}{TMP_SUFFIX}.{}",
        std::process::id()
    ));
    std::fs::write(&tmp, serialize_hidden(ids))?;
    std::fs::rename(&tmp, &target)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A unique, isolated temp dir under `std::env::temp_dir()` — NEVER the real
    /// data dir. Mirrors the `snapback-<tag>-<pid>-<nanos>` convention used across
    /// the crate's tests.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "snapback-hidden-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    // --- parse_hidden / serialize_hidden (Task 1.2) -----------------------

    #[test]
    fn parse_then_serialize_round_trips_a_set() {
        let ids: HashSet<String> = ["s1", "s2", "s3"].iter().map(|s| s.to_string()).collect();
        let body = serialize_hidden(&ids);
        assert_eq!(
            parse_hidden(&body),
            ids,
            "a set survives a serialize/parse trip"
        );
    }

    #[test]
    fn serialize_emits_sorted_deterministic_order() {
        let ids: HashSet<String> = ["c", "a", "b"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            serialize_hidden(&ids),
            "a\nb\nc\n",
            "serialization is sorted so re-saving an unchanged set is byte-stable"
        );
    }

    #[test]
    fn parse_is_fail_soft_over_blanks_whitespace_and_dupes() {
        // Leading/trailing whitespace, blank lines, and a duplicate id.
        let body = "  a  \n\n b\n\n\na\n   \n";
        let want: HashSet<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            parse_hidden(body),
            want,
            "blank lines are skipped, ids are trimmed, and duplicates collapse"
        );
    }

    // --- load_hidden / save_hidden (Tasks 1.3 + 1.4) ----------------------

    #[test]
    fn save_then_load_round_trips_a_set() {
        let base = unique_temp_dir("roundtrip");
        // `state/` does not exist yet, so save must create the dir (create_dir_all).
        let dir = base.join("state");
        let ids: HashSet<String> = ["s1", "s2"].iter().map(|s| s.to_string()).collect();

        save_hidden(&dir, &ids).expect("save creates the dir and writes atomically");
        assert_eq!(load_hidden(&dir), ids, "a saved set loads back identically");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn load_is_empty_when_the_file_is_missing() {
        let dir = unique_temp_dir("missing");
        // No save happened, so `hidden_sessions` does not exist.
        assert!(
            load_hidden(&dir).is_empty(),
            "a missing file fails soft to an empty set"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_skips_a_garbage_blank_line() {
        let dir = unique_temp_dir("garbage");
        std::fs::write(dir.join(HIDDEN_FILE_NAME), "good\n\n   \nalso-good\n")
            .expect("write a file with a blank/whitespace garbage line");
        let want: HashSet<String> = ["good", "also-good"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            load_hidden(&dir),
            want,
            "a blank/whitespace garbage line is skipped, not surfaced as an id"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
