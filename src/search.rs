//! Substring search over sessions (nucleo).
//!
//! Isolates every `nucleo` call so an API change touches one module (Risks
//! table). Builds a matcher over each session's searchable haystack
//! (name/label AND the capped `content_index`) and exposes a [`filter`] that
//! returns ranked indices. Supports two modes behind one interface:
//! name/label-only (default, instant) and name+content; matching stays
//! incremental (re-filter on each keystroke without rebuilding the haystack),
//! and the haystack rebuilds only for changed sessions on `SessionsChanged`.
//!
//! # Which nucleo API
//!
//! `nucleo = 0.5.0` re-exports the finished low-level `nucleo-matcher` types
//! (`Pattern`, `Atom`/`AtomKind`, `Matcher`, `Config`, `Utf32Str`,
//! `CaseMatching`, `Normalization`). We deliberately use that synchronous
//! low-level path rather than the high-level threaded `Nucleo`/`Injector`
//! worker: at this scale (~170 sessions) a per-keystroke score pass is instant,
//! and a synchronous matcher gives deterministic ranking (no background
//! `tick()`/snapshot races) that is trivial to unit-test.
//!
//! Matching is **substring, not fuzzy**: each keystroke rebuilds the small
//! [`Pattern`] via [`Pattern::new`] with a fixed [`AtomKind::Substring`], so a
//! query matches only where it appears as a contiguous run (case-insensitive,
//! smart-case) — the atom kind is forced in code and never depends on user-typed
//! atom syntax. Incrementality still comes from reusing one [`Matcher`] and the
//! prebuilt haystack strings across keystrokes (only the tiny pattern is
//! rebuilt); a `SessionsChanged` refresh rebuilds only the entries whose session
//! actually changed.
//!
//! Everything nucleo-shaped is contained below. The rest of the crate sees only
//! [`SearchIndex`], [`SearchMode`], and [`filter`].
//!
//! # A note on `#[allow(dead_code)]`
//!
//! `snapback` is a *binary* crate, so `pub` does not make an item reachable — the
//! `dead_code` lint fires on any public API the `main` runtime path does not
//! call, even when the item is fully exercised by this module's unit tests. A
//! few items below are exactly that: the deliberate, unit-tested search API
//! surface (the single nucleo isolation seam per the Risks table) that the TUI
//! either reaches through a sibling method or does not yet consume. Each such
//! item carries a *narrowly-scoped* `#[allow(dead_code)]` with a reason — never
//! a crate- or module-wide blanket — so the lint stays sharp everywhere else.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use nucleo::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher, Utf32Str};

use crate::store::Session;

/// Which haystack the matcher scores against.
///
/// The two modes are the "one interface, two modes" contract: the default is
/// name/label-only (instant, the common case); toggling in content widens the
/// haystack to include the capped `content_index`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchMode {
    /// Match the session name/label only (default). Fast: a short haystack.
    #[default]
    NameOnly,
    /// Match the name/label AND the capped `content_index` transcript text.
    NameAndContent,
}

impl SearchMode {
    /// Flip between the two modes (bound to a keypress by the TUI).
    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            SearchMode::NameOnly => SearchMode::NameAndContent,
            SearchMode::NameAndContent => SearchMode::NameOnly,
        }
    }

    /// Whether this mode scores against `content_index` as well as the name.
    // The view layer matches the enum directly; this predicate is the tested
    // form of that check, kept for callers that want a boolean.
    #[allow(dead_code)]
    #[must_use]
    pub fn includes_content(self) -> bool {
        matches!(self, SearchMode::NameAndContent)
    }
}

/// One session's prebuilt, searchable haystacks, keyed by its stable id.
///
/// Both haystack strings are built once (at [`SearchIndex::build`]/refresh) and
/// never touched during per-keystroke scoring — that is what keeps re-filter
/// incremental. `fingerprint` is a cheap change key over the searchable fields
/// so a refresh can reuse an unchanged entry instead of rebuilding it.
struct Entry {
    /// Stable session id; the key used to reuse entries across a refresh.
    session_id: String,
    /// Cheap hash of the searchable fields (`label` + `content_index`); an
    /// entry is rebuilt on refresh only when this changes.
    fingerprint: u64,
    /// Haystack for [`SearchMode::NameOnly`] — the display label.
    name: String,
    /// Haystack for [`SearchMode::NameAndContent`] — label + `content_index`,
    /// so a content-mode match still ranks a name hit (the name is included).
    content: String,
}

