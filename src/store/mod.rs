//! Session store: the framework-independent data core.
//!
//! Owns the `Session` model and the top-level `SessionStore::load()` pipeline
//! (discover -> parse -> derive label/repo/content_index), returning sessions
//! sorted repo -> branch -> timestamp-desc. Every historical correctness
//! constraint (subagent exclusion, resume-from-inside-file `cwd`, fail-soft
//! parsing) lives here and is covered by unit tests under `tests/`.

pub mod discover;
pub mod group;
pub mod label;
pub mod parse;
pub mod preview;

use std::path::{Path, PathBuf};

use rayon::prelude::*;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

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
    /// Capped, readable transcript text for content search.
    pub content_index: String,
}

impl Session {
    /// The branch label for grouping/display, defaulting to `(detached)`.
    pub fn branch_display(&self) -> &str {
        self.git_branch.as_deref().unwrap_or(group::DETACHED)
    }

    /// Build a `Session` from one candidate file, or `None` if it is not a
    /// resumable session (no `cwd`) or cannot be read.
    fn from_file(path: &Path) -> Option<Session> {
        let parsed = parse::parse_file(path)?;
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

        Some(Session {
            file: path.to_path_buf(),
            session_id: parsed.session_id,
            cwd,
            git_branch: parsed.git_branch,
            timestamp,
            repo,
            label,
            content_index: parsed.content_index,
        })
    }
}

/// The data core entry point.
pub struct SessionStore;

impl SessionStore {
    /// Load every resumable session from the default store root
    /// (`$CLAUDE_PROJECTS_DIR` or `~/.claude/projects`).
    pub fn load() -> Vec<Session> {
        Self::load_from(&discover::store_root())
    }

    /// Load every resumable session from an explicit store `root`.
    ///
    /// Pipeline: discover (subagent-excluding) -> parse (fail-soft, per file in
    /// parallel) -> derive (label, repo, timestamp, content_index) -> sort by
    /// repo asc, branch asc, timestamp desc.
    pub fn load_from(root: &Path) -> Vec<Session> {
        let files = discover::discover(root);
        let mut sessions: Vec<Session> = files
            .into_par_iter()
            .filter_map(|f| Session::from_file(&f))
            .collect();
        sessions.sort_by(session_ordering);
        sessions
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
        // The five depth-2 `.jsonl` files, none of the nested subagent file.
        assert_eq!(files.len(), 5, "unexpected discovered set: {files:?}");
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
        // Exactly four resumable sessions survive (5 discovered - 1 sidecar).
        assert_eq!(sessions.len(), 4, "unexpected session count");
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
        let rendered = preview::render(s, 80);
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
}
