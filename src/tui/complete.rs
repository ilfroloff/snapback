//! Pick-list core for the compose box, shared by BOTH drafts (the `Ctrl-R`
//! reply and the `Ctrl-N` background draft): which token the caret is in, which
//! candidates match it, and what accepting one writes. `/` offers skills and
//! commands; `@` offers files and folders and, for a top-level token, agents.
//! Each candidate carries its source's description, if any.
//!
//! Candidates are matched by the board's ONE matcher
//! ([`SearchIndex::admits`]), asked of a candidate's name and then its
//! description, and what matches is ordered by [`Placement`] — the pick list's
//! own order, never the board's. `memchr` and `nucleo` stay in `search.rs`
//! (AGENTS.md MATCHER ISOLATION). A pure core plus one thin impure `read_dir`
//! walker ([`list_tree`]); the caller decides WHEN to read (once per folder,
//! from the key handler, never the render path).
//!
//! Every column is a CHARACTER column (what `TextArea::cursor` reports), never
//! a byte offset, so multi-byte text before the caret cannot mis-cut a token.

use std::path::Path;

use crate::search::SearchIndex;
use crate::store::skills::ListingEntry;

/// Cap on entries read from one folder. Bounds a single keystroke's cost on a
/// huge or slow-mounted folder; a folder past it lists its first entries only.
pub const COMPLETION_MAX_DIR_ENTRIES: usize = 2000;

/// Cap on entries one `@` walk collects across ALL levels. There is no depth
/// cap: a monorepo path sits 9+ levels down (measured 2026-10-05 on a 7k-file
/// pnpm repo: 11.8k entries to depth 18, ~40 ms), and a depth cap silently hid
/// it. The walk is breadth first, so a walk that hits this drops the deepest
/// entries, never the shallow ones; the cap is sized at about twice that repo.
pub const COMPLETION_MAX_TREE_ENTRIES: usize = 25_000;

/// Folders an `@` walk lists but never descends into: build output and vendored
/// trees that would spend [`COMPLETION_MAX_TREE_ENTRIES`] on files nobody
/// mentions. Dot-folders (`.git`, `.next`, `.venv`, `.gradle`, ...) are never
/// descended into either, so none is listed here. A name is listed only when it
/// is generated output or an installed dependency in every repo that has it:
/// `bin`, `out`, `vendor`, `lib`, `public` and `tmp` stay OUT because some repos
/// keep hand-written scripts or committed source there. Names are matched
/// whole, per path component.
const TREE_SKIPPED_DIRS: [&str; 13] = [
    // JS/TS, Rust, Java/Kotlin (Maven `target`, Gradle `build`)
    "node_modules",
    "target",
    "dist",
    "build",
    "coverage",
    "storybook-static",
    "bower_components",
    // Python
    "__pycache__",
    "venv",
    "site-packages",
    // .NET
    "obj",
    // iOS/macOS
    "Pods",
    "DerivedData",
];

/// Rows of the pick list shown at once; more scroll with the highlight. Keeps
/// the list from covering the transcript preview above the compose box.
pub const COMPLETION_VISIBLE_ROWS: usize = 8;

/// What an agent mention starts with after its `@`. claude 2.1.284's mention
/// parser reads `@(agent-[\w:.@-]+)` and turns it into a structured
/// `agent_mention` (verified in `claude -p`, 2026-09-30). One whitespace-free
/// token, so [`completion_context`] reads a half-typed mention like any other.
///
/// claude's docs give `@agent-<name>` as the form to type by hand, plugin agents
/// included (`@agent-<plugin>:<name>`; code.claude.com/docs/en/sub-agents, read
/// 2026-10-02). claude's own picker inserts `@"<name> (agent)"` instead, and
/// that form misses an agent whose name contains `agent-`, because claude strips
/// the first `agent-` from the quoted name before looking it up:
/// `@"my-agent-x (agent)"` yields no `agent_mention` where `@agent-my-agent-x`
/// does (claude 2.1.284, probed in `claude -p`, 2026-10-02). So snapback keeps
/// this form. Evidence: `docs/agents/CLAUDE_CLI.md`, "The `initialize` control
/// handshake".
pub const AGENT_MENTION_PREFIX: &str = "agent-";

/// Appended to an agent's label so a row reads as an agent without a section
/// title: claude 2.1.284's own typeahead labels an agent `<name> (agent)`.
pub const AGENT_LABEL_SUFFIX: &str = " (agent)";

/// The non-alphanumeric characters claude 2.1.284's mention parser accepts in an
/// agent name: its class `[\w:.@-]`, where `\w` is ASCII `[A-Za-z0-9_]` (read
/// from the 2.1.284 binary, 2026-09-30). See [`is_mention_safe`].
const MENTION_NAME_PUNCTUATION: [char; 5] = ['_', ':', '.', '@', '-'];