impl Entry {
    /// Build both haystacks for `session`, tagging it with `fingerprint`.
    fn build(session: &Session, fingerprint: u64) -> Self {
        let mut content =
            String::with_capacity(session.label.len() + 1 + session.content_index.len());
        content.push_str(&session.label);
        if !session.content_index.is_empty() {
            content.push('\n');
            content.push_str(&session.content_index);
        }
        Entry {
            session_id: session.session_id.clone(),
            fingerprint,
            name: session.label.clone(),
            content,
        }
    }

    /// The haystack this entry exposes for `mode`.
    fn haystack(&self, mode: SearchMode) -> &str {
        match mode {
            SearchMode::NameOnly => &self.name,
            SearchMode::NameAndContent => &self.content,
        }
    }
}

/// A live, updatable substring index over the current session list.
///
/// This is the structure the TUI (Phase 5) holds across the app lifetime:
///
/// * type-to-search calls [`set_query`](Self::set_query) once per keystroke —
///   only the [`Pattern`] is rebuilt (as substring atoms), the haystacks are
///   untouched;
/// * a mode toggle calls [`set_mode`](Self::set_mode);
/// * a `SessionsChanged` reload calls [`refresh`](Self::refresh), which rebuilds
///   only the entries whose session changed (keyed by `session_id`) and keeps
///   the active query and mode;
/// * [`results`](Self::results) returns the ranked indices into the *current*
///   entry order (which mirrors the `sessions` slice last passed in).
///
/// A one-shot [`filter`] free function is provided too for callers that just
/// want a single ranked pass without holding an index.
pub struct SearchIndex {
    /// Per-session haystacks, in the same order as the last `sessions` slice.
    entries: Vec<Entry>,
    /// The active query text (preserved across refreshes).
    query: String,
    /// The active mode (preserved across refreshes).
    mode: SearchMode,
    /// Reused scratch matcher — carries nucleo's internal allocations.
    matcher: Matcher,
    /// The active query compiled to [`AtomKind::Substring`] atoms — rebuilt per
    /// keystroke by [`set_query`](Self::set_query) via [`Pattern::new`]. Shared
    /// by both the filter and the highlight seam so they score with the identical
    /// atom kind.
    pattern: Pattern,
    /// How many entries the last build/refresh actually (re)built. Instrumented
    /// so the partial-rebuild behaviour is observable (and unit-testable).
    last_rebuilt: usize,
}

impl Default for SearchIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchIndex {
    /// An empty index with an empty query and the default mode.
    #[must_use]
    pub fn new() -> Self {
        SearchIndex {
            entries: Vec::new(),
            query: String::new(),
            mode: SearchMode::default(),
            matcher: Matcher::new(Config::DEFAULT),
            // `Pattern::default()` has no atoms => treated as "empty query".
            pattern: Pattern::default(),
            last_rebuilt: 0,
        }
    }

    /// Build a fresh index over `sessions` (every entry is built).
    #[must_use]
    pub fn build(sessions: &[Session]) -> Self {
        let mut index = Self::new();
        index.refresh(sessions);
        index
    }

    /// Re-key the index against a new `sessions` slice, rebuilding haystacks
    /// ONLY for sessions that are new or whose searchable content changed.
    ///
    /// Unchanged sessions (same `session_id`, same fingerprint) keep their
    /// already-built haystack strings — no reallocation. The active query and
    /// mode are preserved, and entries are re-ordered to match `sessions` so
    /// the indices returned by [`results`](Self::results) address that slice.
    ///
    /// This is the `SessionsChanged` path: rebuild only what changed, keep the
    /// query, re-rank.
    pub fn refresh(&mut self, sessions: &[Session]) {
        // Move the previous entries into a lookup by stable id so we can reuse
        // the ones that did not change.
        let mut previous: HashMap<String, Entry> = self
            .entries
            .drain(..)
            .map(|e| (e.session_id.clone(), e))
            .collect();

        let mut rebuilt = 0;
        let mut entries = Vec::with_capacity(sessions.len());
        for session in sessions {
            let fingerprint = fingerprint(session);
            let entry = match previous.remove(&session.session_id) {
                // Same id AND same content => reuse the prebuilt haystacks.
                Some(prev) if prev.fingerprint == fingerprint => prev,
                // New session, or its searchable content changed => rebuild.
                _ => {
                    rebuilt += 1;
                    Entry::build(session, fingerprint)
                }
            };
            entries.push(entry);
        }

        self.entries = entries;
        self.last_rebuilt = rebuilt;
        // `self.query`, `self.mode`, and `self.pattern` are intentionally left
        // untouched: the query survives the refresh.
    }

