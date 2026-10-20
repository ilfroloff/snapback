//! Pick-list core for the compose box, shared by EVERY compose box (the `Ctrl-R`
//! reply, the `Ctrl-F` fork and the `Ctrl-N` background draft): which token the
//! caret is in, which candidates match it, and what accepting one writes. `/`
//! offers skills and commands; `@` offers files and folders and, for a top-level
//! token, agents. An `@` holding a `/` lists the folder it names first (the
//! FOLDER STEP) and falls back to the compose folder's whole walk only when that
//! finds nothing ([`at_list`]).
//! Each candidate carries its source's description, if any.
//!
//! Candidates are matched by the board's ONE matcher
//! ([`SearchIndex::admits`]), asked of a candidate's name and then its
//! description, with the token split into parts on [`QUERY_PART_SEPARATORS`]
//! ([`query_parts`]) — on [`ROOT_FALLBACK_SEPARATORS`], `/` included, for that
//! `@` fallback alone — each part one atom. An `@` file or folder the parts reject
//! may still match by the WORD STARTS of its own name
//! ([`SearchIndex::word_start_hit`], the folder step only: `@calis` finds
//! `CategoryListings.tsx`). What matches is ordered by [`Placement`] — the pick
//! list's own order, never the board's — and carries the marks the board's
//! row-label rule draws, or a word-start hit's chunks
//! ([`Candidate::label_marks`]). `memchr` and `nucleo` stay in `search.rs`, and so
//! does the word-start rule (AGENTS.md MATCHER ISOLATION). A pure core plus one
//! thin impure `read_dir` walker ([`list_tree`]); the caller decides WHEN to read
//! (once per folder, from the key handler, never the render path).
//!
//! Every column is a CHARACTER column (what `TextArea::cursor` reports), never
//! a byte offset, so multi-byte text before the caret cannot mis-cut a token.

use std::path::Path;

use crate::search::{MarkScratch, SearchIndex, WordStartHit};
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

/// Put before a command's name in its label, `/name`, as claude's own `/` menu
/// shows it.
const COMMAND_LABEL_PREFIX: &str = "/";

/// The characters a pick-list token is split into parts on, each part matched as
/// its own atom: every part must occur, in any order. They are the word
/// separators of the names the list offers — file names
/// (`sitemaps-storage.service.ts`, `test_page.py`), skill, command and agent
/// names (`cr-review`) — so `@site-stor` finds `sitemaps-storage.service.ts`.
/// Whitespace is not one, because it ends the token ([`completion_context`]);
/// nor is `/`, which is the `@` folder step ([`split_path_query`]) everywhere but
/// the root fallback ([`ROOT_FALLBACK_SEPARATORS`]). The board's own search box
/// does not split on these: whitespace is its only separator.
const QUERY_PART_SEPARATORS: [char; 3] = ['-', '_', '.'];

/// [`QUERY_PART_SEPARATORS`] plus `/`, for the `@` ROOT FALLBACK alone
/// ([`at_list`]). A query whose folder step found nothing is a path typed from
/// memory rather than a real folder, so its folder names are parts too:
/// `@modules/site-stor` is `modules`, `site` and `stor`. Kept out of the folder
/// step, where `/` resolves a real folder (`@/etc/ho`, `@../x/`, the
/// folder-by-folder walk). Built FROM the base set, so the two cannot drift: a
/// base separator added there fails to compile here until this is updated too.
const ROOT_FALLBACK_SEPARATORS: [char; 4] = {
    let [dash, underscore, dot] = QUERY_PART_SEPARATORS;
    [dash, underscore, dot, '/']
};

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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Candidate {
    pub label: String,
    pub insert: String,
    /// What the listing says a command or agent does; `None` for a file or
    /// folder, and for an entry its source gave no description.
    pub description: Option<String>,
    /// The CHAR positions of `label` the query matched, ascending, under the
    /// board's ROW-LABEL rule ([`SearchIndex::match_indices`]) asked of the name or
    /// path alone, so the label's decoration — a command's `/`, an agent's
    /// [`AGENT_LABEL_SUFFIX`], a folder's trailing `/` — is never marked; for an
    /// `@` entry admitted by the word starts of its name, the chunks that matched
    /// ([`WordStartHit::marks`]). Set when the list is filtered, so drawing it
    /// matches nothing.
    pub label_marks: Vec<usize>,
    /// The same, in `description`, for a candidate the query matched by its
    /// description alone; empty for every other.
    pub description_marks: Vec<usize>,
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
/// A WORD-START hit ranks in the tier it lands in, BELOW that tier's substring
/// hits, and among its own kind by its [`WordStartRank`]. See DOMAIN.md "Compose
/// pick list".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Placement {
    NameStart,
    /// A word-start hit whose first chunk begins the name: `CategoryListings.tsx`
    /// for `calis`. Only [`path_hit`] on the [`PathStep::Folder`] step produces it.
    WordStartNameStart(WordStartRank),
    InName,
    /// A word-start hit whose first chunk is on a later word of the name:
    /// `MyCategoryListings.tsx` for `calis`. Only [`path_hit`] on the
    /// [`PathStep::Folder`] step produces it.
    WordStartInName(WordStartRank),
    /// A path entry whose own name does not hold every part but whose path does
    /// (`src/store/discover.rs` for `store`, `store/mod.rs` for `store-mod`).
    /// Only [`select_paths`] (an `@` path, either step) produces it.
    InPath,
    InDescription,
}

/// How tight a word-start hit is ([`WordStartHit`]), declared so the derived order
/// is its tier's: fewer chunks, then fewer skipped words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct WordStartRank {
    chunks: usize,
    skipped: usize,
}

/// Which `@` step a path list serves ([`select_paths`]): the separators its query
/// is split on, and whether the WORD-START rule may admit a name the parts reject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathStep {
    /// The folder the query's `dir_part` names, matched by its leaf
    /// ([`select_entries`]): parts on [`QUERY_PART_SEPARATORS`], and word-start
    /// over each entry's own name.
    Folder,
    /// The ROOT FALLBACK ([`at_list`]): the WHOLE query on
    /// [`ROOT_FALLBACK_SEPARATORS`], substring parts alone. Its parts name folders
    /// as well as the file, so they are not one name's word starts; a word-start
    /// leaf is typed bare (`@calis`) instead.
    RootFallback,
}

