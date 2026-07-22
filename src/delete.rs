//! Session deletion: pure guards plus the thin FS remove driver.
//!
//! This is snapback's FIRST store MUTATION path. Everywhere else the Claude
//! store under `~/.claude/projects/` is treated as read-only, hostile input;
//! HARD delete is the one gated exception (behind a confirm modal and the live
//! guard below). The gate follows the pure-core / thin-driver split
//! (PATTERNS §3): [`can_delete`] and [`toggle_hidden`] are pure, fully
//! unit-tested decisions; [`remove`] is the thin, impure FS driver that performs
//! the unlink and spawns no process.
//!
//! [`remove`] is SUBAGENT-EXCLUSION-safe BY CONSTRUCTION (AGENTS.md SUBAGENT
//! EXCLUSION): it only ever targets the selected session's OWN `<id>.jsonl` file
//! and the sibling `<id>/` directory derived from that file's own parent + stem.
//! It never constructs, matches, or descends into a `subagents/` path any other
//! way, and never another session's directory.

use std::collections::HashSet;

use crate::store::Session;

/// User-facing refusal returned by [`can_delete`] when the target session is
/// live (claude is actively running it as an agent).
///
/// Unlinking a transcript claude is mid-write to would corrupt an in-flight
/// session, so HARD delete is refused and the message is dropped onto the board.
/// Worded like the resume-gate refusals: it states what was observed and points
/// at the next move rather than diagnosing. Soft-hide has no such guard — it is
/// reversible and touches no bytes on disk.
pub const DELETE_LIVE_REFUSAL: &str = "this session is running as an agent — stop it in \
     Claude Code first, then hard-delete.";

/// Pure guard for a HARD delete: refuse when the session is live, allow it
/// otherwise.
///
/// `is_live` comes from the caller's authoritative liveness probe
/// (`App::is_live_now`) taken at the moment of the confirm — the same freshly
/// re-asked posture the resume gate uses at hand-off, never a stale poll.
/// Returns `Err(user-facing message)` so the caller can hand it straight to
/// `set_status`, or `Ok(())` when the delete may proceed.
pub fn can_delete(is_live: bool) -> Result<(), String> {
    if is_live {
        Err(DELETE_LIVE_REFUSAL.to_string())
    } else {
        Ok(())
    }
}

/// Flip the hidden state of a whole GROUP of session ids together, pivoting on
/// `pivot`'s current membership: when `pivot` is currently visible, HIDE every id
/// in `members`; when it is already hidden, EXPOSE them all. Returns the NEW hidden
/// state (`true` = now hidden).
///
/// A background-fork lineage must hide and expose as ONE unit — otherwise hiding a
/// folded head would just drop it and let the fold re-head to a surviving fork, so
/// the lineage never leaves the board. A rootless singleton passes `members` of
/// length one (itself). Pivoting on one id (rather than each member's own state)
/// resolves a partially-hidden group cleanly to the pivot's opposite. The caller
/// owns the side effects — persist via `hidden::save_hidden` and re-filter —
/// keeping this decision pure and trivially testable.
pub fn toggle_hidden(set: &mut HashSet<String>, members: &[String], pivot: &str) -> bool {
    let hide = !set.contains(pivot);
    for id in members {
        if hide {
            set.insert(id.clone());
        } else {
            set.remove(id);
        }
    }
    hide
}