    /// Set the active query, rebuilding the (small) substring [`Pattern`].
    ///
    /// Called once per keystroke. Only the [`Pattern`] is rebuilt — the prebuilt
    /// haystacks are never touched here.
    ///
    /// The pattern is built with [`AtomKind::Substring`] via [`Pattern::new`]
    /// rather than the fuzzy default, so a query matches ONLY where it appears as
    /// a contiguous run of characters (literal, no scattered subsequence). Case
    /// stays smart-case ([`CaseMatching::Smart`]). `Pattern::new` fixes the atom
    /// kind programmatically and does NOT interpret the `'`/`^`/`$`/`!` atom
    /// syntax, so substring matching never depends on what the user types; it
    /// only splits on whitespace (so `foo bar` requires both `foo` and `bar` as
    /// substrings, matching the previous whitespace semantics). Both the filter
    /// ([`results`](Self::results)) and the highlight seam
    /// ([`match_indices`](Self::match_indices)) score against this one
    /// `self.pattern`, so they always agree on the atom kind.
    pub fn set_query(&mut self, query: &str) {
        self.query.clear();
        self.query.push_str(query);
        // NOTE (upstream nucleo-matcher 0.3.1 quirk): the non-ASCII substring
        // path (`exact.rs::substring_match_non_ascii`) scans candidate starts
        // only up to `haystack.len() - needle.len()` EXCLUSIVE (the ASCII memmem
        // path correctly uses `+ 1`). So a substring match that ends at the very
        // LAST char of a haystack containing any non-ASCII char is missed. It is
        // narrow (ASCII haystacks are unaffected) and, because the filter and the
        // highlight both score this one pattern, it applies to them IDENTICALLY —
        // they never disagree. Living with it keeps nucleo pinned + isolated.
        self.pattern = Pattern::new(
            query,
            CaseMatching::Smart,
            Normalization::Smart,
            AtomKind::Substring,
        );
    }

    /// The active query text.
    // The `App` owns its own query string and renders that; this getter is the
    // index-side accessor asserted on by the refresh tests.
    #[allow(dead_code)]
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Set the active search mode (name-only vs. name+content).
    // The TUI flips modes via [`toggle_mode`](Self::toggle_mode); this absolute
    // setter backs the one-shot [`filter`] helper and the mode unit tests.
    #[allow(dead_code)]
    pub fn set_mode(&mut self, mode: SearchMode) {
        self.mode = mode;
    }

    /// Flip the search mode and return the new value.
    pub fn toggle_mode(&mut self) -> SearchMode {
        self.mode = self.mode.toggled();
        self.mode
    }

    /// The active search mode.
    #[must_use]
    pub fn mode(&self) -> SearchMode {
        self.mode
    }

    /// The number of indexed sessions.
    // Collection-shape accessors + rebuild instrumentation; consumed by the
    // partial-rebuild / mode unit tests, not on the bin runtime path.
    #[allow(dead_code)]
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the index holds no sessions.
    #[allow(dead_code)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many entries the last [`build`](Self::build)/[`refresh`](Self::refresh)
    /// actually (re)built. On a steady refresh where nothing changed this is 0.
    // Observability hook that lets the tests lock down "rebuild only what
    // changed"; the running TUI never needs to read it.
    #[allow(dead_code)]
    #[must_use]
    pub fn last_rebuilt(&self) -> usize {
        self.last_rebuilt
    }