impl PathStep {
    fn separators(self) -> &'static [char] {
        match self {
            PathStep::Folder => &QUERY_PART_SEPARATORS,
            PathStep::RootFallback => &ROOT_FALLBACK_SEPARATORS,
        }
    }
}

/// The board's own matcher, compiled for one pick-list token: each of its
/// [`query_parts`] on `separators` is one atom, under the board's per-atom smart
/// case. The parts go in through [`SearchIndex::set_atoms`], never as a
/// space-joined query, so a backslash in the token stays a literal char and never
/// escapes a space.
fn matcher(query: &str, separators: &[char]) -> SearchIndex {
    let mut matcher = SearchIndex::new();
    matcher.set_atoms(&query_parts(query, separators));
    matcher
}

/// The parts `query` is matched by: split on `separators` ([`QUERY_PART_SEPARATORS`],
/// or [`ROOT_FALLBACK_SEPARATORS`] for the `@` root fallback), empty parts dropped
/// (`site--stor`, a `site-` still being typed). A LEADING `.` stays on the first
/// part, so `.env` is the one part `.env` and a dot-name is still matched by its
/// dot: `@.env` lists `.env` as a name-start hit and never `env.example`. A token
/// of separators alone (`-`, `__`) has no part, so it is ONE part, matched whole:
/// no parts is the EMPTY query, which would admit every entry of the walk at
/// every depth.
fn query_parts<'q>(query: &'q str, separators: &[char]) -> Vec<&'q str> {
    // `.` is one byte, so `lead` is a char boundary.
    let lead = usize::from(query.starts_with('.'));
    let mut pieces = query[lead..].split(separators);
    // The first piece starts right after the leading dot, so widening it back to
    // byte 0 puts the dot on it.
    let first = pieces.next().map(|piece| &query[..lead + piece.len()]);
    let parts: Vec<&str> = first
        .into_iter()
        .chain(pieces)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() && !query.is_empty() {
        vec![query]
    } else {
        parts
    }
}

/// What `matcher` marks in `text` under the board's ROW-LABEL rule
/// ([`SearchIndex::match_indices`]: every part in `text`, then every occurrence of
/// each), each CHAR position moved `offset` chars right — past the decoration a
/// label draws before `text`.
fn marks_in(matcher: &SearchIndex, text: &str, offset: usize) -> Vec<usize> {
    matcher
        .match_indices(text)
        .into_iter()
        .map(|pos| pos as usize + offset)
        .collect()
}

/// Where `matcher`'s query lands in a candidate: its `name` (every part in it),
/// else its `description`, else `None`. The name STARTS with the query when any
/// part's hit covers its first char, whichever part that is. Asked of the NAME,
/// never the row's label (`/name`, `<name> (agent)`), so the decoration can never
/// match. An empty query admits every name and marks nothing, so every candidate
/// lands in ONE tier and keeps its source order.
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

/// Where `matcher`'s query lands in the path entry `rel`, with the CHAR positions
/// of `rel` its row marks. Judged on the entry's own name (the last component)
/// first, so a name that starts with the query outranks one that merely holds it,
/// then on the whole path, so a path that needs a folder component for some part
/// ranks below both. On the [`PathStep::Folder`] step a path the parts reject may
/// still be a WORD-START hit of its own name ([`SearchIndex::word_start_hit`]).
///
/// A substring hit is marked under the board's ROW-LABEL rule over the whole path
/// ([`SearchIndex::match_indices_with`]), and its name STARTS with the query when
/// one of those marks covers the name's first char, as in [`placement`]: every
/// part of a name hit lies in the name and so holds no `/`, so a mark there is a
/// part's hit starting there. A word-start hit marks its chunks instead.
fn path_hit(
    matcher: &SearchIndex,
    rel: &str,
    step: PathStep,
    scratch: &mut MarkScratch,
) -> Option<(Placement, Vec<usize>)> {
    let name_at = rel.rfind('/').map_or(0, |slash| slash + 1);
    let name = &rel[name_at..];
    let name_char = || {
        if rel.is_ascii() {
            name_at
        } else {
            rel[..name_at].chars().count()
        }
    };
    if matcher.admits_with(name, scratch) {
        let marks = matcher.match_indices_with(rel, scratch);
        let placement = if marks.binary_search(&name_char()).is_ok() {
            Placement::NameStart
        } else {
            Placement::InName
        };
        return Some((placement, marks));
    }
    if matcher.admits_with(rel, scratch) {
        return Some((Placement::InPath, matcher.match_indices_with(rel, scratch)));
    }
    if step == PathStep::RootFallback {
        return None;
    }
    let WordStartHit {
        at_name_start,
        chunks,
        skipped,
        marks,
    } = matcher.word_start_hit(name, scratch)?;
    let rank = WordStartRank { chunks, skipped };
    let placement = if at_name_start {
        Placement::WordStartNameStart(rank)
    } else {
        Placement::WordStartInName(rank)
    };
    let offset = name_char();
    Some((
        placement,
        marks.into_iter().map(|pos| pos + offset).collect(),
    ))
}

/// The listing entries `matcher` admits, each with where it landed, ordered by
/// [`Placement`]. The sort is stable, which is what keeps the caller's order
/// within a tier.
fn select_listed<'a>(
    matcher: &SearchIndex,
    entries: impl IntoIterator<Item = &'a ListingEntry>,
) -> Vec<(Placement, &'a ListingEntry)> {
    let mut picked: Vec<(Placement, &ListingEntry)> = entries
        .into_iter()
        .filter_map(|e| placement(matcher, &e.name, e.description.as_deref()).map(|p| (p, e)))
        .collect();
    picked.sort_by_key(|(placement, _)| *placement);
    picked
}