/// What opened the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// `/` as the first character of the draft: skills and commands.
    Slash,
    /// `@` at the start of a word: files and folders, then (for a top-level
    /// token) agents.
    At,
}

/// The token the caret ends, with the query typed after its trigger character.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context {
    pub trigger: Trigger,
    pub row: usize,
    /// Character column of the trigger character.
    pub start: usize,
    /// The characters between the trigger and the caret.
    pub query: String,
}

/// One pick-list row: what it shows versus what replaces the query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub label: String,
    pub insert: String,
    /// What the listing says a command or agent does; `None` for a file or
    /// folder, and for an entry its source gave no description.
    pub description: Option<String>,
}

/// One folder entry, as read by [`list_tree`]. `name` is a path relative to the
/// listed folder, `/`-separated: `name` or `sub/dir/name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntryInfo {
    pub name: String,
    pub is_dir: bool,
}

/// The completion token the caret sits at the END of, if any. `lines` and
/// `cursor` are the textarea's, in character columns. A caret in the middle of
/// a token yields `None`, so accepting never has to touch text after the caret.
pub fn completion_context(lines: &[String], cursor: (usize, usize)) -> Option<Context> {
    let (row, col) = cursor;
    let chars: Vec<char> = lines.get(row)?.chars().collect();
    if col > chars.len() {
        return None;
    }
    // The caret must end the token: only whitespace or the line end may follow.
    if chars.get(col).is_some_and(|c| !c.is_whitespace()) {
        return None;
    }
    let mut start = col;
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    let trigger = match chars.get(start) {
        Some('/') if row == 0 && start == 0 && start < col => Trigger::Slash,
        Some('@') if start < col => Trigger::At,
        _ => return None,
    };
    Some(Context {
        trigger,
        row,
        start,
        query: chars[start + 1..col].iter().collect(),
    })
}

/// Where the query landed in a candidate, declared best first so the derived
/// order IS the pick list's order. claude 2.1.284 orders its own `/` list the
/// same way: a name that starts with the query, then a name that holds it
/// elsewhere, then a description-only hit (read from the binary, 2026-10-02).
/// See DOMAIN.md "Compose pick list".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Placement {
    NameStart,
    InName,
    /// A path entry whose folder components hold the query but whose own name
    /// does not (`src/store/discover.rs` for `store`). Only [`select_entries`]
    /// produces it.
    InPath,
    InDescription,
}

/// The board's own matcher, compiled for one pick-list token. The token holds no
/// whitespace ([`completion_context`] ends it there), so it is exactly ONE atom,
/// under the board's per-atom smart case.
fn matcher(query: &str) -> SearchIndex {
    let mut matcher = SearchIndex::new();
    matcher.set_query(query);
    matcher
}

/// Where `matcher`'s query lands in a candidate: its `name`, else its
/// `description`, else `None`. Asked of the NAME, never the row's label (`/name`,
/// `<name> (agent)`), so the decoration can never match. An empty query admits
/// every name and marks nothing, so every candidate lands in ONE tier and keeps
/// its source order.
fn placement(matcher: &SearchIndex, name: &str, description: Option<&str>) -> Option<Placement> {
    if matcher.admits(name) {
        Some(if matcher.atom_match_positions(name).first() == Some(&0) {
            Placement::NameStart
        } else {
            Placement::InName
        })
    } else if description.is_some_and(|d| matcher.admits(d)) {
        Some(Placement::InDescription)
    } else {
        None
    }
}

/// [`placement`] of a path entry: judged on its own name (the last component)
/// first, so a name that starts with the query outranks one that merely holds it,
/// and a hit only in a folder component ranks last.
fn path_placement(matcher: &SearchIndex, rel: &str) -> Option<Placement> {
    let base = rel.rsplit('/').next().unwrap_or(rel);
    placement(matcher, base, None).or_else(|| matcher.admits(rel).then_some(Placement::InPath))
}

/// The listing entries `query` matches, ordered by [`Placement`]. The sort is
/// stable, which is what keeps the caller's order within a tier.
fn select_listed<'a>(
    entries: impl IntoIterator<Item = &'a ListingEntry>,
    query: &str,
) -> Vec<&'a ListingEntry> {
    let matcher = matcher(query);
    let mut picked: Vec<(Placement, &ListingEntry)> = entries
        .into_iter()
        .filter_map(|e| placement(&matcher, &e.name, e.description.as_deref()).map(|p| (p, e)))
        .collect();
    picked.sort_by_key(|(placement, _)| *placement);
    picked.into_iter().map(|(_, e)| e).collect()
}

/// Commands (skills included) whose name holds `query` anywhere, or else whose
/// description does, matched like the board's search box (an uppercase letter
/// matches exactly). A name that starts with it lists first, then a name that
/// holds it elsewhere, then a description hit, each tier in the given order.
/// `label` is `/name`; `insert` is the bare name (the space is
/// [`replacement`]'s); `description` is the entry's.
pub fn filter_commands(entries: &[ListingEntry], query: &str) -> Vec<Candidate> {
    select_listed(entries, query)
        .into_iter()
        .map(|e| Candidate {
            label: format!("/{}", e.name),
            insert: e.name.clone(),
            description: e.description.clone(),
        })
        .collect()
}