/// Thin, impure HARD-delete driver: unlink the session's `<id>.jsonl` transcript
/// and, when it is present, remove the sibling `<encoded-cwd>/<id>/` directory
/// that holds its subagent transcripts.
///
/// SUBAGENT EXCLUSION: the id directory is derived ONLY from this session's own
/// file — the file's PARENT joined with its file STEM, so `<encoded-cwd>/<id>.jsonl`
/// yields exactly `<encoded-cwd>/<id>/`. Removal can therefore never reach a
/// `subagents/` path by any other route, and never another session's directory.
/// The directory is removed only when it actually exists (a session with no
/// subagents simply has none). Spawns no process.
///
/// Returns the first FS error (e.g. the transcript already vanished from the
/// live store); the caller keeps the board up and reports it, matching the
/// fail-soft posture elsewhere.
pub(crate) fn remove(session: &Session) -> std::io::Result<()> {
    std::fs::remove_file(&session.file)?;

    // Derive the sibling id dir from the file's OWN parent + stem — the only
    // path removal may ever target besides the file itself. `file_stem` on
    // `<id>.jsonl` is `<id>`, so this resolves to exactly `<encoded-cwd>/<id>/`.
    if let (Some(parent), Some(stem)) = (session.file.parent(), session.file.file_stem()) {
        let id_dir = parent.join(stem);
        if id_dir.is_dir() {
            std::fs::remove_dir_all(&id_dir)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A unique, isolated temp dir under `std::env::temp_dir()` — NEVER the real
    /// `~/.claude/projects` store. Mirrors the `snapback-<tag>-<pid>-<nanos>`
    /// convention used across the crate's tests.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "snapback-delete-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Build a minimal `Session` pointing at `file`. `remove` only reads
    /// `session.file`, so the other fields carry inert placeholders.
    fn session_at(file: PathBuf) -> Session {
        Session {
            file,
            session_id: "sess-x".to_string(),
            cwd: PathBuf::from("/tmp/project"),
            git_branch: None,
            timestamp: None,
            repo: "project".to_string(),
            label: "label".to_string(),
            root_uuid: None,
            msg_count: 0,
            content_index: String::new(),
        }
    }

    // --- can_delete (Task 2.1) --------------------------------------------

    #[test]
    fn can_delete_refuses_a_live_session_and_allows_an_idle_one() {
        let refusal = can_delete(true).expect_err("a live session must be refused");
        assert_eq!(
            refusal, DELETE_LIVE_REFUSAL,
            "the refusal carries the live-session message"
        );
        assert!(
            !refusal.is_empty(),
            "the refusal must be a user-facing message, not empty"
        );
        assert!(
            can_delete(false).is_ok(),
            "a non-live session may be hard-deleted"
        );
    }

    // --- toggle_hidden (Task 2.2) -----------------------------------------

    #[test]
    fn toggle_hidden_flips_a_whole_group_pivoting_on_the_selected_id() {
        let mut ids: HashSet<String> = HashSet::new();
        let members = vec!["s1".to_string(), "s2".to_string(), "s3".to_string()];

        // Pivot s1 is visible → the first toggle hides the WHOLE lineage.
        assert!(
            toggle_hidden(&mut ids, &members, "s1"),
            "hiding a visible pivot reports the new state as hidden = true"
        );
        assert!(
            members.iter().all(|m| ids.contains(m)),
            "every lineage member is hidden together, not just the pivot"
        );

        // Pivot s1 is now hidden → the second toggle exposes the whole lineage.
        assert!(
            !toggle_hidden(&mut ids, &members, "s1"),
            "exposing a hidden pivot reports the new state as hidden = false"
        );
        assert!(
            members.iter().all(|m| !ids.contains(m)),
            "every lineage member is exposed together"
        );

        // A singleton (members = [pivot]) still round-trips.
        let solo = vec!["only".to_string()];
        assert!(toggle_hidden(&mut ids, &solo, "only"));
        assert!(ids.contains("only"));
        assert!(!toggle_hidden(&mut ids, &solo, "only"));
        assert!(!ids.contains("only"));

        // A PARTIALLY-hidden group resolves uniformly to the pivot's opposite:
        // s2 pre-hidden, visible pivot s1 → hide all (both end hidden).
        let mut mixed: HashSet<String> = HashSet::from(["s2".to_string()]);
        let group = vec!["s1".to_string(), "s2".to_string()];
        assert!(toggle_hidden(&mut mixed, &group, "s1"));
        assert!(mixed.contains("s1") && mixed.contains("s2"));
    }

    // --- remove (Tasks 2.3 + 2.4) -----------------------------------------

    #[test]
    fn remove_deletes_the_transcript_and_its_sibling_id_dir() {
        let base = unique_temp_dir("with-dir");
        // Lay out `<encoded-cwd>/<id>.jsonl` alongside the sibling
        // `<id>/subagents/agent-*.jsonl` that hard delete must also clear.
        let project = base.join("-Users-me-project");
        let id = "sess-remove-1";
        let file = project.join(format!("{id}.jsonl"));
        let subagents = project.join(id).join("subagents");
        std::fs::create_dir_all(&subagents).expect("create the subagents fixture dir");
        std::fs::write(&file, "{}\n").expect("write the transcript file");
        std::fs::write(subagents.join("agent-1.jsonl"), "{}\n")
            .expect("write a subagent transcript");

        let id_dir = project.join(id);
        assert!(
            file.is_file() && id_dir.is_dir(),
            "the fixture is laid out before removal"
        );

        remove(&session_at(file.clone())).expect("remove unlinks the file and the id dir");

        assert!(!file.exists(), "the transcript file is gone");
        assert!(
            !id_dir.exists(),
            "the sibling <id>/ dir (subagents included) is gone"
        );
        assert!(
            project.is_dir(),
            "removal targets only this id's paths, never the encoded-cwd dir"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn remove_deletes_only_the_file_when_there_is_no_sibling_dir() {
        let base = unique_temp_dir("no-dir");
        let project = base.join("-Users-me-solo");
        std::fs::create_dir_all(&project).expect("create the project dir");
        let id = "sess-solo-1";
        let file = project.join(format!("{id}.jsonl"));
        std::fs::write(&file, "{}\n").expect("write the transcript file");
        // No `<id>/` sibling dir exists; remove must tolerate its absence.

        remove(&session_at(file.clone())).expect("remove tolerates a missing sibling dir");

        assert!(!file.exists(), "the transcript file is gone");
        assert!(project.is_dir(), "the project dir is left untouched");

        let _ = std::fs::remove_dir_all(&base);
    }
}
