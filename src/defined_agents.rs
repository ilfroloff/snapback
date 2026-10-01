//! Discovery of DEFINED Claude Code agents (subagents) for a NEW session.
//!
//! DISTINCT from [`crate::agents`], which detects RUNNING/live agents via
//! `claude agents --json` for the Attach/Fork overlay. Here we enumerate the
//! agents a user can BIND a brand-new session to (`claude --agent <name>`):
//! Markdown files with YAML frontmatter under `<claude-profile>/agents/*.md`
//! (user-level, default `~/.claude/agents`, PROFILE-scoped via
//! `config::claude_config_dir_if_known`) and `<launch_dir>/.claude/agents/*.md`
//! (project-level, repo-local — unaffected by the profile). Keep the two
//! concepts apart — this module never touches the live-agent wire shape and
//! `agents.rs` never touches these definition files.
//!
//! This list is a CONVENIENCE only: built-in and plugin agents are not files on
//! disk, so a filesystem scan is inherently incomplete. The picker therefore
//! always offers a bare "default (no agent)" launch and a launch is never blocked
//! on discovery finding a match. Every read is FAIL-SOFT (the same discipline as
//! the JSONL / `claude agents --json` rules): a missing dir, an unreadable file,
//! or malformed frontmatter is skipped, never a panic.
//!
//! Pure core, thin impure driver: the merge/dedup ([`select_agents`]) and the
//! frontmatter parse ([`parse_frontmatter`]) are pure and unit-tested; the FS
//! walk ([`discover_agents`] / `agents_in_dir`) is the thin wrapper over them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config;

/// The subdirectory (under the Claude profile and `<launch_dir>/.claude`) that
/// holds selectable agent-definition Markdown files. Named per Claude Code's
/// on-disk layout (`.claude/agents/*.md`); a `const` so the two call sites cannot
/// drift.
const AGENTS_SUBDIR: &str = "agents";

/// The `.claude` config directory name for the PROJECT-level locations: the
/// agents dir (`<launch_dir>/.claude/agents`) and [`crate::claude_settings`]'
/// project settings files (`<launch_dir>/.claude/settings{,.local}.json`), so the
/// name claude gives its config directory is written once. The user-level agents
/// location is NOT this literal joined onto the home dir any more — it is
/// profile-scoped via [`config::claude_config_dir_if_known`], which already
/// resolves to `<home>/.claude` by default (or `$CLAUDE_CONFIG_DIR` when set).
/// Kept here because the project-level paths are deliberately NOT
/// profile-scoped (they are repo-local), so they still need the literal named
/// per NO MAGIC VALUES.
pub(crate) const CLAUDE_DIR: &str = ".claude";

/// File extension of an agent-definition file. Only `*.md` files are considered;
/// anything else in the directory is ignored.
const AGENT_FILE_EXT: &str = "md";

/// The frontmatter fence line that opens and closes a YAML frontmatter block.
const FRONTMATTER_FENCE: &str = "---";

/// A user-selectable agent definition for a new session.
///
/// `name` is the value passed as `claude --agent <name>`; `description` (the
/// frontmatter `description`, if any) is shown dim in the picker as a hint. Kept
/// intentionally minimal (YAGNI): the `model`/`tools` frontmatter fields are not
/// read — the launcher only needs the name and the picker only needs a hint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinedAgent {
    /// The agent identifier passed to `claude --agent <name>`.
    pub name: String,
    /// A short human hint (frontmatter `description`), if present.
    pub description: Option<String>,
}

/// Merge user-level and project-level agents into the selectable list.
///
/// PROJECT overrides USER on a `name` collision (a repo-local agent shadows a
/// same-named user agent), the result is deduped by `name`, and sorted by `name`
/// so the picker order is stable across runs. Pure so the precedence + dedup is
/// unit-tested without touching the filesystem.
#[must_use]
pub fn select_agents(user: Vec<DefinedAgent>, project: Vec<DefinedAgent>) -> Vec<DefinedAgent> {
    // Insert user first, then let project entries overwrite on a name collision.
    let mut by_name: HashMap<String, DefinedAgent> = HashMap::new();
    for agent in user.into_iter().chain(project) {
        by_name.insert(agent.name.clone(), agent);
    }
    let mut agents: Vec<DefinedAgent> = by_name.into_values().collect();
    agents.sort_by(|a, b| a.name.cmp(&b.name));
    agents
}

