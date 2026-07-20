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
pub mod lineage;
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
            root_uuid: parsed.root_uuid,
            msg_count: parsed.msg_count,
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
}