    /// Rank the current sessions against the active query and mode.
    ///
    /// Returns entry indices (into the current order, i.e. the last `sessions`
    /// slice) sorted best-match-first. An empty query returns every session in
    /// the stable input order.
    pub fn results(&mut self) -> Vec<usize> {
        // No atoms => empty/whitespace query: return all in stable order.
        if self.pattern.atoms.is_empty() {
            return (0..self.entries.len()).collect();
        }

        // Borrow the three fields disjointly so the scoring closure can hold an
        // immutable pattern/entries borrow and a mutable matcher borrow at once.
        let mode = self.mode;
        let pattern = &self.pattern;
        let matcher = &mut self.matcher;
        let entries = &self.entries;

        let mut buf: Vec<char> = Vec::new();
        let mut scored: Vec<(usize, u32)> = entries
            .iter()
            .enumerate()
            .filter_map(|(i, entry)| {
                let haystack = Utf32Str::new(entry.haystack(mode), &mut buf);
                pattern.score(haystack, matcher).map(|score| (i, score))
            })
            .collect();

        // Score descending; ties broken by original index so ordering is stable
        // and deterministic (keeps the repo->branch->timestamp input order).
        scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        scored.into_iter().map(|(i, _)| i).collect()
    }

    /// The CHAR indices within `display` that the active query matches.
    ///
    /// This is the highlight seam: it scores the query against the given
    /// DISPLAY string (a row's visible label) SPECIFICALLY — deliberately
    /// decoupled from the filtering haystack (which, in name+content mode, also
    /// spans `content_index`) — so a highlight only ever marks what is actually
    /// visible in the row. A content-only match therefore returns an empty set
    /// (the term is absent from the visible label), and the row shows no
    /// highlight, which is the intended behaviour.
    ///
    /// Reuses the same live [`Pattern`]/[`Matcher`] the filter already holds (no
    /// fresh allocation per keystroke). The returned positions are CHAR indices
    /// into `display` (nucleo yields `u32` positions into the `Utf32Str`, i.e.
    /// char positions), sorted ascending and deduplicated. An empty/whitespace
    /// query parses to zero atoms and yields an empty set — nothing to
    /// highlight.
    pub fn match_indices(&mut self, display: &str) -> Vec<u32> {
        // No atoms => empty/whitespace query: nothing is highlighted.
        if self.pattern.atoms.is_empty() {
            return Vec::new();
        }

        let mut buf: Vec<char> = Vec::new();
        let haystack = Utf32Str::new(display, &mut buf);
        let mut indices: Vec<u32> = Vec::new();

        // `Pattern::indices` appends each atom's char positions into `indices`
        // (positions into the Utf32Str, i.e. char indices) WITHOUT clearing or
        // sorting, and returns `None` when the pattern does not match `display`
        // — e.g. a content-only hit whose term never appears in the visible
        // label. Treat a non-match as "no highlight".
        if self
            .pattern
            .indices(haystack, &mut self.matcher, &mut indices)
            .is_none()
        {
            return Vec::new();
        }

        // Per nucleo's own guidance for highlighting: unique + sorted positions.
        indices.sort_unstable();
        indices.dedup();
        indices
    }
}

/// Compute a cheap change key over a session's *searchable* fields.
///
/// Only the fields the haystacks are built from (`label`, `content_index`)
/// participate, so a refresh reuses an entry unless its searchable text moved.
fn fingerprint(session: &Session) -> u64 {
    let mut hasher = DefaultHasher::new();
    session.label.hash(&mut hasher);
    session.content_index.hash(&mut hasher);
    hasher.finish()
}