/// Parse an agent-definition file's leading YAML frontmatter FAIL-SOFT.
///
/// Expects a `---`-delimited block at the very top; within it, simple
/// `key: value` lines. `name` is taken from the frontmatter, falling back to
/// `stem` (the file stem) when the frontmatter omits it, so a present-but-nameless
/// file is still selectable rather than silently dropped. `description` is
/// optional. Returns `None` only when there is no frontmatter block at all (a
/// plain Markdown file is not an agent definition) or when no usable name can be
/// derived. Never panics: any unexpected shape simply yields the fallback / `None`.
/// Hand-rolled (no YAML crate) to keep the crate dependency-free, mirroring the
/// self-contained markdown pass in `store::preview`.
#[must_use]
pub fn parse_frontmatter(contents: &str, stem: &str) -> Option<DefinedAgent> {
    let body = frontmatter_lines(contents)?;
    let name = field(&body, "name")
        .map(unquote)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| stem.trim().to_string());
    if name.is_empty() {
        return None; // no frontmatter name and an empty stem — nothing to launch.
    }
    let description = field(&body, "description")
        .map(unquote)
        .filter(|s| !s.is_empty());
    Some(DefinedAgent { name, description })
}

/// The body lines of a leading YAML frontmatter block, or `None` when the
/// content does not open with a `---` fence or never closes it.
///
/// The opening fence must be the first line (tolerating a leading UTF-8 BOM);
/// everything up to the next `---` line is the body. A missing closing fence is
/// treated as malformed and fails soft to `None`.
fn frontmatter_lines(contents: &str) -> Option<Vec<&str>> {
    let mut lines = contents.lines();
    let first = lines.next()?.trim_start_matches('\u{feff}').trim_end();
    if first != FRONTMATTER_FENCE {
        return None; // no frontmatter block -> not an agent definition file.
    }
    let mut body = Vec::new();
    for line in lines {
        if line.trim_end() == FRONTMATTER_FENCE {
            return Some(body); // closing fence reached.
        }
        body.push(line);
    }
    None // opened a block but never closed it -> malformed, skip.
}

/// The trimmed value of the first `key: value` frontmatter line whose key
/// matches `key`, or `None`. Splits on the FIRST `:` so a value containing a
/// colon survives intact.
fn field<'a>(body: &[&'a str], key: &str) -> Option<&'a str> {
    body.iter().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        (k.trim() == key).then_some(v.trim())
    })
}

/// Strip a single pair of matching surrounding quotes (`'…'` or `"…"`) from a
/// frontmatter value, returning the inner text. Leaves unquoted values untouched.
fn unquote(s: &str) -> String {
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

/// The user-level agents directory: `<claude-profile>/agents` (default
/// `~/.claude/agents`, or `$CLAUDE_CONFIG_DIR/agents` when that override is set).
///
/// PROFILE-scoped, unlike the project-level path below: this list is exactly
/// what `claude --agent <name>` can resolve for the profile the board is
/// running against, so a mismatched profile is what makes a perfectly valid
/// agent name surface `NEW_SESSION_NONZERO_HINT` ("check that its name is
/// valid") — the name is valid, just in a different profile.
///
/// `None` when no profile can be named (no override and no home): the guessed
/// relative `.claude` would resolve against the launch dir and list that
/// project's own agents a second time as user-level ones.
fn user_agents_dir() -> Option<PathBuf> {
    user_agents_dir_from(config::claude_config_dir_if_known())
}

/// The pure half of [`user_agents_dir`]: `profile`'s agents subdirectory, and no
/// directory at all without a profile.
fn user_agents_dir_from(profile: Option<PathBuf>) -> Option<PathBuf> {
    profile.map(|profile| profile.join(AGENTS_SUBDIR))
}

/// The project-level agents directory for a launch dir (`<launch_dir>/.claude/agents`).
fn project_agents_dir(launch_dir: &Path) -> PathBuf {
    launch_dir.join(CLAUDE_DIR).join(AGENTS_SUBDIR)
}

/// Parse every `*.md` agent definition directly inside `dir`, fail-soft.
///
/// A missing / unreadable directory yields an empty list; an unreadable file or
/// one whose frontmatter does not parse is skipped. Never descends into
/// subdirectories — agent definitions live directly in `agents/`.
fn agents_in_dir(dir: &Path) -> Vec<DefinedAgent> {
    let mut agents = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return agents; // missing / unreadable dir -> no agents (fail-soft).
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(AGENT_FILE_EXT) {
            continue; // only *.md agent-definition files.
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue; // unreadable file -> skip, never abort the scan.
        };
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if let Some(agent) = parse_frontmatter(&contents, stem) {
            agents.push(agent);
        }
    }
    agents
}