/// Whether claude's mention parser reads ALL of `name` after
/// [`AGENT_MENTION_PREFIX`]: non-empty, and every character ASCII alphanumeric
/// or one of [`MENTION_NAME_PUNCTUATION`]. A name it would cut short would be
/// inserted and then silently ignored, so such an agent is never offered.
pub fn is_mention_safe(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || MENTION_NAME_PUNCTUATION.contains(&c))
}

/// Agents matched and ordered as [`filter_commands`] matches commands — by name
/// anywhere, then by description — skipping every name that is not
/// [`is_mention_safe`]. A query that already starts with [`AGENT_MENTION_PREFIX`]
/// (typed in any case) is matched by what follows it; any other query by itself,
/// against the name and description and never the label — so `@a` lists only
/// the agents that hold an `a` there, never every agent through the prefix or
/// [`AGENT_LABEL_SUFFIX`]. `label` is the name plus [`AGENT_LABEL_SUFFIX`];
/// `insert` is [`AGENT_MENTION_PREFIX`] plus the name.
pub fn select_agents(entries: &[ListingEntry], query: &str) -> Vec<Candidate> {
    select_listed(
        entries.iter().filter(|e| is_mention_safe(&e.name)),
        strip_mention_prefix(query),
    )
    .into_iter()
    .map(|e| Candidate {
        label: format!("{}{AGENT_LABEL_SUFFIX}", e.name),
        insert: format!("{AGENT_MENTION_PREFIX}{}", e.name),
        description: e.description.clone(),
    })
    .collect()
}

/// `query` without a leading [`AGENT_MENTION_PREFIX`] typed in any case. The
/// rest keeps ITS case, or smart case could never see an uppercase letter typed
/// after the prefix.
fn strip_mention_prefix(query: &str) -> &str {
    match query.get(..AGENT_MENTION_PREFIX.len()) {
        Some(head) if head.eq_ignore_ascii_case(AGENT_MENTION_PREFIX) => {
            &query[AGENT_MENTION_PREFIX.len()..]
        }
        _ => query,
    }
}

/// Every `@` candidate for `query`: the entries of the folder its `dir_part`
/// names (`dir_entries`, read by the caller) that its leaf matches, in
/// [`select_entries`]' order, followed, ONLY for a top-level token (no
/// `dir_part`), by the matching [`select_agents`] — entries above agents
/// whatever their tiers. An agent is not a path, so `@src/…` never lists one.
pub fn at_candidates(
    dir_entries: &[DirEntryInfo],
    agents: &[ListingEntry],
    query: &str,
) -> Vec<Candidate> {
    let (dir_part, leaf) = split_path_query(query);
    let mut candidates = select_entries(dir_entries, leaf);
    if dir_part.is_empty() {
        candidates.extend(select_agents(agents, query));
    }
    candidates
}

/// Split an `@` query after its LAST `/` into `(dir_part, leaf)`:
/// `"src/tu"` -> `("src/", "tu")`, `"README"` -> `("", "README")`.
pub fn split_path_query(query: &str) -> (&str, &str) {
    match query.rfind('/') {
        Some(i) => query.split_at(i + 1),
        None => ("", query),
    }
}

/// Entries whose path holds `leaf` anywhere (an uppercase letter matches
/// exactly, as in the board's search box): its own name or any folder above it, so
/// `disc` finds `store/discover.rs`. An empty `leaf` lists only the folder's direct
/// children, keeping the list a folder-by-folder walk. A dot-name (any component
/// starting with `.`) shows only when `leaf` itself starts with `.`, a visibility
/// gate applied before matching. Ordered by [`path_placement`] (own name starts
/// with `leaf`, then holds it, then folder components hold it), then folders
/// before files, then by path. Paths are inserted VERBATIM; folders carry a
/// trailing `/`.
pub fn select_entries(entries: &[DirEntryInfo], leaf: &str) -> Vec<Candidate> {
    let matcher = matcher(leaf);
    let show_hidden = leaf.starts_with('.');
    let mut picked: Vec<(Placement, &DirEntryInfo)> = entries
        .iter()
        .filter(|e| !leaf.is_empty() || !e.name.contains('/'))
        .filter(|e| show_hidden || !e.name.split('/').any(|c| c.starts_with('.')))
        .filter_map(|e| path_placement(&matcher, &e.name).map(|p| (p, e)))
        .collect();
    picked.sort_by(|(pa, a), (pb, b)| {
        pa.cmp(pb)
            .then_with(|| b.is_dir.cmp(&a.is_dir))
            .then_with(|| a.name.cmp(&b.name))
    });
    picked
        .into_iter()
        .map(|(_, e)| {
            let text = if e.is_dir {
                format!("{}/", e.name)
            } else {
                e.name.clone()
            };
            Candidate {
                label: text.clone(),
                insert: text,
                description: None,
            }
        })
        .collect()
}