/// The row of a listing entry that landed at `placement`: its name between
/// `prefix` and `suffix`, its description, and the marks of whichever of the two
/// the query matched — the name's, moved past `prefix`, or for a description-only
/// hit the description's.
fn listed_candidate(
    matcher: &SearchIndex,
    (placement, entry): (Placement, &ListingEntry),
    (prefix, suffix): (&str, &str),
    insert: String,
) -> Candidate {
    let (label_marks, description_marks) = match (placement, entry.description.as_deref()) {
        (Placement::InDescription, Some(description)) => {
            (Vec::new(), marks_in(matcher, description, 0))
        }
        _ => (
            marks_in(matcher, &entry.name, prefix.chars().count()),
            Vec::new(),
        ),
    };
    Candidate {
        label: format!("{prefix}{}{suffix}", entry.name),
        insert,
        description: entry.description.clone(),
        label_marks,
        description_marks,
    }
}

/// Commands (skills included) whose name holds every part of `query` anywhere,
/// or else whose description does, matched like the board's search box (an
/// uppercase letter matches exactly). A name that starts with a part lists first,
/// then a name that holds them elsewhere, then a description hit, each tier in
/// the given order. `label` is `/name`; `insert` is the bare name (the space is
/// [`replacement`]'s); `description` is the entry's.
pub fn filter_commands(entries: &[ListingEntry], query: &str) -> Vec<Candidate> {
    let matcher = matcher(query, &QUERY_PART_SEPARATORS);
    select_listed(&matcher, entries)
        .into_iter()
        .map(|picked| {
            let insert = picked.1.name.clone();
            listed_candidate(&matcher, picked, (COMMAND_LABEL_PREFIX, ""), insert)
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
/// (typed in any case) is matched by what follows it, stripped BEFORE the query
/// is split into parts; any other query by itself, against the name and
/// description and never the label — so `@a` lists only the agents that hold an
/// `a` there, never every agent through the prefix or [`AGENT_LABEL_SUFFIX`].
/// `label` is the name plus [`AGENT_LABEL_SUFFIX`]; `insert` is
/// [`AGENT_MENTION_PREFIX`] plus the name.
pub fn select_agents(entries: &[ListingEntry], query: &str) -> Vec<Candidate> {
    let matcher = matcher(strip_mention_prefix(query), &QUERY_PART_SEPARATORS);
    select_listed(
        &matcher,
        entries.iter().filter(|e| is_mention_safe(&e.name)),
    )
    .into_iter()
    .map(|picked| {
        let insert = format!("{AGENT_MENTION_PREFIX}{}", picked.1.name);
        listed_candidate(&matcher, picked, ("", AGENT_LABEL_SUFFIX), insert)
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

/// The FOLDER STEP's `@` candidates for `query`: the entries of the folder its
/// `dir_part` names (`dir_entries`, read by the caller) that its leaf matches, in
/// [`select_entries`]' order, followed, ONLY for a top-level token (no
/// `dir_part`), by the matching [`select_agents`] — entries above agents
/// whatever their tiers. An agent is not a path, so `@src/…` never lists one.
/// [`at_list`] decides whether these rows are the list.
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

/// An `@` pick list: its rows, and the folder part [`replacement`] writes before
/// each pick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtList {
    pub candidates: Vec<Candidate>,
    /// The query's own `dir_part` ([`split_path_query`]) for the folder step;
    /// EMPTY for the root fallback, whose rows are full paths from the compose
    /// folder and so replace the WHOLE query.
    pub dir_part: String,
}

/// The `@` list for `query`, given `folder_hits`, the FOLDER STEP's rows
/// ([`at_candidates`] over the folder the query's `dir_part` names). Those rows
/// ARE the list when there are any, or when the query holds no `/`. Otherwise —
/// the folder is missing, empty or unreadable, or nothing in it matched — it is
/// the ROOT FALLBACK: the compose folder's own walk, which `root` reads only then,
/// matched by the WHOLE query split on [`ROOT_FALLBACK_SEPARATORS`] under
/// [`select_entries`]' gates and order, and inserted with an empty `dir_part`. It
/// never lists an agent: a query holding `/` is a path.
pub fn at_list<'r>(
    query: &str,
    folder_hits: Vec<Candidate>,
    root: impl FnOnce() -> &'r [DirEntryInfo],
) -> AtList {
    let (dir_part, _) = split_path_query(query);
    if folder_hits.is_empty() && !dir_part.is_empty() {
        AtList {
            candidates: select_paths(root(), query, PathStep::RootFallback),
            dir_part: String::new(),
        }
    } else {
        AtList {
            candidates: folder_hits,
            dir_part: dir_part.to_owned(),
        }
    }
}

/// Split an `@` query after its LAST `/` into `(dir_part, leaf)`:
/// `"src/tu"` -> `("src/", "tu")`, `"README"` -> `("", "README")`.
pub fn split_path_query(query: &str) -> (&str, &str) {
    match query.rfind('/') {
        Some(i) => query.split_at(i + 1),
        None => ("", query),
    }
}

/// Entries whose path holds every part of `leaf` anywhere (an uppercase letter
/// matches exactly, as in the board's search box): its own name or any folder
/// above it, so `disc` finds `store/discover.rs` — or whose own name the leaf
/// WORD-START matches under that same per-part case, so `calis` and `CaLis` find
/// `CategoryListings.tsx`. An empty `leaf` lists only the folder's direct
/// children, keeping the list a folder-by-folder walk. A dot-name (any component starting with `.`) shows only when `leaf`
/// itself starts with `.`, a visibility gate applied before matching. Both gates
/// read the raw `leaf`, never its parts. Ordered by [`path_hit`]'s [`Placement`]
/// (own name starts with a part, then holds them, then folder components do, a
/// word-start hit below the substring hits of its tier), then folders before
/// files, then by path. Paths are inserted VERBATIM; folders carry a trailing
/// `/`, which is never marked.
pub fn select_entries(entries: &[DirEntryInfo], leaf: &str) -> Vec<Candidate> {
    select_paths(entries, leaf, PathStep::Folder)
}

/// [`select_entries`] for either `step`: the folder step's leaf, or the root
/// fallback's WHOLE query, split on that step's separators. Both gates read the
/// raw `query`.
///
/// It runs on every keystroke over a walk of up to [`COMPLETION_MAX_TREE_ENTRIES`],
/// so the pass allocates nothing per entry it rejects: ONE [`MarkScratch`] serves
/// every question asked of every entry, an admitted row's marks are computed once,
/// and the walk arrives sorted. Measured 2026-10-19 (release build, Apple M4 Max,
/// median / p95 of 101 runs over a synthetic 25,000-entry monorepo walk): a
/// one-char `@c` or `@s`, which admits nearly every entry, took 16.5 / 17.7 ms and
/// 18.0 / 18.8 ms — past one 60 Hz frame — while every question allocated its own
/// buffers, the fold pushed one char at a time and every keystroke sorted the
/// whole walk by path, and takes 6.8 / 7.1 ms and 7.9 / 8.3 ms this way. The
/// word-start rule's own share of a longer query is 0.2-1.1 ms over that walk and
/// two real ones (10,084 and 25,000 entries).
fn select_paths(entries: &[DirEntryInfo], query: &str, step: PathStep) -> Vec<Candidate> {
    let matcher = matcher(query, step.separators());
    let show_hidden = query.starts_with('.');
    let mut scratch = MarkScratch::new();
    let mut picked: Vec<(Placement, &DirEntryInfo, Vec<usize>)> = entries
        .iter()
        .filter(|e| !query.is_empty() || !e.name.contains('/'))
        .filter(|e| show_hidden || !has_dot_component(&e.name))
        .filter_map(|e| {
            path_hit(&matcher, &e.name, step, &mut scratch)
                .map(|(placement, marks)| (placement, e, marks))
        })
        .collect();
    // Ordered by placement, then by `listing_order`: stably by the second, then
    // stably by the first. A walk arrives in `listing_order` already
    // ([`list_tree`]), so the first sort is one pass that confirms it, and the
    // second compares tiers alone, never two long paths.
    picked.sort_by(|(_, a, _), (_, b, _)| listing_order(a, b));
    picked.sort_by_key(|(placement, _, _)| *placement);
    picked
        .into_iter()
        .map(|(_, e, label_marks)| {
            let text = if e.is_dir {
                format!("{}/", e.name)
            } else {
                e.name.clone()
            };
            Candidate {
                label: text.clone(),
                insert: text,
                label_marks,
                ..Candidate::default()
            }
        })
        .collect()
}

/// Whether any `/`-separated component of `rel` starts with `.` — the dot-name
/// gate of [`select_paths`].
fn has_dot_component(rel: &str) -> bool {
    rel.starts_with('.') || rel.contains("/.")
}

/// The order `@` entries list in within one tier: folders before files, then by
/// path.
fn listing_order(a: &DirEntryInfo, b: &DirEntryInfo) -> std::cmp::Ordering {
    b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name))
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

/// Entries of `dir` and the folders below it, as `/`-relative paths, walked
/// breadth first and bounded by [`COMPLETION_MAX_TREE_ENTRIES`], then returned in
/// [`listing_order`] — the order a tier of the `@` list shows, so the walk is
/// sorted once per read rather than on every keystroke ([`select_paths`]). Never
/// descends a symlink (no cycles), a dot-folder or a [`TREE_SKIPPED_DIRS`]
/// folder. FAIL-SOFT: a missing or unreadable folder is empty and a failing entry
/// is skipped.
pub fn list_tree(dir: &Path) -> Vec<DirEntryInfo> {
    list_tree_capped(dir, COMPLETION_MAX_TREE_ENTRIES)
}

fn list_tree_capped(root: &Path, cap: usize) -> Vec<DirEntryInfo> {
    let mut out = walk_breadth_first(root, cap);
    out.sort_by(listing_order);
    out
}

fn walk_breadth_first(root: &Path, cap: usize) -> Vec<DirEntryInfo> {
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
            ..Candidate::default()
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
                label_marks: vec![1],
                description_marks: Vec::new(),
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
                label_marks: vec![0, 1],
                description_marks: Vec::new(),
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

    /// The walk comes back in the order a tier lists in — folders first, then by
    /// path — not in the breadth-first order it was read in, so a keystroke's sort
    /// over it is one confirming pass.
    #[test]
    fn list_tree_returns_folders_first_then_by_path() {
        let dir = temp_dir("complete-order");
        std::fs::create_dir_all(dir.join("c/deep")).unwrap();
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::write(dir.join("b.txt"), "x").unwrap();
        std::fs::write(dir.join("A.txt"), "x").unwrap();
        std::fs::write(dir.join("a/x.txt"), "x").unwrap();
        let got: Vec<String> = list_tree(&dir).into_iter().map(|e| e.name).collect();
        assert_eq!(
            got,
            vec!["a", "c", "c/deep", "A.txt", "a/x.txt", "b.txt"],
            "a breadth-first order would put every top-level entry before `a/x.txt`"
        );
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

    // --- a token matched by its parts ---------------------------------------

    /// The reported shape: a NestJS-style module tree.
    fn sitemaps_tree() -> Vec<DirEntryInfo> {
        vec![
            entry("src", true),
            entry("src/modules", true),
            entry("src/modules/sitemaps", true),
            entry("src/modules/sitemaps/sitemaps-storage.service.ts", false),
            entry("src/modules/sitemaps/sitemaps.controller.ts", false),
            entry("src/modules/storage", true),
            entry("src/modules/storage/site-settings.ts", false),
        ]
    }

    /// `@site-stor` is two parts, each a substring of the path, in either order;
    /// the file whose own name holds both lists first, then a path that holds them
    /// across a folder and a name.
    #[test]
    fn a_token_matches_by_its_parts_in_any_order() {
        let tree = sitemaps_tree();
        let expected = vec![
            "src/modules/sitemaps/sitemaps-storage.service.ts",
            "src/modules/storage/site-settings.ts",
        ];
        assert_eq!(inserts(&at_candidates(&tree, &[], "site-stor")), expected);
        assert_eq!(
            inserts(&at_candidates(&tree, &[], "stor-site")),
            expected,
            "part order does not matter"
        );
        assert_eq!(
            inserts(&at_candidates(&tree, &[], "sitemaps-storage")),
            vec!["src/modules/sitemaps/sitemaps-storage.service.ts"],
            "the full name typed still matches"
        );
    }

    /// Substring per part, never fuzzy: a misspelt part finds nothing.
    #[test]
    fn a_misspelt_part_finds_nothing() {
        assert_eq!(
            inserts(&at_candidates(&sitemaps_tree(), &[], "site-stro")),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn underscore_and_dot_split_a_token_too() {
        let e = vec![
            entry("src/sitemaps-storage.service.ts", false),
            entry("tests/test_page_heading.py", false),
            entry("README.md", false),
        ];
        assert_eq!(
            inserts(&select_entries(&e, "stor.ts")),
            vec!["src/sitemaps-storage.service.ts"]
        );
        assert_eq!(
            inserts(&select_entries(&e, "head_test")),
            vec!["tests/test_page_heading.py"]
        );
    }

    /// A LEADING `.` belongs to the first part, so `@.env` still matches `.env`
    /// by its dot (never `env.example` or `venv/` by a bare `env`), lists it as a
    /// name-start hit, and keeps the dot-name gate.
    #[test]
    fn a_leading_dot_stays_part_of_the_first_part() {
        let e = vec![
            entry("env.example", false),
            entry("venv", true),
            entry(".env.local", false),
            entry("config/.env", false),
            entry(".env", false),
        ];
        assert_eq!(
            inserts(&select_entries(&e, ".env")),
            vec![".env", ".env.local", "config/.env"]
        );
        assert_eq!(inserts(&select_entries(&e, ".env.loc")), vec![".env.local"]);
        assert_eq!(
            inserts(&select_entries(&e, "env")),
            vec!["env.example", "venv/"],
            "with no leading dot the dot-names stay hidden"
        );
    }

    /// A token of separators alone has no part to split into, so it is matched
    /// whole, as before parts existed — never as an empty query that would list
    /// every entry of the walk at every depth.
    #[test]
    fn a_token_of_separators_alone_is_matched_whole() {
        let e = vec![
            entry("src", true),
            entry("src/cr-review.md", false),
            entry("src/__init__.py", false),
            entry("notes.txt", false),
        ];
        assert_eq!(inserts(&select_entries(&e, "-")), vec!["src/cr-review.md"]);
        assert_eq!(inserts(&select_entries(&e, "__")), vec!["src/__init__.py"]);
        let agents = vec![listed("sby-agent", None), listed("Explore", None)];
        assert_eq!(
            inserts(&select_agents(&agents, "-")),
            vec!["agent-sby-agent"]
        );
    }

    /// `agent-` is stripped BEFORE the split, so the parts are what follows it: an
    /// `agent` part would match neither the name nor the description here.
    #[test]
    fn the_mention_prefix_is_stripped_before_the_split() {
        let agents = vec![
            listed("explore-docs", Some("Reads the docs")),
            listed("sby-agent", None),
        ];
        assert_eq!(
            inserts(&select_agents(&agents, "agent-exp-doc")),
            vec!["agent-explore-docs"]
        );
        assert_eq!(
            inserts(&select_agents(&agents, "AGENT-doc-exp")),
            vec!["agent-explore-docs"]
        );
    }

    /// Smart case per PART: `Site` matches exactly, `stor` in any case — so a
    /// per-token rule (one uppercase letter makes every part exact) would miss
    /// `SiteMap-STORAGE.ts`. The word-start rule decides case the same way, so
    /// `sitemap-storage.ts` is no word-start hit for `Site-stor` either.
    #[test]
    fn smart_case_is_decided_per_part() {
        let e = vec![
            entry("SiteMap-STORAGE.ts", false),
            entry("sitemap-storage.ts", false),
        ];
        assert_eq!(
            inserts(&select_entries(&e, "Site-stor")),
            vec!["SiteMap-STORAGE.ts"]
        );
        let site = matcher("Site-stor", &QUERY_PART_SEPARATORS);
        assert!(
            !site.admits("sitemap-storage.ts"),
            "the substring parts reject it: `Site` matches exactly"
        );
        assert_eq!(
            path_hit(
                &site,
                "sitemap-storage.ts",
                PathStep::Folder,
                &mut MarkScratch::new()
            ),
            None,
            "and so do its word starts: `Site` meets `sitemap` exactly too"
        );
        assert_eq!(
            inserts(&select_entries(&e, "site-STOR")),
            vec!["SiteMap-STORAGE.ts"]
        );
        assert_eq!(
            inserts(&select_entries(&e, "site-stor")),
            vec!["SiteMap-STORAGE.ts", "sitemap-storage.ts"]
        );
    }

    /// A backslash in a token is a literal character of its part, never an escape:
    /// `foo\-bar` is the parts `foo\` and `bar`, so it never becomes the escaped
    /// phrase `foo bar` the board's search box would read in `foo\ bar`.
    #[test]
    fn a_backslash_is_literal_in_a_part() {
        let e = vec![entry("foo bar.txt", false), entry(r"foo\-bar.txt", false)];
        assert_eq!(
            inserts(&select_entries(&e, r"foo\-bar")),
            vec![r"foo\-bar.txt"]
        );
    }

    /// A name STARTS with the query when any part's hit sits at its first char,
    /// whichever part that is.
    #[test]
    fn a_name_start_is_any_parts_hit_at_the_first_char() {
        let commands = vec![
            listed("my-site-storage", None),
            listed("storage-site", None),
            listed("site-storage", None),
        ];
        assert_eq!(
            inserts(&filter_commands(&commands, "site-stor")),
            vec!["storage-site", "site-storage", "my-site-storage"]
        );
    }

    // --- word-start (camel-hump) matching -------------------------------------

    /// The reported shape: a CamelCase file deep in a monorepo, beside a name
    /// that holds the same letters in order without starting words with them.
    fn product_pages_tree() -> Vec<DirEntryInfo> {
        vec![
            entry("apps", true),
            entry("apps/mono", true),
            entry("apps/mono/src", true),
            entry("apps/mono/src/modules", true),
            entry("apps/mono/src/modules/ProductPages", true),
            entry("apps/mono/src/modules/ProductPages/CategoryListings", true),
            entry(
                "apps/mono/src/modules/ProductPages/CategoryListings/CategoryListings.tsx",
                false,
            ),
            entry(
                "apps/mono/src/modules/ProductPages/CategoryListings/index.ts",
                false,
            ),
            entry("packages", true),
            entry("packages/contract-alias", true),
            entry("packages/contract-alias/index.ts", false),
        ]
    }

    /// `@calis` and `@CaLis` hold no substring of `CategoryListings.tsx`, so only
    /// the word-start rule finds it, as `Ca` + `Lis` of its OWN name: the marks are
    /// those chunks, never the folder above that spells the same words.
    #[test]
    fn a_camel_case_file_is_found_by_its_word_starts() {
        for query in ["calis", "CaLis"] {
            assert_eq!(
                marked_labels(&at_candidates(&product_pages_tree(), &[], query)),
                vec![
                    "apps/mono/src/modules/ProductPages/[Ca]tegory[Lis]tings/",
                    "apps/mono/src/modules/ProductPages/CategoryListings/[Ca]tegory[Lis]tings.tsx",
                ],
                "@{query}: `contract-alias` holds the letters in order, and is no hit"
            );
        }
    }

    /// Smart case binds a word-start hit as it binds a substring one, per part: a
    /// capital meets a capital alone. So `@C` and `@Ca` keep `cat.rs` out, `@CaLis`
    /// lists `CategoryListings.tsx` alone where `@calis` lists every name whose
    /// word starts spell it, and `@CaLi-st` holds `Ca` + `Li` exact while its `st`
    /// meets `Store` and `STORE` alike.
    #[test]
    fn a_capital_matches_exactly_in_a_word_start_hit_too() {
        let cat = vec![entry("cat.rs", false)];
        for query in ["C", "Ca"] {
            assert_eq!(
                inserts(&select_entries(&cat, query)),
                Vec::<&str>::new(),
                "@{query}"
            );
        }
        let e = vec![
            entry("ClickAwayListener", false),
            entry("CategoryListings.tsx", false),
            entry("categoryListings.tsx", false),
        ];
        assert_eq!(
            inserts(&select_entries(&e, "CaLis")),
            vec!["CategoryListings.tsx"],
            "`ClickAwayListener` would need its `a` to meet `Away`"
        );
        assert_eq!(
            inserts(&select_entries(&e, "calis")),
            vec![
                "CategoryListings.tsx",
                "categoryListings.tsx",
                "ClickAwayListener"
            ]
        );
        let stores = vec![
            entry("CategoryLinkStore.ts", false),
            entry("CategoryLinkSTORE.ts", false),
            entry("categoryLinkStore.ts", false),
            entry("CategoryLINKStore.ts", false),
        ];
        assert_eq!(
            inserts(&select_entries(&stores, "CaLi-st")),
            vec!["CategoryLinkSTORE.ts", "CategoryLinkStore.ts"]
        );
    }

    /// Each word-start tier sits right below the substring tier it lands in: a
    /// substring name start, then a word-start name start, then a substring hit
    /// elsewhere in the name, then a word-start hit on a later word, then a path.
    #[test]
    fn a_word_start_hit_ranks_below_the_substring_hits_of_its_tier() {
        let e = vec![
            entry("calis/zz.ts", false),
            entry("MyCategoryListings.tsx", false),
            entry("xcalis.ts", false),
            entry("CategoryListings.tsx", false),
            entry("CalisThing.ts", false),
        ];
        assert_eq!(
            inserts(&select_entries(&e, "calis")),
            vec![
                "CalisThing.ts",
                "CategoryListings.tsx",
                "xcalis.ts",
                "MyCategoryListings.tsx",
                "calis/zz.ts",
            ]
        );
    }

    /// Within a word-start tier, fewer chunks first, then fewer skipped words, and
    /// only then folders first and the path: `ClickAwayListener` (`c` + `a` +
    /// `lis`) lists below both `Category…Listings` names though it is a folder, and
    /// `CategoryFooListings` skips a word, so it follows `CategoryListings`.
    #[test]
    fn word_start_hits_order_by_fewest_chunks_then_fewest_skipped_words() {
        let e = vec![
            entry("ClickAwayListener", true),
            entry("CategoryFooListings.tsx", false),
            entry("CategoryListings.tsx", false),
        ];
        assert_eq!(
            marked_labels(&select_entries(&e, "calis")),
            vec![
                "[Ca]tegory[Lis]tings.tsx",
                "[Ca]tegoryFoo[Lis]tings.tsx",
                "[C]lick[A]way[Lis]tener/",
            ]
        );
    }

    /// The ROOT FALLBACK stays substring: its parts name folders as well as the
    /// file, so `@ProductPages/calis` finds nothing, while the folder step on the
    /// real folder does word-start match its leaf.
    #[test]
    fn the_root_fallback_lists_no_word_start_hit() {
        let tree = product_pages_tree();
        assert_eq!(
            written(&at_list_over(&tree, &[], "ProductPages/calis")),
            Vec::<String>::new()
        );
        // `cat` + `calis` WOULD word-start match `CatCategoryListings.tsx` (`Cat` +
        // `Ca` + `Lis`), and `calis` is no substring, so only the rule's absence
        // keeps the fallback empty; the folder step on `src/` does find it.
        let cats = vec![
            entry("src", true),
            entry("src/CatCategoryListings.tsx", false),
        ];
        assert_eq!(
            written(&at_list_over(&cats, &[], "cat/calis")),
            Vec::<String>::new()
        );
        assert_eq!(
            written(&at_list_over(&cats, &[], "src/calis")),
            vec!["src/CatCategoryListings.tsx "]
        );
        assert_eq!(
            written(&at_list_over(
                &tree,
                &[],
                "apps/mono/src/modules/ProductPages/calis"
            )),
            vec![
                "apps/mono/src/modules/ProductPages/CategoryListings/",
                "apps/mono/src/modules/ProductPages/CategoryListings/CategoryListings.tsx ",
            ]
        );
    }

    /// Word-start is for `@` files and folders alone: a command or an agent named
    /// in CamelCase is matched by substring, as before.
    #[test]
    fn commands_and_agents_list_no_word_start_hit() {
        let listing = vec![
            listed("CategoryListings", None),
            listed("category-listings", Some("Lists every category")),
        ];
        assert!(filter_commands(&listing, "calis").is_empty());
        assert!(select_agents(&listing, "calis").is_empty());
        assert_eq!(
            inserts(&at_candidates(
                &[entry("CategoryListings.tsx", false)],
                &listing,
                "calis"
            )),
            vec!["CategoryListings.tsx"],
            "the same query lists the file, so the agents' absence is the rule's"
        );
    }

    #[test]
    fn a_dot_component_is_any_component_starting_with_a_dot() {
        for dotted in [".env", ".git/config", "src/.env", "a/.b/c"] {
            assert!(has_dot_component(dotted), "{dotted}");
        }
        for plain in ["src/x.rs", "a.b/c", "a./b", "env.example", ""] {
            assert!(!has_dot_component(plain), "{plain}");
        }
    }

    // --- the `@` root fallback ----------------------------------------------

    /// The entries of `tree` below `folder` (`"src/"`), relative to it: what the
    /// folder step reads for a query whose `dir_part` is `folder`.
    fn under(tree: &[DirEntryInfo], folder: &str) -> Vec<DirEntryInfo> {
        tree.iter()
            .filter_map(|e| e.name.strip_prefix(folder).map(|rel| entry(rel, e.is_dir)))
            .collect()
    }

    /// The `@` list for `query` with `tree` as the compose folder's walk: the
    /// folder step reads `tree` below the query's `dir_part` (all of it for a
    /// top-level token), the root fallback reads `tree` whole.
    fn at_list_over(tree: &[DirEntryInfo], agents: &[ListingEntry], query: &str) -> AtList {
        let (dir_part, _) = split_path_query(query);
        let folder = under(tree, dir_part);
        at_list(query, at_candidates(&folder, agents, query), || tree)
    }

    /// What picking each row writes after the `@`.
    fn written(list: &AtList) -> Vec<String> {
        list.candidates
            .iter()
            .map(|c| replacement(Trigger::At, &list.dir_part, c, false))
            .collect()
    }

    /// The reported shape: a path typed from memory names no real folder, so the
    /// folder step finds nothing and the WHOLE query, split on `/` too, is matched
    /// against the compose folder's walk. A pick writes the full path, replacing
    /// the whole query — never `modules/` + `src/modules/…`.
    #[test]
    fn a_slash_query_whose_folder_step_finds_nothing_falls_back_to_the_root_walk() {
        let tree = sitemaps_tree();
        let list = at_list_over(&tree, &[], "modules/site-stor");
        assert_eq!(list.dir_part, "", "a fallback row replaces the whole query");
        assert_eq!(
            written(&list),
            vec![
                "src/modules/sitemaps/sitemaps-storage.service.ts ",
                "src/modules/storage/site-settings.ts ",
            ]
        );
        assert_eq!(
            written(&at_list_over(&tree, &[], "sitemaps/stor")),
            vec!["src/modules/sitemaps/sitemaps-storage.service.ts "]
        );
    }

    /// `@sitemaps/` names no folder at the root, so it lists every path holding
    /// `sitemaps`, folders first; a folder pick writes `path/` with no space, so
    /// the list reopens on that REAL folder through the folder step.
    #[test]
    fn a_slash_query_naming_no_real_folder_lists_every_path_holding_it() {
        assert_eq!(
            written(&at_list_over(&sitemaps_tree(), &[], "sitemaps/")),
            vec![
                "src/modules/sitemaps/",
                "src/modules/sitemaps/sitemaps-storage.service.ts ",
                "src/modules/sitemaps/sitemaps.controller.ts ",
            ]
        );
    }

    /// The folder step comes first: when the folder the query names lists
    /// anything, that IS the list, inserted under the query's own `dir_part`, and
    /// the root walk is never read. An empty leaf still lists only the folder's
    /// direct children.
    #[test]
    fn a_folder_step_that_finds_something_wins_over_the_root_fallback() {
        let tree = sitemaps_tree();
        let src = under(&tree, "src/");
        let root_read = std::cell::Cell::new(false);
        let root = || {
            root_read.set(true);
            tree.as_slice()
        };
        let list = at_list(
            "src/site-stor",
            at_candidates(&src, &[], "src/site-stor"),
            root,
        );
        assert_eq!(list.dir_part, "src/");
        assert_eq!(
            inserts(&list.candidates),
            vec![
                "modules/sitemaps/sitemaps-storage.service.ts",
                "modules/storage/site-settings.ts",
            ],
            "rows relative to the folder, as before"
        );
        let list = at_list("src/", at_candidates(&src, &[], "src/"), || tree.as_slice());
        assert_eq!(
            (list.dir_part.as_str(), inserts(&list.candidates)),
            ("src/", vec!["modules/"]),
            "an empty leaf lists the direct children alone"
        );
        assert!(
            !root_read.get(),
            "the root walk is read only for a fallback"
        );
    }

    /// An absolute or parent folder part that lists anything is the folder step,
    /// untouched by the fallback.
    #[test]
    fn an_absolute_or_parent_folder_that_matches_is_untouched() {
        let tree = sitemaps_tree();
        let etc = vec![entry("hosts", false), entry("hostname", false)];
        let list = at_list("/etc/ho", at_candidates(&etc, &[], "/etc/ho"), || {
            tree.as_slice()
        });
        assert_eq!(list.dir_part, "/etc/");
        assert_eq!(written(&list), vec!["/etc/hostname ", "/etc/hosts "]);
        let other = vec![entry("x.rs", false)];
        let list = at_list("../other/", at_candidates(&other, &[], "../other/"), || {
            tree.as_slice()
        });
        assert_eq!(written(&list), vec!["../other/x.rs "]);
    }

    /// An agent is not a path: a query holding `/` lists none in either mode,
    /// even one whose description holds what was typed.
    #[test]
    fn a_slash_query_never_lists_an_agent() {
        let agents = vec![listed("sby-agent", Some("Pings back/forth"))];
        assert_eq!(
            written(&at_list_over(&sitemaps_tree(), &agents, "back/")),
            Vec::<String>::new()
        );
        assert!(
            written(&at_list_over(&sitemaps_tree(), &agents, "sitemaps/"))
                .iter()
                .all(|w| w.starts_with("src/"))
        );
    }

    /// A fallback row is marked on its full path: every part, folder names
    /// included, every occurrence.
    #[test]
    fn a_fallback_row_is_marked_on_its_full_path() {
        assert_eq!(
            marked_labels(&at_list_over(&sitemaps_tree(), &[], "modules/site-stor").candidates),
            vec![
                "src/[modules]/[site]maps/[site]maps-[stor]age.service.ts",
                "src/[modules]/[stor]age/[site]-settings.ts",
            ]
        );
    }

    /// The dot-name gate reads the WHOLE query in fallback mode, and a leading `.`
    /// stays on its first part: `.gith` never matches `notgithub/` by a bare
    /// `gith`.
    #[test]
    fn the_fallback_shows_dot_names_only_for_a_query_starting_with_a_dot() {
        let tree = vec![
            entry(".github", true),
            entry(".github/workflows", true),
            entry(".github/workflows/ci.yml", false),
            entry("docs", true),
            entry("docs/workflows-ci.md", false),
            entry("notgithub", true),
            entry("notgithub/ci.md", false),
        ];
        assert_eq!(
            written(&at_list_over(&tree, &[], "workflows/ci")),
            vec!["docs/workflows-ci.md "]
        );
        assert_eq!(
            written(&at_list_over(&tree, &[], ".gith/ci")),
            vec![".github/workflows/ci.yml "]
        );
    }

    /// Every part of the WHOLE query must sit in one path, so parts spread over
    /// different paths list nothing, and neither does a query the walk lacks.
    #[test]
    fn a_query_neither_step_matches_lists_nothing() {
        let tree = sitemaps_tree();
        assert_eq!(
            written(&at_list_over(&tree, &[], "sitemaps/settings")),
            Vec::<String>::new()
        );
        assert_eq!(
            written(&at_list_over(&tree, &[], "nope/zzz")),
            Vec::<String>::new()
        );
    }

    // --- marks ----------------------------------------------------------------

    /// `text` with each run of `marks` (CHAR positions) wrapped in `[` `]`, so a
    /// test reads the marks as text.
    fn bracketed(text: &str, marks: &[usize]) -> String {
        let mut out = String::new();
        let mut open = false;
        for (pos, c) in text.chars().enumerate() {
            let hit = marks.contains(&pos);
            if hit != open {
                out.push(if hit { '[' } else { ']' });
                open = hit;
            }
            out.push(c);
        }
        if open {
            out.push(']');
        }
        out
    }

    /// Every row's label, marked.
    fn marked_labels(candidates: &[Candidate]) -> Vec<String> {
        candidates
            .iter()
            .map(|c| bracketed(&c.label, &c.label_marks))
            .collect()
    }

    /// A path marks EVERY occurrence of every part, folder components included —
    /// the board's row-label rule — and a folder's trailing `/` never.
    #[test]
    fn a_path_marks_every_occurrence_of_every_part() {
        assert_eq!(
            marked_labels(&at_candidates(&sitemaps_tree(), &[], "site-stor")),
            vec![
                "src/modules/[site]maps/[site]maps-[stor]age.service.ts",
                "src/modules/[stor]age/[site]-settings.ts",
            ]
        );
        assert_eq!(
            marked_labels(&select_entries(&sitemaps_tree(), "maps")),
            vec![
                "src/modules/site[maps]/",
                "src/modules/site[maps]/site[maps]-storage.service.ts",
                "src/modules/site[maps]/site[maps].controller.ts",
            ]
        );
        assert_eq!(
            marked_labels(&select_entries(&[entry(".env.local", false)], ".env.loc")),
            vec!["[.env].[loc]al"],
            "a leading dot is marked with its part"
        );
    }

    /// A command's marks sit one char right of its name's, past the `/`.
    #[test]
    fn a_command_mark_is_moved_past_its_slash() {
        let commands = vec![listed("cr-review", None), listed("review", None)];
        assert_eq!(
            marked_labels(&filter_commands(&commands, "cr-rev")),
            vec!["/[cr]-[rev]iew"]
        );
        assert_eq!(
            marked_labels(&filter_commands(&commands, "re")),
            vec!["/[re]view", "/cr-[re]view"]
        );
    }

    /// An agent is marked in its NAME: an `agent` part marks `sby-agent`'s own
    /// `agent`, never the ` (agent)` the label adds.
    #[test]
    fn an_agent_mark_never_reaches_its_label_suffix() {
        let agents = vec![listed("sby-agent", None)];
        assert_eq!(
            marked_labels(&select_agents(&agents, "agent")),
            vec!["sby-[agent] (agent)"]
        );
        assert_eq!(
            marked_labels(&select_agents(&agents, "agent-sby-ag")),
            vec!["[sby]-[ag]ent (agent)"]
        );
    }

    /// A description-only hit marks its description, and its label not at all; a
    /// name hit marks the name alone, even when the description holds the query.
    #[test]
    fn a_description_hit_marks_the_description_alone() {
        let commands = vec![
            listed("zeta", Some("Review a change")),
            listed("review", Some("review it")),
        ];
        let got = filter_commands(&commands, "rev-chan");
        assert_eq!(marked_labels(&got), vec!["/zeta"]);
        assert_eq!(
            bracketed(
                got[0].description.as_deref().unwrap(),
                &got[0].description_marks
            ),
            "[Rev]iew a [chan]ge"
        );
        let got = filter_commands(&commands, "rev");
        assert_eq!(marked_labels(&got), vec!["/[rev]iew", "/zeta"]);
        assert!(
            got[0].description_marks.is_empty(),
            "a name hit leaves its description unmarked"
        );
        assert_eq!(
            bracketed(
                got[1].description.as_deref().unwrap(),
                &got[1].description_marks
            ),
            "[Rev]iew a change"
        );
    }

    /// No query, no marks — the list is the folder walk, not a search.
    #[test]
    fn an_empty_query_marks_nothing() {
        let entries = at_candidates(
            &sitemaps_tree(),
            &[listed("sby-agent", Some("Pings back"))],
            "",
        );
        assert!(!entries.is_empty());
        assert!(entries
            .iter()
            .all(|c| c.label_marks.is_empty() && c.description_marks.is_empty()));
    }
}