/// Discover the agents selectable for a new session in `launch_dir`, FAIL-SOFT.
///
/// Scans `<claude-profile>/agents/*.md` (user, default `~/.claude/agents`;
/// skipped when no profile can be named) and `<launch_dir>/.claude/agents/*.md`
/// (project), parses each fail-soft, and merges via [`select_agents`] (project
/// overrides user). The return is always a (possibly empty) list, never an
/// error: every missing dir / unreadable file / malformed frontmatter is
/// skipped. The thin impure driver over the pure [`select_agents`] /
/// [`parse_frontmatter`].
#[must_use]
pub fn discover_agents(launch_dir: &Path) -> Vec<DefinedAgent> {
    let user = user_agents_dir()
        .map(|dir| agents_in_dir(&dir))
        .unwrap_or_default();
    let project = agents_in_dir(&project_agents_dir(launch_dir));
    select_agents(user, project)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(name: &str, description: Option<&str>) -> DefinedAgent {
        DefinedAgent {
            name: name.to_string(),
            description: description.map(str::to_string),
        }
    }

    #[test]
    fn parses_name_and_description_from_frontmatter() {
        let md = "---\nname: code-reviewer\ndescription: Reviews diffs for bugs\nmodel: sonnet\ntools: Read, Grep\n---\n\nYou are a reviewer.\n";
        let parsed = parse_frontmatter(md, "file-stem").expect("valid frontmatter parses");
        assert_eq!(
            parsed,
            agent("code-reviewer", Some("Reviews diffs for bugs"))
        );
    }

    #[test]
    fn falls_back_to_the_file_stem_when_frontmatter_has_no_name() {
        // A present-but-nameless frontmatter is tolerated: the file stem names it
        // rather than dropping an otherwise-usable definition.
        let md = "---\ndescription: no explicit name here\n---\nbody";
        let parsed = parse_frontmatter(md, "planner").expect("stem fallback still parses");
        assert_eq!(parsed.name, "planner");
        assert_eq!(parsed.description.as_deref(), Some("no explicit name here"));
    }

    #[test]
    fn strips_surrounding_quotes_from_values() {
        let md = "---\nname: \"quoted-name\"\ndescription: 'single quoted'\n---\n";
        let parsed = parse_frontmatter(md, "stem").expect("quoted values parse");
        assert_eq!(parsed, agent("quoted-name", Some("single quoted")));
    }

    #[test]
    fn returns_none_without_a_frontmatter_block() {
        // A plain Markdown file (no leading `---` fence) is not an agent
        // definition — skip it rather than guessing a name.
        assert!(parse_frontmatter("# Just a heading\n\nsome text", "stem").is_none());
    }

    #[test]
    fn returns_none_on_an_unterminated_frontmatter_block() {
        // Opened a block but never closed it -> malformed -> fail soft to None.
        assert!(parse_frontmatter("---\nname: x\nno closing fence here", "stem").is_none());
    }

    #[test]
    fn tolerates_a_leading_bom_before_the_fence() {
        let md = "\u{feff}---\nname: bommed\n---\n";
        let parsed = parse_frontmatter(md, "stem").expect("a BOM before the fence is tolerated");
        assert_eq!(parsed.name, "bommed");
    }

    #[test]
    fn description_absent_yields_none() {
        let md = "---\nname: solo\n---\n";
        let parsed = parse_frontmatter(md, "stem").expect("name-only frontmatter parses");
        assert_eq!(parsed, agent("solo", None));
    }

    #[test]
    fn select_agents_dedups_project_over_user_and_sorts_by_name() {
        let user = vec![
            agent("zeta", Some("user zeta")),
            agent("shared", Some("USER shared")),
        ];
        let project = vec![
            agent("shared", Some("PROJECT shared")),
            agent("alpha", Some("project alpha")),
        ];
        let merged = select_agents(user, project);
        // Sorted by name: alpha, shared, zeta.
        assert_eq!(
            merged.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "shared", "zeta"]
        );
        // The project entry wins the `shared` name collision.
        let shared = merged.iter().find(|a| a.name == "shared").unwrap();
        assert_eq!(shared.description.as_deref(), Some("PROJECT shared"));
    }

    #[test]
    fn select_agents_is_empty_for_no_inputs() {
        assert!(select_agents(vec![], vec![]).is_empty());
    }

    #[test]
    fn discover_agents_is_empty_for_a_dir_without_agent_files() {
        // A launch dir with no `.claude/agents` must fail soft to an empty list,
        // never an error — the picker then degrades to a bare `claude` launch.
        let dir = std::env::temp_dir().join(format!(
            "snapback-defined-agents-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp launch dir");
        let project = agents_in_dir(&project_agents_dir(&dir));
        assert!(project.is_empty(), "no project agents in a bare launch dir");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn agents_in_dir_reads_and_skips_fail_soft() {
        // A real directory with one valid, one non-md, and one malformed file:
        // only the valid agent survives, and the scan never panics.
        let dir = std::env::temp_dir().join(format!(
            "snapback-agents-in-dir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp agents dir");
        std::fs::write(
            dir.join("reviewer.md"),
            "---\nname: reviewer\ndescription: valid one\n---\nbody",
        )
        .unwrap();
        std::fs::write(dir.join("notes.txt"), "not an agent file").unwrap();
        std::fs::write(dir.join("broken.md"), "no frontmatter at all").unwrap();

        let agents = agents_in_dir(&dir);
        assert_eq!(agents.len(), 1, "only the valid *.md agent is discovered");
        assert_eq!(agents[0].name, "reviewer");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn user_agents_dir_follows_claude_config_dir() {
        // The user-level agents dir must be PROFILE-scoped: it is derived from
        // `config::claude_config_dir_if_known`, not a hardcoded `~/.claude`. Set
        // the env var by its LITERAL name (never via a const under test) and
        // assert the LITERAL `agents` segment, per the repo's env-test convention.
        let _guard = config::env_lock();
        let previous = std::env::var_os("CLAUDE_CONFIG_DIR");

        let dir = std::env::temp_dir().join(format!(
            "snapback-user-agents-dir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::env::set_var("CLAUDE_CONFIG_DIR", &dir);
        let overridden = user_agents_dir();
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        let default = user_agents_dir();

        match previous {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }

        assert_eq!(
            overridden,
            Some(dir.join("agents")),
            "a set CLAUDE_CONFIG_DIR must relocate the user-level agents dir"
        );
        assert_eq!(
            default,
            dirs::home_dir().map(|home| home.join(".claude").join("agents")),
            "with CLAUDE_CONFIG_DIR unset the default is <home>/.claude/agents, and no dir without a home"
        );
    }

    /// The no-home, no-override case is `None`, as it was before the list became
    /// profile-scoped — never a relative `.claude/agents`, which would re-list the
    /// launch project's agents as user-level ones.
    #[test]
    fn user_agents_dir_from_names_no_dir_without_a_profile() {
        assert_eq!(
            user_agents_dir_from(None),
            None,
            "no profile names no user-level agents dir — never a guessed relative one"
        );
        assert_eq!(
            user_agents_dir_from(Some(PathBuf::from("/profiles/work"))),
            Some(PathBuf::from("/profiles/work/agents")),
            "a named profile's user-level agents live in its `agents` subdir"
        );
        assert_eq!(
            user_agents_dir_from(Some(PathBuf::from(".claude-work"))),
            Some(PathBuf::from(".claude-work/agents")),
            "a user-set relative profile is honoured as given — no absolute-path filter here"
        );
    }
}