/// The text that replaces the query after the trigger character. A command, an
/// `@` file and an `@` agent mention end with a space; an `@` folder does NOT,
/// so the list reopens one level down. `next_is_whitespace` omits the space when
/// one already follows.
pub fn replacement(
    trigger: Trigger,
    dir_part: &str,
    candidate: &Candidate,
    next_is_whitespace: bool,
) -> String {
    let is_folder = trigger == Trigger::At && candidate.insert.ends_with('/');
    let mut out = match trigger {
        Trigger::Slash => candidate.insert.clone(),
        Trigger::At => format!("{dir_part}{}", candidate.insert),
    };
    if !is_folder && !next_is_whitespace {
        out.push(' ');
    }
    out
}

/// Move the highlight one row, wrapping around a list of `len` rows.
pub fn move_highlight(current: usize, len: usize, forward: bool) -> usize {
    if len == 0 {
        return 0;
    }
    if forward {
        (current + 1) % len
    } else {
        (current + len - 1) % len
    }
}

/// The highlight after the list changed: reset to the top when the token moved
/// to a new start, otherwise clamped into range.
pub fn settle_highlight(current: usize, len: usize, token_start_changed: bool) -> usize {
    if token_start_changed || len == 0 {
        0
    } else {
        current.min(len - 1)
    }
}

/// Entries of `dir` and the folders below it, as `/`-relative paths, breadth
/// first, bounded by [`COMPLETION_MAX_TREE_ENTRIES`]. Never descends a symlink (no cycles), a
/// dot-folder or a [`TREE_SKIPPED_DIRS`] folder. FAIL-SOFT: a missing or
/// unreadable folder is empty and a failing entry is skipped.
pub fn list_tree(dir: &Path) -> Vec<DirEntryInfo> {
    list_tree_capped(dir, COMPLETION_MAX_TREE_ENTRIES)
}