/// One-shot substring filter: rank `sessions` against `query` in `mode`.
///
/// Returns session indices INTO `sessions`, sorted best-match-first. An empty
/// (or whitespace-only) query returns every index in the stable input order.
/// `mode` is the single toggle between name-only and name+content matching.
///
/// This builds a throwaway [`SearchIndex`]; long-lived callers (the TUI) should
/// hold a [`SearchIndex`] and drive it incrementally instead.
///
/// The TUI holds a live index rather than calling this, so it is unused on the
/// bin runtime path; it is the primary driver of the ranking/mode unit tests and
/// a documented one-shot convenience, hence retained + `dead_code` allowed.
#[allow(dead_code)]
#[must_use]
pub fn filter(query: &str, sessions: &[Session], mode: SearchMode) -> Vec<usize> {
    let mut index = SearchIndex::build(sessions);
    index.set_mode(mode);
    index.set_query(query);
    index.results()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Build a synthetic session with a given id, label, and content text.
    fn session(id: &str, label: &str, content: &str) -> Session {
        Session {
            file: PathBuf::from(format!("/tmp/{id}.jsonl")),
            session_id: id.to_string(),
            cwd: PathBuf::from("/tmp/project"),
            git_branch: None,
            timestamp: None,
            repo: "project".to_string(),
            label: label.to_string(),
            content_index: content.to_string(),
        }
    }

    /// Name-only mode must NOT match a term that lives only in the content, but
    /// name+content mode (same interface, different mode) must.
    #[test]
    fn name_only_excludes_content_only_term_that_content_mode_includes() {
        // "webhook" appears only in session 0's CONTENT, and in session 1's NAME.
        let sessions = [
            session("s0", "alpha login flow", "resolved the webhook retries"),
            session("s1", "webhook debugging notes", "unrelated body text"),
        ];

        // Name-only: session 0 is excluded (term is content-only); only s1 hits.
        let name_only = filter("webhook", &sessions, SearchMode::NameOnly);
        assert_eq!(
            name_only,
            vec![1],
            "name-only must match only the name hit (s1), never the content-only s0"
        );
        assert!(
            !name_only.contains(&0),
            "content-only term must not surface in name-only mode"
        );

        // Name+content: the SAME term now also pulls in s0 via its content.
        let name_and_content = filter("webhook", &sessions, SearchMode::NameAndContent);
        assert!(
            name_and_content.contains(&0),
            "name+content mode must include the content-only match (s0)"
        );
        assert!(
            name_and_content.contains(&1),
            "name+content mode must still include the name match (s1)"
        );
    }

    /// A closer match must rank above a looser one.
    #[test]
    fn ranking_orders_closer_match_above_looser() {
        // Both contain "cat" as a CONTIGUOUS substring: s0 mid-word inside
        // "con[cat]enate", s1 as the exact short word. The exact/word-boundary
        // hit must rank first.
        let sessions = [
            session("far", "concatenate helper", ""),
            session("near", "cat", ""),
        ];

        let ranked = filter("cat", &sessions, SearchMode::NameOnly);
        assert!(
            ranked.contains(&0) && ranked.contains(&1),
            "both should match"
        );

        let pos_far = ranked.iter().position(|&i| i == 0).unwrap();
        let pos_near = ranked.iter().position(|&i| i == 1).unwrap();
        assert!(
            pos_near < pos_far,
            "the exact 'cat' must rank above the mid-word 'concatenate': {ranked:?}"
        );
    }

    /// Substring matching (not fuzzy): a scattered subsequence that the OLD
    /// fuzzy matcher WOULD have accepted no longer matches, while a genuine
    /// contiguous substring of the same haystack still does — in BOTH modes.
    #[test]
    fn search_matches_contiguous_substring_not_scattered_subsequence() {
        let named = [session("s0", "abc", "")];

        // "ac" is a subsequence of "abc" (a at 0, c at 2, 'b' skipped): the old
        // fuzzy matcher accepted it. Substring matching rejects it — not a
        // contiguous run. This is the negative that genuinely guards the change.
        assert!(
            filter("ac", &named, SearchMode::NameOnly).is_empty(),
            "a scattered subsequence must NOT match under substring search"
        );
        // Genuine contiguous substrings of the same name still match.
        assert_eq!(
            filter("ab", &named, SearchMode::NameOnly),
            vec![0],
            "a leading contiguous substring matches"
        );
        assert_eq!(
            filter("bc", &named, SearchMode::NameOnly),
            vec![0],
            "a trailing contiguous substring matches"
        );
        assert_eq!(
            filter("abc", &named, SearchMode::NameOnly),
            vec![0],
            "the whole name matches"
        );
        // The same substring rule governs the name side of name+content mode.
        assert!(
            filter("ac", &named, SearchMode::NameAndContent).is_empty(),
            "name+content mode is substring too: no scattered name match"
        );

        // Content side: a contiguous run in the transcript matches in
        // name+content mode; a scattered subsequence of it does not.
        let bodied = [session("c0", "notes", "deploy pipeline")];
        assert_eq!(
            filter("deploy", &bodied, SearchMode::NameAndContent),
            vec![0],
            "a contiguous content substring matches in name+content mode"
        );
        // "dpp" is a subsequence of "deploy pipeline" (d at 0, p at 2, p at 7)
        // that the old fuzzy matcher accepted; substring matching rejects it.
        assert!(
            filter("dpp", &bodied, SearchMode::NameAndContent).is_empty(),
            "a scattered content subsequence must not match under substring search"
        );
    }

    /// An empty query returns every session in the stable input order.
    #[test]
    fn empty_query_returns_all_in_stable_order() {
        let sessions = [
            session("a", "first", "x"),
            session("b", "second", "y"),
            session("c", "third", "z"),
        ];

        assert_eq!(
            filter("", &sessions, SearchMode::NameOnly),
            vec![0, 1, 2],
            "empty query returns all in input order"
        );
        // Whitespace-only parses to zero atoms and behaves like an empty query.
        assert_eq!(
            filter("   ", &sessions, SearchMode::NameAndContent),
            vec![0, 1, 2],
            "whitespace-only query returns all in input order"
        );
    }

    /// A refresh rebuilds ONLY the changed session (keyed by id), preserves the
    /// active query/mode, keeps unchanged entries, and reflects the change.
    #[test]
    fn refresh_rebuilds_only_changed_session_and_preserves_query() {
        // v1: no session mentions "payment" anywhere.
        let v1 = [
            session("s1", "alpha", "old body text"),
            session("s2", "beta", "steady body text"),
        ];

        let mut index = SearchIndex::build(&v1);
        assert_eq!(index.last_rebuilt(), 2, "initial build builds every entry");

        index.set_mode(SearchMode::NameAndContent);
        index.set_query("payment");
        assert!(
            index.results().is_empty(),
            "no v1 session matches 'payment' yet"
        );

        // v2: s2 is byte-identical (must be reused); s1's content changed to add
        // the term; the list is also REORDERED to prove indices track the slice.
        let v2 = [
            session("s2", "beta", "steady body text"),
            session("s1", "alpha", "added a payment webhook handler"),
        ];
        index.refresh(&v2);

        // Only the changed session (s1) was rebuilt; s2 was reused by id.
        assert_eq!(
            index.last_rebuilt(),
            1,
            "refresh must rebuild only the changed session, reusing the unchanged one"
        );
        assert_eq!(index.len(), 2, "both sessions remain indexed after refresh");

        // The active query survived the refresh untouched.
        assert_eq!(index.query(), "payment", "refresh must preserve the query");
        assert_eq!(
            index.mode(),
            SearchMode::NameAndContent,
            "refresh must preserve the mode"
        );

        // The change is reflected, and the returned index addresses the NEW
        // slice order: s1 now sits at position 1.
        assert_eq!(
            index.results(),
            vec![1],
            "the changed session must now match, at its new slice index"
        );
    }

    /// A no-op refresh (same sessions, same content) rebuilds nothing.
    #[test]
    fn steady_refresh_rebuilds_nothing() {
        let sessions = [
            session("s1", "alpha", "body"),
            session("s2", "beta", "body"),
        ];
        let mut index = SearchIndex::build(&sessions);
        index.refresh(&sessions);
        assert_eq!(
            index.last_rebuilt(),
            0,
            "an unchanged refresh must reuse every entry (rebuild none)"
        );
    }

    /// The mode toggle flips between the two modes and drives inclusion.
    #[test]
    fn mode_toggle_flips_content_inclusion() {
        let mut index = SearchIndex::new();
        assert_eq!(index.mode(), SearchMode::NameOnly, "default is name-only");
        assert!(!index.mode().includes_content());

        assert_eq!(index.toggle_mode(), SearchMode::NameAndContent);
        assert!(index.mode().includes_content());

        assert_eq!(index.toggle_mode(), SearchMode::NameOnly);
        assert!(!index.mode().includes_content());
        assert!(index.is_empty(), "a fresh index holds no sessions");
    }

    /// The highlight seam marks a contiguous substring hit exactly, and marks
    /// NOTHING for a scattered (non-contiguous) subsequence — the seam agrees
    /// with the substring filter.
    #[test]
    fn match_indices_returns_matched_char_positions() {
        let mut index = SearchIndex::new();

        // Contiguous substring: "bc" over "abc" marks positions 1 and 2.
        index.set_query("bc");
        assert_eq!(
            index.match_indices("abc"),
            vec![1, 2],
            "a contiguous hit marks exactly its chars"
        );

        // "ac" is a fuzzy subsequence of "abc" (a at 0, c at 2, 'b' skipped) that
        // the OLD fuzzy matcher would have marked [0, 2]. Under substring matching
        // it is not a contiguous run, so the seam highlights nothing.
        index.set_query("ac");
        assert!(
            index.match_indices("abc").is_empty(),
            "a scattered subsequence must not highlight under substring matching"
        );
    }

    /// A non-matching query and an empty/whitespace query both highlight
    /// nothing.
    #[test]
    fn match_indices_is_empty_for_non_matching_and_empty_queries() {
        let mut index = SearchIndex::new();

        index.set_query("zzz");
        assert!(
            index.match_indices("abc").is_empty(),
            "a query that does not match the label highlights nothing"
        );

        index.set_query("");
        assert!(
            index.match_indices("abc").is_empty(),
            "an empty query highlights nothing"
        );

        index.set_query("   ");
        assert!(
            index.match_indices("abc").is_empty(),
            "a whitespace-only query parses to zero atoms and highlights nothing"
        );
    }

    /// A multi-byte / unicode label returns CHAR positions (not byte offsets)
    /// and never panics. The 4-byte leading emoji is the tell: a naive
    /// byte-offset approach would report `[5..=10]`, the correct char positions
    /// are `[2..=7]`.
    #[test]
    fn match_indices_uses_char_positions_not_byte_offsets() {
        let mut index = SearchIndex::new();
        index.set_query("deploy");

        // "🚀 deploy now": 🚀(0) ' '(1) d(2) e(3) p(4) l(5) o(6) y(7) ... The
        // emoji is one char but four bytes, so char and byte positions diverge.
        // (Trailing " now" keeps the match off the final char, sidestepping the
        // upstream nucleo non-ASCII substring tail off-by-one noted in
        // `set_query`; the point here is CHAR- vs byte-index alignment.)
        assert_eq!(
            index.match_indices("🚀 deploy now"),
            vec![2, 3, 4, 5, 6, 7],
            "positions must be CHAR indices, aligned past a multi-byte char"
        );
    }

    /// The highlight run equals exactly the matched substring position (one
    /// contiguous block), and stays on CHAR indices even when a multi-byte char
    /// sits INSIDE the run.
    #[test]
    fn match_indices_marks_the_contiguous_substring_run_unicode_safe() {
        let mut index = SearchIndex::new();

        // The run is exactly the substring span, nothing scattered: "deploy" in
        // "please deploy now" occupies chars 7..=12.
        index.set_query("deploy");
        assert_eq!(
            index.match_indices("please deploy now"),
            vec![7, 8, 9, 10, 11, 12],
            "the highlight is exactly the contiguous substring span"
        );

        // Multi-byte char INSIDE the run: "a🚀bc" is a(0) 🚀(1) b(2) c(3); the
        // rocket is one CHAR but four bytes. Matching "🚀b" marks char run [1, 2]
        // — a naive byte-offset approach would report [1, 5]. (The trailing "c"
        // keeps the run off the final char, sidestepping the upstream nucleo
        // non-ASCII substring tail off-by-one noted in `set_query`.)
        index.set_query("🚀b");
        assert_eq!(
            index.match_indices("a🚀bc"),
            vec![1, 2],
            "the run spans a multi-byte char and stays on char indices"
        );
    }

    /// Incremental re-filter: narrowing the query per keystroke re-ranks over
    /// the SAME prebuilt haystacks (entries are never rebuilt on set_query).
    #[test]
    fn incremental_requery_does_not_rebuild_haystacks() {
        let sessions = [
            session("s0", "deploy pipeline", ""),
            session("s1", "debug parser", ""),
        ];
        let mut index = SearchIndex::build(&sessions);
        let built = index.last_rebuilt();

        index.set_query("de");
        let wide = index.results();
        assert!(wide.contains(&0) && wide.contains(&1), "'de' matches both");

        index.set_query("deploy");
        let narrow = index.results();
        assert_eq!(
            narrow,
            vec![0],
            "'deploy' narrows to the deploy session only"
        );

        // Re-querying never triggered a haystack rebuild.
        assert_eq!(
            index.last_rebuilt(),
            built,
            "set_query must not rebuild the haystack"
        );
    }
}