fn list_tree_capped(root: &Path, cap: usize) -> Vec<DirEntryInfo> {
    let mut out = Vec::new();
    let mut queue = std::collections::VecDeque::from([String::new()]);
    while let Some(prefix) = queue.pop_front() {
        let Ok(read) = std::fs::read_dir(root.join(&prefix)) else {
            continue;
        };
        for entry in read.take(COMPLETION_MAX_DIR_ENTRIES).filter_map(Result::ok) {
            if out.len() >= cap {
                return out;
            }
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let file_type = entry.file_type();
            let real_dir = file_type.as_ref().is_ok_and(|t| t.is_dir());
            // Only a symlink needs the extra stat to learn it points at a folder.
            let is_dir =
                real_dir || file_type.is_ok_and(|t| t.is_symlink()) && entry.path().is_dir();
            let rel = format!("{prefix}{name}");
            if real_dir && !name.starts_with('.') && !TREE_SKIPPED_DIRS.contains(&name.as_str()) {
                queue.push_back(format!("{rel}/"));
            }
            out.push(DirEntryInfo { name: rel, is_dir });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &[&str]) -> Vec<String> {
        s.iter().map(|l| (*l).to_owned()).collect()
    }

    fn ctx(l: &[&str], cur: (usize, usize)) -> Option<Context> {
        completion_context(&lines(l), cur)
    }

    fn cand(s: &str) -> Candidate {
        Candidate {
            label: s.to_owned(),
            insert: s.to_owned(),
            description: None,
        }
    }

    fn listed(name: &str, description: Option<&str>) -> ListingEntry {
        ListingEntry {
            name: name.to_owned(),
            description: description.map(str::to_owned),
        }
    }

    fn inserts(candidates: &[Candidate]) -> Vec<&str> {
        candidates.iter().map(|c| c.insert.as_str()).collect()
    }

    fn entry(name: &str, is_dir: bool) -> DirEntryInfo {
        DirEntryInfo {
            name: name.to_owned(),
            is_dir,
        }
    }

    #[test]
    fn slash_triggers_only_at_row_zero_column_zero() {
        let c = ctx(&["/cr-re"], (0, 6)).expect("slash token");
        assert_eq!(c.trigger, Trigger::Slash);
        assert_eq!(c.query, "cr-re");
        assert_eq!(c.start, 0);
        assert!(ctx(&["hi /cr"], (0, 6)).is_none());
        assert!(ctx(&["hi", "/cr"], (1, 3)).is_none());
        assert!(ctx(&["/cr x"], (0, 5)).is_none(), "whitespace before caret");
    }

    #[test]
    fn a_bare_trigger_has_an_empty_query() {
        assert_eq!(ctx(&["/"], (0, 1)).unwrap().query, "");
        assert_eq!(ctx(&["@"], (0, 1)).unwrap().query, "");
        assert!(ctx(&["/"], (0, 0)).is_none(), "caret before the trigger");
    }

    #[test]
    fn at_triggers_at_line_start_and_after_whitespace() {
        let c = ctx(&["@src/tu"], (0, 7)).unwrap();
        assert_eq!(
            (c.trigger, c.start, c.query.as_str()),
            (Trigger::At, 0, "src/tu")
        );
        let c = ctx(&["read @RE"], (0, 8)).unwrap();
        assert_eq!(
            (c.trigger, c.start, c.query.as_str()),
            (Trigger::At, 5, "RE")
        );
        let c = ctx(&["a", "b @x"], (1, 4)).unwrap();
        assert_eq!((c.row, c.start), (1, 2));
    }

    #[test]
    fn an_at_inside_a_word_never_triggers() {
        assert!(ctx(&["a@b.com"], (0, 7)).is_none());
        assert!(ctx(&["mail a@b"], (0, 8)).is_none());
    }

    #[test]
    fn a_caret_inside_a_token_yields_none() {
        assert!(ctx(&["/cr-review"], (0, 3)).is_none());
        assert!(ctx(&["@src/tui"], (0, 4)).is_none());
        // Whitespace right after the caret still ends the token.
        assert!(ctx(&["@src rest"], (0, 4)).is_some());
        assert!(ctx(&["@src"], (0, 9)).is_none(), "column past the line");
    }

    #[test]
    fn columns_are_characters_not_bytes() {
        let c = ctx(&["héllo wörld @日本"], (0, 15)).expect("multi-byte before caret");
        assert_eq!(
            (c.trigger, c.start, c.query.as_str()),
            (Trigger::At, 12, "日本")
        );
    }

    #[test]
    fn the_command_filter_matches_anywhere_in_the_name() {
        let names = vec![
            listed("cr-review", None),
            listed("Handoff", None),
            listed("pr-squash", None),
        ];
        let got = filter_commands(&names, "h");
        assert_eq!(inserts(&got), vec!["Handoff", "pr-squash"]);
        assert_eq!(
            got[0],
            Candidate {
                label: "/Handoff".into(),
                insert: "Handoff".into(),
                description: None,
            }
        );
        assert_eq!(filter_commands(&names, "").len(), 3);
        assert_eq!(
            inserts(&filter_commands(&names, "review")),
            vec!["cr-review"],
            "anywhere, not only at the start"
        );
    }

    #[test]
    fn the_command_filter_carries_each_description() {
        let entries = vec![
            listed("compact", Some("Clear history but keep a summary")),
            listed("cr-review", None),
        ];
        let got = filter_commands(&entries, "c");
        assert_eq!(
            got.iter()
                .map(|c| c.description.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("Clear history but keep a summary"), None]
        );
    }

    #[test]
    fn mention_safe_names_follow_claudes_name_class() {
        for ok in ["plugin:agent", "Explore", "a.b@c-d_e", "sby-agent"] {
            assert!(is_mention_safe(ok), "{ok} should be offered");
        }
        for bad in ["has space", "\"q\"", "é", ""] {
            assert!(!is_mention_safe(bad), "{bad:?} should be skipped");
        }
    }

    #[test]
    fn an_agent_row_is_labelled_as_an_agent_and_inserts_a_mention() {
        let got = select_agents(&[listed("sby-agent", Some("Pings back"))], "sb");
        assert_eq!(
            got,
            vec![Candidate {
                label: "sby-agent (agent)".into(),
                insert: "agent-sby-agent".into(),
                description: Some("Pings back".into()),
            }]
        );
    }

    #[test]
    fn agents_match_anywhere_in_the_name_in_input_order() {
        let agents = vec![
            listed("zeta", None),
            listed("Explore", None),
            listed("sby-agent", None),
            listed("explain", None),
        ];
        assert_eq!(
            inserts(&select_agents(&agents, "exp")),
            vec!["agent-Explore", "agent-explain"]
        );
        assert_eq!(
            inserts(&select_agents(&agents, "xpl")),
            vec!["agent-Explore", "agent-explain"]
        );
        assert_eq!(
            inserts(&select_agents(&agents, "")),
            vec![
                "agent-zeta",
                "agent-Explore",
                "agent-sby-agent",
                "agent-explain"
            ]
        );
        assert_eq!(
            inserts(&select_agents(&agents, "agent")),
            vec!["agent-sby-agent"]
        );
    }

    #[test]
    fn a_typed_mention_prefix_matches_the_name_after_it() {
        let agents = vec![listed("sby-agent", None), listed("Explore", None)];
        assert_eq!(
            inserts(&select_agents(&agents, "agent-sb")),
            vec!["agent-sby-agent"]
        );
        assert_eq!(
            inserts(&select_agents(&agents, "AGENT-ex")),
            vec!["agent-Explore"],
            "the prefix is case-insensitive too"
        );
        assert_eq!(select_agents(&agents, "agent-").len(), 2);
    }

    /// `Explore`'s LABEL, `Explore (agent)`, holds an `a` and its name does not,
    /// so only matching the label could list it.
    #[test]
    fn a_short_query_never_lists_every_agent_through_the_mention_prefix() {
        let agents = vec![
            listed("Explore", None),
            listed("alpha", None),
            listed("sby-agent", None),
        ];
        assert_eq!(
            inserts(&select_agents(&agents, "a")),
            vec!["agent-alpha", "agent-sby-agent"]
        );
        assert_eq!(
            inserts(&select_agents(&agents, "ag")),
            vec!["agent-sby-agent"]
        );
        assert_eq!(
            inserts(&select_agents(&agents, "agent")),
            vec!["agent-sby-agent"]
        );
    }

    /// `zeta` before `alpha`: a tier keeps the catalog's order, never the
    /// alphabet's.
    #[test]
    fn a_name_start_hit_lists_first_then_a_name_hit_then_a_description_hit() {
        let entries = vec![
            listed("zeta", Some("Review a change")),
            listed("cr-review", None),
            listed("alpha", Some("the review loop")),
            listed("review-pr", None),
            listed("reviewer", Some("Checks style")),
            listed("other", Some("unrelated")),
        ];
        let got = filter_commands(&entries, "review");
        assert_eq!(
            inserts(&got),
            vec!["review-pr", "reviewer", "cr-review", "zeta", "alpha"]
        );
        assert_eq!(
            got.iter()
                .map(|c| c.description.as_deref())
                .collect::<Vec<_>>(),
            vec![
                None,
                Some("Checks style"),
                None,
                Some("Review a change"),
                Some("the review loop")
            ]
        );
    }

    #[test]
    fn an_agent_matches_its_description_after_every_name_hit() {
        let agents = vec![
            listed("planner", Some("Reviews plans before work")),
            listed("sby-agent", Some("Pings back")),
            listed("reviewer", None),
        ];
        assert_eq!(
            inserts(&select_agents(&agents, "review")),
            vec!["agent-reviewer", "agent-planner"]
        );
        assert_eq!(
            inserts(&select_agents(&agents, "pings")),
            vec!["agent-sby-agent"]
        );
    }

    #[test]
    fn an_uppercase_letter_matches_exactly_as_the_board_search_does() {
        let agents = vec![listed("explain", None), listed("Explore", None)];
        assert_eq!(
            inserts(&select_agents(&agents, "ex")),
            vec!["agent-explain", "agent-Explore"]
        );
        assert_eq!(
            inserts(&select_agents(&agents, "Ex")),
            vec!["agent-Explore"]
        );
        let commands = vec![listed("Handoff", None), listed("pr-squash", None)];
        assert_eq!(inserts(&filter_commands(&commands, "H")), vec!["Handoff"]);
        assert_eq!(
            inserts(&filter_commands(&commands, "h")),
            vec!["Handoff", "pr-squash"]
        );
        let e = vec![entry("README.md", false), entry("Readme", true)];
        assert_eq!(inserts(&select_entries(&e, "READ")), vec!["README.md"]);
        assert_eq!(
            inserts(&select_entries(&e, "read")),
            vec!["Readme/", "README.md"]
        );
    }

    #[test]
    fn a_mention_prefix_keeps_the_case_of_what_follows_it() {
        let agents = vec![listed("explain", None), listed("Explore", None)];
        assert_eq!(
            inserts(&select_agents(&agents, "agent-Ex")),
            vec!["agent-Explore"]
        );
        assert_eq!(
            inserts(&select_agents(&agents, "AGENT-ex")),
            vec!["agent-explain", "agent-Explore"]
        );
        assert_eq!(
            inserts(&select_agents(&agents, "agent-")),
            vec!["agent-explain", "agent-Explore"]
        );
    }

    #[test]
    fn an_agent_claude_cannot_mention_is_never_offered() {
        let agents = vec![
            listed("has space", None),
            listed("é-agent", None),
            listed("ok-agent", None),
        ];
        assert_eq!(inserts(&select_agents(&agents, "")), vec!["agent-ok-agent"]);
    }

    #[test]
    fn a_top_level_at_lists_folders_then_files_then_agents() {
        let dir = vec![entry("notes.txt", false), entry("src", true)];
        let agents = vec![listed("sby-agent", None), listed("Explore", None)];
        assert_eq!(
            inserts(&at_candidates(&dir, &agents, "")),
            vec!["src/", "notes.txt", "agent-sby-agent", "agent-Explore"]
        );
        assert_eq!(
            inserts(&at_candidates(&dir, &agents, "s")),
            vec!["src/", "notes.txt", "agent-sby-agent"]
        );
    }

    #[test]
    fn an_at_under_a_folder_lists_no_agents() {
        let sub = vec![entry("sby-notes.md", false)];
        let agents = vec![listed("sby-agent", None)];
        assert_eq!(
            inserts(&at_candidates(&sub, &agents, "src/sb")),
            vec!["sby-notes.md"]
        );
        assert_eq!(
            inserts(&at_candidates(&sub, &agents, "src/")),
            vec!["sby-notes.md"]
        );
    }

    #[test]
    fn split_path_query_splits_after_the_last_slash() {
        assert_eq!(split_path_query("src/tu"), ("src/", "tu"));
        assert_eq!(split_path_query("README"), ("", "README"));
        assert_eq!(split_path_query(""), ("", ""));
        assert_eq!(split_path_query("a/b/"), ("a/b/", ""));
        assert_eq!(split_path_query("/etc/ho"), ("/etc/", "ho"));
    }

    #[test]
    fn dot_names_show_only_for_a_dot_leaf() {
        let e = vec![
            entry(".git", true),
            entry("src", true),
            entry(".env", false),
        ];
        let plain: Vec<_> = select_entries(&e, "")
            .into_iter()
            .map(|c| c.insert)
            .collect();
        assert_eq!(plain, vec!["src/"]);
        let dotted: Vec<_> = select_entries(&e, ".")
            .into_iter()
            .map(|c| c.insert)
            .collect();
        assert_eq!(dotted, vec![".git/", ".env"]);
    }

    #[test]
    fn folders_come_first_then_files_each_sorted() {
        let e = vec![
            entry("b.rs", false),
            entry("zed", true),
            entry("a.rs", false),
            entry("alpha", true),
        ];
        let got: Vec<_> = select_entries(&e, "")
            .into_iter()
            .map(|c| c.label)
            .collect();
        assert_eq!(got, vec!["alpha/", "zed/", "a.rs", "b.rs"]);
    }

    #[test]
    fn entry_match_is_case_insensitive_but_inserts_verbatim() {
        let e = vec![entry("README.md", false), entry("Readme", true)];
        let got: Vec<_> = select_entries(&e, "read")
            .into_iter()
            .map(|c| c.insert)
            .collect();
        assert_eq!(got, vec!["Readme/", "README.md"]);
    }

    #[test]
    fn entries_match_anywhere_and_a_name_start_hit_outranks_folders_first() {
        let e = vec![
            entry("notes.md", false),
            entry("assets", true),
            entry("sample.txt", false),
            entry("zz", true),
        ];
        let labels = |leaf: &str| -> Vec<String> {
            select_entries(&e, leaf)
                .into_iter()
                .map(|c| c.label)
                .collect()
        };
        assert_eq!(labels("s"), vec!["sample.txt", "assets/", "notes.md"]);
        assert_eq!(labels(""), vec!["assets/", "zz/", "notes.md", "sample.txt"]);
    }

    #[test]
    fn a_dot_name_stays_hidden_until_the_leaf_starts_with_a_dot() {
        let e = vec![
            entry(".env", false),
            entry("venv", true),
            entry("env.example", false),
        ];
        assert_eq!(
            inserts(&select_entries(&e, "env")),
            vec!["env.example", "venv/"],
            ".env holds `env` but stays hidden"
        );
        assert_eq!(inserts(&select_entries(&e, ".env")), vec![".env"]);
    }

    #[test]
    fn replacement_covers_every_branch() {
        let skill = cand("cr-review");
        assert_eq!(replacement(Trigger::Slash, "", &skill, false), "cr-review ");
        assert_eq!(replacement(Trigger::Slash, "", &skill, true), "cr-review");
        let folder = cand("tui/");
        assert_eq!(replacement(Trigger::At, "src/", &folder, false), "src/tui/");
        assert_eq!(replacement(Trigger::At, "src/", &folder, true), "src/tui/");
        let file = cand("mod.rs");
        assert_eq!(
            replacement(Trigger::At, "src/", &file, false),
            "src/mod.rs "
        );
        assert_eq!(replacement(Trigger::At, "src/", &file, true), "src/mod.rs");
    }

    #[test]
    fn an_agent_pick_ends_with_a_space_like_a_file() {
        let agent = select_agents(&[listed("sby-agent", None)], "")
            .pop()
            .expect("one agent");
        assert_eq!(
            replacement(Trigger::At, "", &agent, false),
            "agent-sby-agent "
        );
        assert_eq!(
            replacement(Trigger::At, "", &agent, true),
            "agent-sby-agent"
        );
    }

    #[test]
    fn the_highlight_wraps_both_ways() {
        assert_eq!(move_highlight(2, 3, true), 0);
        assert_eq!(move_highlight(0, 3, false), 2);
        assert_eq!(move_highlight(1, 3, true), 2);
        assert_eq!(move_highlight(1, 3, false), 0);
        assert_eq!(move_highlight(0, 0, true), 0);
    }

    #[test]
    fn the_highlight_clamps_and_resets_on_a_new_token() {
        assert_eq!(settle_highlight(5, 3, false), 2);
        assert_eq!(settle_highlight(1, 3, false), 1);
        assert_eq!(settle_highlight(2, 3, true), 0);
        assert_eq!(settle_highlight(2, 0, false), 0);
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "snapback-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn list_tree_of_a_missing_folder_is_empty() {
        assert!(list_tree(Path::new("/no/such/snapback-dir")).is_empty());
    }

    #[test]
    fn a_query_matches_the_full_path_not_only_the_name() {
        let e = vec![
            entry("src", true),
            entry("src/store", true),
            entry("src/store/discover.rs", false),
            entry("src/store/mod.rs", false),
        ];
        assert_eq!(
            inserts(&select_entries(&e, "disc")),
            vec!["src/store/discover.rs"]
        );
        // A folder component matches too, folders first within the tier.
        assert_eq!(
            inserts(&select_entries(&e, "store")),
            vec!["src/store/", "src/store/discover.rs", "src/store/mod.rs"]
        );
    }

    #[test]
    fn a_name_start_outranks_a_name_hit_outranks_a_path_only_hit() {
        let e = vec![
            entry("lib/store", true),
            entry("lib/store/zz.rs", false),
            entry("lib/mystore.rs", false),
            entry("store.rs", false),
        ];
        assert_eq!(
            inserts(&select_entries(&e, "store")),
            vec![
                "lib/store/",
                "store.rs",
                "lib/mystore.rs",
                "lib/store/zz.rs"
            ]
        );
    }

    #[test]
    fn an_empty_leaf_lists_direct_children_only_and_dot_components_stay_hidden() {
        let e = vec![
            entry("src", true),
            entry("src/a.rs", false),
            entry(".git", true),
            entry(".git/config", false),
        ];
        assert_eq!(inserts(&select_entries(&e, "")), vec!["src/"]);
        assert_eq!(inserts(&select_entries(&e, "conf")), Vec::<&str>::new());
        assert_eq!(
            inserts(&select_entries(&e, ".git")),
            vec![".git/", ".git/config"]
        );
    }

    #[test]
    fn list_tree_walks_every_depth_and_skips_vendored_folders() {
        let dir = temp_dir("complete-tree");
        std::fs::create_dir_all(dir.join("src/store")).unwrap();
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::create_dir_all(dir.join(".git/objects")).unwrap();
        // Ambiguous names stay walked; every other skipped name is pinned here.
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/run.sh"), "x").unwrap();
        for skipped in [
            "node_modules",
            "dist",
            "build",
            "coverage",
            "storybook-static",
            "bower_components",
            "__pycache__",
            "venv",
            "site-packages",
            "obj",
            "Pods",
            "DerivedData",
        ] {
            std::fs::create_dir_all(dir.join(skipped).join("x")).unwrap();
            std::fs::write(dir.join(skipped).join("x/junk"), "x").unwrap();
        }
        std::fs::write(dir.join("src/store/discover.rs"), "x").unwrap();
        std::fs::write(dir.join("target/debug/junk"), "x").unwrap();
        std::fs::write(dir.join(".git/objects/o"), "x").unwrap();
        let mut got: Vec<String> = list_tree(&dir).into_iter().map(|e| e.name).collect();
        got.sort();
        // Skipped folders are listed themselves, never descended into.
        assert_eq!(
            got,
            vec![
                ".git",
                "DerivedData",
                "Pods",
                "__pycache__",
                "bin",
                "bin/run.sh",
                "bower_components",
                "build",
                "coverage",
                "dist",
                "node_modules",
                "obj",
                "site-packages",
                "src",
                "src/store",
                "src/store/discover.rs",
                "storybook-static",
                "target",
                "venv"
            ]
        );
        assert_eq!(list_tree_capped(&dir, 2).len(), 2, "the entry cap holds");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_deep_path_survives_a_big_sibling() {
        // The reported shape: a monorepo whose target sits 9 levels down beside a
        // sibling bigger than the old 5000-entry cap, which BFS spends first.
        let dir = temp_dir("complete-deep");
        let deep = dir.join("apps/mono/src/modules/Layout/Page/ui/PageHeading");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("PageHeading.tsx"), "x").unwrap();
        let big = dir.join("packages/gen");
        std::fs::create_dir_all(&big).unwrap();
        for i in 0..5500 {
            std::fs::write(big.join(format!("f{i}.ts")), "x").unwrap();
        }
        let picked = select_entries(&list_tree(&dir), "PageHead");
        let got = inserts(&picked);
        assert_eq!(
            got,
            vec![
                "apps/mono/src/modules/Layout/Page/ui/PageHeading/",
                "apps/mono/src/modules/Layout/Page/ui/PageHeading/PageHeading.tsx",
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
