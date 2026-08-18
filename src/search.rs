//! Substring search over sessions: a SIMD membership filter that also marks the
//! preview, and a nucleo highlight for the row label.
//!
//! Isolates every `nucleo` call so an API change touches one module (Risks
//! table). Builds the searchable haystacks for each session (name/label AND the
//! capped `content_index`) and exposes a [`filter`] that returns matching
//! indices. Supports two modes behind one interface: name/label-only (default,
//! instant) and name+content; matching stays incremental (re-filter on each
//! keystroke without rebuilding the haystack), and the haystack rebuilds only
//! for changed sessions on `SessionsChanged`.
//!
//! # The filter answers membership, not rank
//!
//! Per keystroke the filter asks ONE question per entry — does every query atom
//! occur as a substring? — and answers it with `memchr::memmem` (SIMD, no
//! allocation, no UTF-8 → UTF-32 conversion). **nucleo is not on this path.**
//!
//! It used to be. The filter scored survivors with [`Pattern::score`] and sorted
//! by that score — a rank that provably could never reach the screen, because
//! `App::order_filtered` re-sorts every result by `(Reverse(timestamp),
//! session_id)`, a total order with ZERO ties (measured: 66 entries → 66 distinct
//! keys). With unique keys even sort stability is irrelevant: the score was
//! computed and then thrown away. It was not cheap. `Utf32Str::new` is zero-copy
//! only for pure-ASCII haystacks; for any haystack holding a non-ASCII byte it
//! allocates and fills a whole `Vec<char>` at ~8.6 ns/byte, and **86% of real
//! entries are non-ASCII** once `serde_json` decodes `\uXXXX` escapes (the raw
//! files are pure ASCII — the decoded `content_index` is what nucleo saw). So
//! 76–81% of every keystroke rebuilt megabytes of UTF-32 to order a list that was
//! about to be re-ordered by timestamp.
//!
//! Only the `Some`/`None` membership bit was ever load-bearing, and memmem
//! answers exactly that. Measured against the real corpus, dropping the rank
//! stage took the worst keystroke (`n`, 65/66 entries matching) from **13.9 ms to
//! 417 ns**, and typing `n`→`np`→`npx` from **25.8 ms to ~43 µs**. The win grows
//! with the corpus: cost is now purely linear in haystack bytes (~27 µs per
//! 1.2 MB) with no per-entry conversion, so the worst case sits below the old
//! best case.
//!
//! # Smart case is decided PER ATOM
//!
//! nucleo's [`CaseMatching::Smart`] makes each atom independently case-sensitive
//! iff **that atom** carries an uppercase char — `foo BAR` matches `foo`
//! case-insensitively and `BAR` case-sensitively, within one query. The decision
//! therefore rides each [`AtomFinder`], never the query as a whole, and each atom
//! picks its own haystack accordingly. A per-QUERY branch would look equivalent
//! and silently break mixed queries.
//!
//! This branch is **mandatory, not an optimization**. Answering an
//! uppercase-bearing query from the lowercased haystack would include rows nucleo
//! excludes: measured, `NPX` finds 6 entries there while nucleo matches 0. That
//! is an inclusion regression, not a ranking nuance.
//!
//! # Why BOTH haystacks are kept
//!
//! Each entry carries a CASED haystack and a LOWERCASED sibling, and both are
//! live: the case-SENSITIVE atom branch searches the cased one, the
//! case-INSENSITIVE branch the lowercased one. It is tempting to conclude that
//! removing nucleo's scoring left the cased haystack dead — it did not. Deleting
//! it would leave every uppercase-bearing query (`TODO`, `API`, `React`)
//! searching a lowercased haystack case-sensitively, matching nothing.
//!
//! Lowercasing happens ONCE per entry at build/refresh, never per keystroke. The
//! cost is ~2× the extracted content in memory — bounded by `store::parse`'s
//! `CONTENT_INDEX_CAP` and cheap at this scale (tens of MB store-wide).
//!
//! # Scope-limited matching
//!
//! The candidate set is narrowed before any of this runs.
//! [`results`](SearchIndex::results) matches EVERY entry, while
//! [`results_within`](SearchIndex::results_within) runs the SAME core over only a
//! caller-supplied subset of entry indices. The TUI drives the per-keystroke
//! re-filter through `results_within` with its in-scope set, so an out-of-scope
//! session is never scanned only to be discarded after the fact. Both entry
//! points share one private core, and any out-of-range candidate index is skipped
//! fail-soft.
//!
//! # Where this filter and nucleo disagree
//!
//! The filter is no longer nucleo's match set *by construction*, so the (narrow)
//! divergences are stated rather than assumed. Each is accepted deliberately:
//!
//! * **Unicode normalization.** nucleo's [`Normalization::Smart`] accent-folds
//!   (`resume` matching `résumé`); a byte search does not, so such a match is
//!   excluded. **Unchanged** — the old gate rejected it identically, before
//!   nucleo was ever consulted.
//! * **Per-string vs per-char lowercasing.** We lowercase via
//!   [`str::to_lowercase`], which is context-sensitive per string (Greek final
//!   sigma `Σ`→`ς`, `İ`→`i̇`), whereas nucleo folds per char. For some non-ASCII
//!   text this is marginally more restrictive than nucleo. **Unchanged** — same
//!   reason.
//! * **The non-ASCII tail off-by-one.** Here the filter is the more CORRECT of
//!   the two, and diverges from the highlight; see
//!   [`set_query`](SearchIndex::set_query).
//! * **The smart-case predicate on a non-ASCII atom.** nucleo's `ignore_case`
//!   field is private, so the decision is re-derived rather than read. For an
//!   ASCII atom the re-derivation is EXACT (`char::is_uppercase` over ASCII is
//!   precisely nucleo's `is_ascii_uppercase`), which covers the realistic input
//!   space. For a non-ASCII atom nucleo consults a case-folding table rather than
//!   the Unicode `Uppercase` property, so the two can differ on exotica such as
//!   titlecase digraphs (`ǅ`), where nucleo would match case-sensitively and we
//!   match case-insensitively.
//!
//! # Which nucleo API, and what is left of it
//!
//! `nucleo = 0.5.0` re-exports the finished low-level `nucleo-matcher` types
//! (`Pattern`, `Atom`/`AtomKind`, `Matcher`, `Config`, `Utf32Str`,
//! `CaseMatching`, `Normalization`). We deliberately use that synchronous
//! low-level path rather than the high-level threaded `Nucleo`/`Injector`
//! worker: a synchronous matcher is deterministic (no background
//! `tick()`/snapshot races) and trivial to unit-test.
//!
//! nucleo now backs exactly ONE thing: the ROW-LABEL highlight
//! ([`match_indices`](SearchIndex::match_indices)), which marks the matched chars
//! of a single visible row label — bounded, tiny work. The filter no longer calls
//! it, and neither do the preview's marks.
//!
//! # Two marking seams, because there are two shapes of surface
//!
//! [`match_indices`](SearchIndex::match_indices) asks nucleo about ONE string and
//! answers only when EVERY atom occurs in it (`Pattern::indices` propagates an
//! atom's miss to the whole pattern). That is exactly right for a row LABEL: one
//! string, and the row is on the board because that string matched.
//!
//! A previewed TRANSCRIPT is many strings, and the filter admitted the session
//! because the atoms occur ANYWHERE in it. So the preview marks through
//! [`atom_match_positions`](SearchIndex::atom_match_positions), which runs the
//! filter's OWN [`AtomFinder`]s over one rendered line and returns the char
//! positions any atom covers — PER ATOM, union across atoms. Asking the
//! whole-pattern seam per line would mark nothing the moment a two-word query's
//! words sat on different lines, and an unmarked pane cannot explain why the row is
//! there. Both seams take their atoms from the one splitter ([`gate_atoms`]), so
//! "which words" is never re-decided.
//!
//! Matching is **substring, not fuzzy**: each keystroke rebuilds the small
//! [`Pattern`] via [`Pattern::new`] with a fixed [`AtomKind::Substring`], so a
//! query marks only where it appears as a contiguous run (smart-case) — the atom
//! kind is forced in code and never depends on user-typed atom syntax. The filter
//! holds that rule independently, in [`atoms_match`]: memmem is substring search
//! by nature, so the two agree on the atom kind without sharing a `Pattern`.
//! Incrementality comes from the prebuilt haystack strings, untouched across
//! keystrokes (only the tiny pattern and the atom finders are rebuilt); a
//! `SessionsChanged` refresh rebuilds only the entries whose session actually
//! changed.
//!
//! Everything nucleo-shaped is contained below, and so is every `memchr` call.
//! The rest of the crate sees only [`SearchIndex`], [`SearchMode`], and
//! [`filter`].
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
use std::ops::Range;

use memchr::memmem;
use nucleo::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher, Utf32Str};

use crate::store::Session;

/// Which haystack the filter searches.
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

    /// Whether this mode searches `content_index` as well as the name.
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
/// Every haystack string is built once (at [`SearchIndex::build`]/refresh) and
/// never touched during per-keystroke scoring — that is what keeps re-filter
/// incremental. `fingerprint` is a cheap change key over the searchable fields
/// so a refresh can reuse an unchanged entry instead of rebuilding it.
///
/// Each mode carries TWO haystacks, and BOTH are live: the CASED one (`name` /
/// `content`) backs the case-SENSITIVE atom branch, and a LOWERCASED sibling
/// (`name_lower` / `content_lower`) backs the case-INSENSITIVE branch — see the
/// module docs. Neither is redundant: dropping the cased pair would break every
/// uppercase-bearing query, and dropping the lowercased pair would force a
/// re-casing allocation per entry per keystroke. The cost is ~2× the extracted
/// content in memory, bounded by `store::parse`'s `CONTENT_INDEX_CAP`.
struct Entry {
    /// Stable session id; the key used to reuse entries across a refresh.
    session_id: String,
    /// Cheap hash of the searchable fields (`label` + `content_index`); an
    /// entry is rebuilt on refresh only when this changes.
    fingerprint: u64,
    /// CASED haystack for [`SearchMode::NameOnly`] — the display label. Searched
    /// by the case-sensitive atom branch.
    name: String,
    /// CASED haystack for [`SearchMode::NameAndContent`] — label + `content_index`,
    /// so a content-mode query still matches a name hit (the name is included).
    /// Searched by the case-sensitive atom branch.
    content: String,
    /// LOWERCASED `name`, searched by the case-insensitive [`SearchMode::NameOnly`]
    /// atom branch.
    name_lower: String,
    /// LOWERCASED `content`, searched by the case-insensitive
    /// [`SearchMode::NameAndContent`] atom branch.
    content_lower: String,
}

impl Entry {
    /// Build every haystack for `session`, tagging it with `fingerprint`.
    fn build(session: &Session, fingerprint: u64) -> Self {
        let mut content =
            String::with_capacity(session.label.len() + 1 + session.content_index.len());
        content.push_str(&session.label);
        if !session.content_index.is_empty() {
            content.push('\n');
            content.push_str(&session.content_index);
        }
        // Lowercase ONCE here (build/refresh), never per keystroke. `to_lowercase`
        // is unicode-aware, so a case-insensitive atom matches mixed-case text.
        // It folds per string, not per char, so it is not byte-for-byte nucleo's
        // own folding — see the module-level divergence list for that narrow
        // per-string vs. per-char exception.
        let name_lower = session.label.to_lowercase();
        let content_lower = content.to_lowercase();
        Entry {
            session_id: session.session_id.clone(),
            fingerprint,
            name: session.label.clone(),
            content,
            name_lower,
            content_lower,
        }
    }

    /// The CASED haystack for `mode` — searched by a case-SENSITIVE atom.
    fn haystack(&self, mode: SearchMode) -> &str {
        match mode {
            SearchMode::NameOnly => &self.name,
            SearchMode::NameAndContent => &self.content,
        }
    }

    /// The LOWERCASED haystack for `mode` — searched by a case-INSENSITIVE atom.
    fn gate_haystack(&self, mode: SearchMode) -> &str {
        match mode {
            SearchMode::NameOnly => &self.name_lower,
            SearchMode::NameAndContent => &self.content_lower,
        }
    }
}

/// One query atom compiled to a SIMD substring searcher, carrying the smart-case
/// decision nucleo would have made for **that atom**.
///
/// `case_sensitive` mirrors nucleo's [`CaseMatching::Smart`], which is decided
/// **PER ATOM**, never per query: an atom matches case-sensitively iff it carries
/// an uppercase char, so `foo BAR` searches case-insensitively for `foo` and
/// case-sensitively for `BAR` in the one query. The flag rides the atom precisely
/// so that stays true — a per-query decision would look equivalent and silently
/// break every mixed-case query.
///
/// The needle is baked to match: lowercased bytes for a case-insensitive atom
/// (searched against the entry's lowercased haystack), cased bytes for a
/// case-sensitive one (searched against the cased haystack). Both haystacks are
/// prebuilt, so a keystroke allocates nothing per entry.
struct AtomFinder {
    /// SIMD substring searcher over this atom's needle bytes. Owned
    /// (`into_owned`) so it can outlive the query string it was built from and
    /// live in the [`SearchIndex`] across keystrokes.
    finder: memmem::Finder<'static>,
    /// Whether this atom matches case-sensitively (it carries an uppercase char),
    /// and therefore which of the entry's two haystacks it searches.
    case_sensitive: bool,
}

impl AtomFinder {
    /// Compile one (already unescaped, non-empty) query `atom`.
    ///
    /// The uppercase test re-derives nucleo's decision because `Atom::ignore_case`
    /// is a private field — it cannot be read back. For an ASCII atom the
    /// re-derivation is EXACT: `char::is_uppercase` over ASCII is precisely
    /// nucleo's `is_ascii_uppercase` test. See the module docs for the narrow
    /// non-ASCII divergence.
    fn new(atom: &str) -> Self {
        let case_sensitive = atom.chars().any(char::is_uppercase);
        // Bake the needle into the case it will be searched in, so the scan
        // itself is a raw byte compare with nothing to fold per keystroke.
        let needle = if case_sensitive {
            atom.to_string()
        } else {
            atom.to_lowercase()
        };
        AtomFinder {
            finder: memmem::Finder::new(needle.as_bytes()).into_owned(),
            case_sensitive,
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
/// * [`results`](Self::results) returns the matching indices into the *current*
///   entry order (which mirrors the `sessions` slice last passed in).
///
/// A one-shot [`filter`] free function is provided too for callers that just
/// want a single pass without holding an index.
pub struct SearchIndex {
    /// Per-session haystacks, in the same order as the last `sessions` slice.
    entries: Vec<Entry>,
    /// The active query text (preserved across refreshes).
    query: String,
    /// The active mode (preserved across refreshes).
    mode: SearchMode,
    /// Reused scratch matcher — carries nucleo's internal allocations. Used ONLY
    /// by the row-LABEL highlight ([`match_indices`](Self::match_indices)).
    matcher: Matcher,
    /// The active query compiled to [`AtomKind::Substring`] atoms — rebuilt per
    /// keystroke by [`set_query`](Self::set_query) via [`Pattern::new`]. Backs the
    /// row-LABEL highlight ([`match_indices`](Self::match_indices)) ONLY; the filter
    /// answers membership from `atom_finders` instead.
    pattern: Pattern,
    /// The active query's atoms compiled to SIMD substring searchers, each
    /// carrying its own smart-case decision, rebuilt once per keystroke by
    /// [`set_query`](Self::set_query). This IS the filter: [`results`](Self::results)
    /// reuses these across every entry so no per-entry needle setup or allocation
    /// happens during a scan. The preview's marks
    /// ([`atom_match_positions`](Self::atom_match_positions)) reuse the SAME
    /// finders, which is what makes a mark and an admission the same decision.
    atom_finders: Vec<AtomFinder>,
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
            // `Pattern::default()` has no atoms => nothing to highlight.
            pattern: Pattern::default(),
            // No atoms => the empty query, and `results` returns all.
            atom_finders: Vec::new(),
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
    /// query, re-filter.
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

    /// Set the active query, rebuilding the (small) substring [`Pattern`] and the
    /// per-atom SIMD finders.
    ///
    /// Called once per keystroke. Only these two tiny structures are rebuilt — the
    /// prebuilt haystacks are never touched here.
    ///
    /// The pattern is built with [`AtomKind::Substring`] via [`Pattern::new`]
    /// rather than the fuzzy default, so the highlight marks ONLY a contiguous run
    /// of characters (literal, no scattered subsequence). Case stays smart-case
    /// ([`CaseMatching::Smart`]). `Pattern::new` fixes the atom kind
    /// programmatically and does NOT interpret the `'`/`^`/`$`/`!` atom syntax, so
    /// matching never depends on what the user types; it only splits on whitespace
    /// (so `foo bar` requires both `foo` and `bar` as substrings). The filter
    /// enforces the same substring rule independently — memmem searches substrings
    /// by nature — and [`gate_atoms`] mirrors nucleo's splitter so both demand the
    /// same atoms.
    pub fn set_query(&mut self, query: &str) {
        self.query.clear();
        self.query.push_str(query);
        // NOTE (upstream nucleo-matcher 0.3.1 quirk): the non-ASCII substring
        // path (`exact.rs::substring_match_non_ascii`) scans candidate starts
        // only up to `haystack.len() - needle.len()` EXCLUSIVE (the ASCII memmem
        // path correctly uses `+ 1`). So a substring match that ends at the very
        // LAST char of a haystack containing any non-ASCII char is missed.
        //
        // The filter and the highlight DIVERGE here, and that is accepted
        // deliberately. They used to score this one `Pattern`, so the quirk hit
        // both identically; now the memmem filter FINDS such a match while this
        // pattern still misses it, so the row can appear with no highlight. The
        // filter is the more CORRECT of the two — it errs toward MORE matches, and
        // an un-highlighted row is already an expected sight (a content-only match
        // shows none, by design). It also needs a non-ASCII haystack AND a match
        // ending at its very last char. Reproducing the upstream bug in the filter
        // to keep the two symmetrical would forfeit a real correctness gain to
        // protect a comment; the divergence is pinned by a test instead
        // (`non_ascii_tail_match_filters_in_though_nucleo_highlight_misses_it`).
        self.pattern = Pattern::new(
            query,
            CaseMatching::Smart,
            Normalization::Smart,
            AtomKind::Substring,
        );
        // Compile the filter's substring searchers ONCE per keystroke, reused
        // across every entry by the scan in `matching_candidates`. Split the query
        // into the same atoms nucleo requires and bake each one's smart-case
        // decision into its own finder.
        self.atom_finders = gate_atoms(query)
            .into_iter()
            .filter(|atom| !atom.is_empty())
            .map(|atom| AtomFinder::new(&atom))
            .collect();
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

    /// Match the current sessions against the active query and mode.
    ///
    /// Returns the entry indices (into the current order, i.e. the last
    /// `sessions` slice) that match, in the input order. An empty query returns
    /// every session.
    ///
    /// A thin wrapper over [`matching_candidates`](Self::matching_candidates)
    /// with EVERY entry as the candidate set;
    /// [`results_within`](Self::results_within) is the same core over a
    /// caller-supplied subset.
    // The running TUI now matches a scope-limited candidate set via
    // `results_within`, so this whole-index variant is no longer on the bin
    // runtime path; it is retained for the one-shot `filter` convenience and the
    // membership/mode/refresh unit tests that drive it directly.
    #[allow(dead_code)]
    pub fn results(&self) -> Vec<usize> {
        let all: Vec<usize> = (0..self.entries.len()).collect();
        self.matching_candidates(&all)
    }

    /// Match a SCOPE-LIMITED candidate set against the active query and mode.
    ///
    /// `candidates` are entry indices (into the current order, mirroring the
    /// `sessions`/`scoped` slices the TUI holds); only they are scanned, so an
    /// out-of-scope session is never searched just to be discarded afterward.
    /// Returns the matching candidates in the order given.
    ///
    /// Fail-soft: any candidate index outside the current entries is silently
    /// skipped (never a panic / out-of-bounds), consistent with the crate's
    /// fail-soft posture. An empty (or whitespace-only) query returns the
    /// in-range candidates unchanged, mirroring [`results`](Self::results)'s
    /// empty-query "return all" behaviour over its own candidate set.
    pub fn results_within(&self, candidates: &[usize]) -> Vec<usize> {
        self.matching_candidates(candidates)
    }

    /// Shared membership core behind [`results`](Self::results) (every entry) and
    /// [`results_within`](Self::results_within) (a scope-limited subset).
    ///
    /// Answers membership for exactly the given `candidates` (entry indices) and
    /// returns the matches **in the order given** — this deliberately imposes no
    /// order of its own. `App::order_filtered` owns display order, re-sorting
    /// every result by a tie-free total order, which is precisely why ranking them
    /// here would be wasted work (see the module docs). Out-of-range candidate
    /// indices are skipped fail-soft, and an empty query (no atoms)
    /// short-circuits to the in-range candidates.
    fn matching_candidates(&self, candidates: &[usize]) -> Vec<usize> {
        // No atoms => empty/whitespace query: return the in-range candidates in
        // the order given (mirrors results()'s stable-order "return all" case).
        if self.atom_finders.is_empty() {
            return candidates
                .iter()
                .copied()
                .filter(|&i| i < self.entries.len())
                .collect();
        }

        let mode = self.mode;
        let entries = &self.entries;
        let finders = &self.atom_finders;

        candidates
            .iter()
            .copied()
            .filter(|&i| {
                // Fail-soft: skip any candidate index out of range (never panic).
                entries.get(i).is_some_and(|entry| {
                    // A SIMD byte-substring scan per atom, over whichever haystack
                    // that atom's smart-case decision calls for. No UTF-32
                    // conversion, no allocation, no scoring: membership is the
                    // only bit anything downstream reads.
                    atoms_match(entry.haystack(mode), entry.gate_haystack(mode), finders)
                })
            })
            .collect()
    }

    /// The CHAR indices within `display` that the active query matches.
    ///
    /// This is the ROW-LABEL highlight seam, and the only remaining nucleo call:
    /// it scores the query against the given DISPLAY string (a row's visible
    /// label) SPECIFICALLY — deliberately decoupled from the filtering haystack
    /// (which, in name+content mode, also spans `content_index`) — so a highlight
    /// only ever marks what is actually visible in the row. A content-only match
    /// therefore returns an empty set (the term is absent from the visible label),
    /// and the row shows no highlight, which is the intended behaviour.
    ///
    /// WHOLE-pattern by nature: nucleo answers only when EVERY atom occurs in
    /// `display`, which is what a label wants and what a transcript LINE must not
    /// be asked — see [`atom_match_positions`](Self::atom_match_positions).
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

    /// The CHAR positions within `text` that the active query's atoms cover — the
    /// PREVIEW-MARK seam.
    ///
    /// Marks **PER ATOM**, through the very [`AtomFinder`]s the FILTER answers
    /// membership with: every occurrence of every atom, wherever it falls, and the
    /// UNION of those runs is what the caller marks. nucleo is not on this path.
    ///
    /// Why this is not [`match_indices`](Self::match_indices) — the whole-pattern
    /// seam, which would mark nothing whenever a two-word query's words sat on
    /// different lines — is argued once in the module docs ("Two marking seams").
    /// The short of it: marking per atom reproduces the FILTER's own admission
    /// rule, so a marked pane explains why the row is on the board.
    ///
    /// Smart case rides each atom exactly as it does in the filter — these ARE the
    /// filter's finders — so `foo BAR` marks `foo` case-insensitively and `BAR`
    /// case-sensitively inside one call, each against the haystack casing its own
    /// decision calls for.
    ///
    /// Returns ascending, deduplicated CHAR positions (never byte offsets). Runs
    /// that overlap or abut, whether from one atom or several, merge into one
    /// marked span rather than double-styling a char. Empty for an empty or
    /// whitespace-only query, and for text no atom occurs in.
    pub fn atom_match_positions(&self, text: &str) -> Vec<usize> {
        // No atoms => empty/whitespace query: nothing is marked. Deliberately NOT
        // the filter's "no atoms => every candidate": an unqueried pane marks
        // nothing, it does not mark everything.
        if self.atom_finders.is_empty() || text.is_empty() {
            return Vec::new();
        }
        // One flag per CHAR of `text`. Marking into it is what merges the union:
        // two atoms covering one char mark it once, and the output comes out sorted
        // and deduplicated by construction.
        let mut marked = vec![false; text.chars().count()];
        // Both case branches' byte -> CHAR maps, built ONCE per call rather than per
        // atom. Both are built unconditionally: `text` here is a single rendered
        // line, so branching to skip one buys nothing worth the extra state.
        let cased_starts = char_starts(text);
        let (lowered, lowered_starts) = lowered_with_char_starts(text);
        for atom in &self.atom_finders {
            let (haystack, starts) = if atom.case_sensitive {
                (text, cased_starts.as_slice())
            } else {
                (lowered.as_str(), lowered_starts.as_slice())
            };
            mark_atom(atom, haystack, starts, &mut marked);
        }
        marked
            .iter()
            .enumerate()
            .filter_map(|(pos, &hit)| hit.then_some(pos))
            .collect()
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

/// Split `query` into the substring atoms nucleo's [`Pattern::new`] would require,
/// so the filter demands exactly the atoms the label highlight does.
///
/// The ONE splitter: the filter, the preview marks
/// ([`atom_match_positions`](SearchIndex::atom_match_positions), through the
/// finders built here) and nucleo's label highlight all take their atoms from it.
/// Re-splitting a query anywhere else is how two surfaces start disagreeing about
/// which words the user typed.
///
/// Mirrors nucleo's `pattern_atoms` splitter plus its substring-atom unescape:
/// split on an ASCII space that is NOT escaped by an immediately preceding
/// backslash, drop the empty atoms, and unescape `\ ` → ` ` inside each atom (any
/// other backslash stays literal, exactly as nucleo keeps it in the needle).
/// Backslash-free queries — the entire realistic input space for the search box —
/// reduce to a plain space split; the escape handling is what keeps the filter
/// from wrongly rejecting a phrase typed with an escaped space.
///
/// The returned atoms are CASED: the smart-case decision is made per atom by
/// [`AtomFinder::new`], which needs the original casing to make it.
fn gate_atoms(query: &str) -> Vec<String> {
    let mut atoms: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut prev_backslash = false;
    for c in query.chars() {
        if c == ' ' {
            if prev_backslash {
                // Escaped space: turn the pending "\ " into a literal space so the
                // atom is the phrase nucleo will look for, not two atoms.
                current.pop();
                current.push(' ');
                prev_backslash = false;
            } else {
                // Unescaped space: an atom boundary. Drop empties (runs of spaces).
                if !current.is_empty() {
                    atoms.push(std::mem::take(&mut current));
                }
            }
            continue;
        }
        current.push(c);
        prev_backslash = c == '\\';
    }
    if !current.is_empty() {
        atoms.push(current);
    }
    atoms
}

/// True iff EVERY atom occurs as a byte substring of the haystack its own
/// smart-case decision selects — `cased` for a case-sensitive atom, `lowercased`
/// for a case-insensitive one.
///
/// This is the whole filter. `memchr::memmem` is SIMD substring search over raw
/// bytes: no UTF-8 → UTF-32 conversion and no per-entry allocation. Byte
/// substring is exactly char substring here, since UTF-8 is self-synchronizing —
/// a valid needle can only ever match at a char boundary.
///
/// The two haystacks must be the SAME text differing only in case; the per-atom
/// choice between them is what makes `foo BAR` fold `foo` but not `BAR`. Every
/// atom is required (AND semantics). An empty `finders` slice trivially passes —
/// the caller handles the empty-query "return all" case before reaching here.
fn atoms_match(cased: &str, lowercased: &str, finders: &[AtomFinder]) -> bool {
    finders.iter().all(|atom| {
        let haystack = if atom.case_sensitive {
            cased
        } else {
            lowercased
        };
        atom.finder.find(haystack.as_bytes()).is_some()
    })
}

/// Mark every CHAR that one atom covers in `haystack` — the case branch that
/// atom's own smart-case decision selected — translating each byte hit back into
/// source char positions through `starts`.
///
/// Occurrences are taken OVERLAPPING: the scan resumes one byte past a hit's
/// START, not past its end, so `aa` over `aaa` covers all three chars. The caller
/// marks a union, and a reader looking at `aaa` sees the whole word emphasized;
/// stopping at non-overlapping hits would leave the tail char plain for no reason
/// anyone could see. Resuming mid-char is harmless — UTF-8 is self-synchronizing,
/// so a valid needle can only ever match at a char boundary.
///
/// Fail-soft on the mark itself (`get_mut`): a position outside `marked` is
/// skipped rather than panicking, so a byte/char map that ever disagreed with the
/// text would misplace a mark, never take the board down.
fn mark_atom(atom: &AtomFinder, haystack: &str, starts: &[usize], marked: &mut [bool]) {
    let needle_len = atom.finder.needle().len();
    if needle_len == 0 {
        return;
    }
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(offset) = atom.finder.find(&bytes[from..]) {
        let start = from + offset;
        for pos in char_span(starts, start, start + needle_len) {
            if let Some(mark) = marked.get_mut(pos) {
                *mark = true;
            }
        }
        from = start + 1;
    }
}

/// The SOURCE char positions a haystack byte range `start..end` covers.
///
/// `starts` is ascending, so each end is a binary search rather than a re-scan of
/// the text: the char whose bytes contain `start`, through the last char that
/// begins before `end`. That keeps a line with many hits linear-ish instead of
/// counting chars from the top of the line once per hit.
fn char_span(starts: &[usize], start: usize, end: usize) -> Range<usize> {
    let first = starts
        .partition_point(|&byte| byte <= start)
        .saturating_sub(1);
    let last = starts.partition_point(|&byte| byte < end);
    first..last
}

/// Byte offset of each CHAR of `text`, ascending — the map [`char_span`] reads to
/// answer a memmem hit in CHAR positions.
fn char_starts(text: &str) -> Vec<usize> {
    text.char_indices().map(|(byte, _)| byte).collect()
}

/// `text` lowercased the way the filter lowercases its haystacks, paired with the
/// byte offset at which each SOURCE char begins inside that lowercased string.
///
/// The map is what makes a hit in the lowercased haystack addressable in the
/// ORIGINAL text's char positions, which is what gets marked. It cannot be assumed
/// 1:1: lowercasing can LENGTHEN a char (`İ` → `i̇`, one char into two), so the
/// k-th lowercased char is not the k-th source char.
///
/// The string itself comes from [`str::to_lowercase`] — the same function the
/// filter's haystacks are built with — while the offsets are accumulated per char.
/// The two agree because the ONLY context-sensitive mapping in that function is a
/// Greek capital sigma, whose two lowercase forms (`σ`, `ς`) are both one char and
/// two bytes. The `debug_assert` states that invariant where it would break, and
/// [`char_span`] plus the `get_mut` in [`mark_atom`] keep a future divergence to a
/// misplaced mark rather than a panic.
fn lowered_with_char_starts(text: &str) -> (String, Vec<usize>) {
    let lowered = text.to_lowercase();
    let mut starts = Vec::with_capacity(text.len());
    let mut byte = 0usize;
    for ch in text.chars() {
        starts.push(byte);
        byte += ch.to_lowercase().map(char::len_utf8).sum::<usize>();
    }
    debug_assert_eq!(
        byte,
        lowered.len(),
        "per-char and per-string lowercasing must agree in byte length"
    );
    (lowered, starts)
}

/// One-shot substring filter: match `sessions` against `query` in `mode`.
///
/// Returns the session indices INTO `sessions` that match, in the input order.
/// An empty (or whitespace-only) query returns every index. `mode` is the single
/// toggle between name-only and name+content matching.
///
/// This builds a throwaway [`SearchIndex`]; long-lived callers (the TUI) should
/// hold a [`SearchIndex`] and drive it incrementally instead.
///
/// The TUI holds a live index rather than calling this, so it is unused on the
/// bin runtime path; it is the primary driver of the membership/mode unit tests
/// and a documented one-shot convenience, hence retained + `dead_code` allowed.
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
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    /// Collect matched indices into a SET.
    ///
    /// The filter's contract is WHICH entries match, never in what order: it
    /// returns candidates in the order given and `App::order_filtered` imposes
    /// display order. Asserting on a set is what keeps these tests pinning the
    /// contract rather than an implementation detail — see
    /// [`filter_membership_ignores_match_closeness`].
    fn as_set(indices: &[usize]) -> BTreeSet<usize> {
        indices.iter().copied().collect()
    }

    /// The match set nucleo ITSELF produces for `query` — the pre-change filter
    /// path, reconstructed.
    ///
    /// Before this module answered membership with memmem, the filter scored
    /// every entry's CASED haystack with this exact `Pattern` and kept the
    /// `Some`s. Keeping that path alive as an ORACLE is what makes the parity
    /// claim a proof: hand-written expectations would encode whatever the author
    /// believed nucleo's smart-case rules to be, which is precisely the thing
    /// under test (the `NPX` trap is a measured case where that belief is wrong).
    /// Asking nucleo cannot be wrong about nucleo.
    ///
    /// The deliberate divergences are NOT covered by this oracle and must be kept
    /// out of any corpus compared against it — see the module docs, and
    /// [`non_ascii_tail_match_filters_in_though_nucleo_highlight_misses_it`].
    fn nucleo_match_set(query: &str, sessions: &[Session], mode: SearchMode) -> BTreeSet<usize> {
        let pattern = Pattern::new(
            query,
            CaseMatching::Smart,
            Normalization::Smart,
            AtomKind::Substring,
        );
        if pattern.atoms.is_empty() {
            return (0..sessions.len()).collect();
        }
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut buf: Vec<char> = Vec::new();
        sessions
            .iter()
            .map(|s| Entry::build(s, fingerprint(s)))
            .enumerate()
            .filter_map(|(i, entry)| {
                let haystack = Utf32Str::new(entry.haystack(mode), &mut buf);
                pattern.score(haystack, &mut matcher).map(|_| i)
            })
            .collect()
    }

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
            root_uuid: None,
            msg_count: 0,
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

    /// Match CLOSENESS decides membership not at all — and never decided what
    /// the user saw either.
    ///
    /// This test used to assert that the exact `cat` RANKED ABOVE the mid-word
    /// `concatenate`. That rank had no runtime consumer and provably could not
    /// reach the screen: `App::recompute_filtered` hands every result straight to
    /// `App::order_filtered`, which re-sorts by `(Reverse(timestamp),
    /// session_id)` — a TOTAL order with zero ties (measured: 66 entries → 66
    /// distinct keys), so not even sort stability could leak the old rank out.
    /// The property was pinned for its own sake, which is exactly why the filter
    /// could stop scoring with nucleo at no user-visible cost. The name is kept
    /// pointed at that finding so the next reader does not "restore" a rank
    /// nothing reads. What IS contractual is MEMBERSHIP: both spellings match.
    #[test]
    fn filter_membership_ignores_match_closeness() {
        // Both contain "cat" as a CONTIGUOUS substring: s0 mid-word inside
        // "con[cat]enate", s1 as the exact short word.
        let sessions = [
            session("far", "concatenate helper", ""),
            session("near", "cat", ""),
        ];

        let matched = filter("cat", &sessions, SearchMode::NameOnly);
        assert_eq!(
            as_set(&matched),
            BTreeSet::from([0, 1]),
            "a mid-word hit and an exact hit are equally members: {matched:?}"
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

    /// The row-LABEL highlight seam marks a contiguous substring hit exactly, and
    /// marks NOTHING for a scattered (non-contiguous) subsequence — the seam agrees
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

    /// The chars `atom_match_positions` marked, run by run, joined with `+`.
    ///
    /// Runs rather than raw positions: per-atom marking puts SEVERAL runs on one
    /// string, and a position list cannot tell "marked the query" from "marked
    /// whatever sits at a borrowed offset".
    fn marked_runs(index: &SearchIndex, text: &str) -> String {
        let positions = index.atom_match_positions(text);
        let mut runs: Vec<String> = Vec::new();
        let mut run = String::new();
        let mut previous: Option<usize> = None;
        for (pos, ch) in text.chars().enumerate() {
            if !positions.contains(&pos) {
                continue;
            }
            if previous.is_some_and(|p| p + 1 != pos) {
                runs.push(std::mem::take(&mut run));
            }
            run.push(ch);
            previous = Some(pos);
        }
        if !run.is_empty() {
            runs.push(run);
        }
        runs.join("+")
    }

    /// The PREVIEW-MARK seam marks PER ATOM: a string carrying only ONE atom of a
    /// multi-atom query is still marked, for that atom.
    ///
    /// This is where it parts company with [`SearchIndex::match_indices`], which
    /// answers only when EVERY atom occurs in the one string it was given. That
    /// whole-string rule is right for a row label and wrong for a transcript LINE:
    /// the filter admitted the session because the atoms occur anywhere in it, so a
    /// line holding one of them is showing the user part of the answer.
    #[test]
    fn atom_match_positions_marks_each_atom_independently() {
        let mut index = SearchIndex::new();
        index.set_query("deploy pipeline");

        // Both atoms in one string: BOTH runs are marked (not just the first).
        assert_eq!(
            marked_runs(&index, "the deploy pipeline ran"),
            "deploy+pipeline"
        );
        // One atom only: still marked, for the atom that is there. The
        // whole-string seam marks nothing here.
        assert_eq!(marked_runs(&index, "the deploy ran green"), "deploy");
        assert_eq!(marked_runs(&index, "the pipeline ran green"), "pipeline");
        assert!(
            index.match_indices("the deploy ran green").is_empty(),
            "the row-LABEL seam requires every atom in the one string; that is the \
             rule this seam deliberately does not share"
        );
        // Neither atom: nothing to mark.
        assert!(index.atom_match_positions("nothing to see").is_empty());
    }

    /// EVERY occurrence is marked, including ones that overlap — the caller marks a
    /// union, so a repeated word is emphasized everywhere it is said.
    #[test]
    fn atom_match_positions_marks_every_occurrence_including_overlaps() {
        let mut index = SearchIndex::new();

        // Repeated word: both occurrences, as two runs.
        index.set_query("webhook");
        assert_eq!(
            marked_runs(&index, "the webhook and the webhook again"),
            "webhook+webhook"
        );

        // Overlapping occurrences ("aa" at 0 and at 1) merge into ONE run covering
        // all three chars, rather than leaving the tail char plain.
        index.set_query("aa");
        assert_eq!(marked_runs(&index, "aaa"), "aaa");

        // Two atoms whose runs ABUT merge into one run too — the union is marked
        // once, never as two overlapping spans.
        index.set_query("dep loy");
        assert_eq!(marked_runs(&index, "deploy now"), "deploy");
    }

    /// Positions are CHAR indices, not byte offsets, on BOTH case branches.
    #[test]
    fn atom_match_positions_uses_char_positions_not_byte_offsets() {
        let mut index = SearchIndex::new();

        // "🚀 deploy now": the rocket is one char but FOUR bytes, so a byte offset
        // would report 5..11 where the char positions are 2..8.
        index.set_query("deploy");
        assert_eq!(
            index.atom_match_positions("🚀 deploy now"),
            vec![2, 3, 4, 5, 6, 7]
        );

        // The same through the CASE-SENSITIVE branch, which searches the cased text.
        index.set_query("Deploy");
        assert_eq!(
            index.atom_match_positions("🚀 Deploy now"),
            vec![2, 3, 4, 5, 6, 7]
        );

        // Unlike the nucleo seam, a match at the very LAST char of a non-ASCII
        // string is found here — the filter's own byte scan has no tail off-by-one,
        // and the marks now follow the filter.
        index.set_query("fin");
        assert_eq!(index.atom_match_positions("café fin"), vec![5, 6, 7]);
    }

    /// A char whose lowercase form is LONGER than itself does not shift the marks.
    ///
    /// `İ` (U+0130) lowercases to TWO chars (`i` + a combining dot), so the k-th
    /// char of the lowercased haystack is not the k-th char of the text. Assuming
    /// they line up marks the wrong chars — here it would run two positions past
    /// the word and off the end of the string.
    #[test]
    fn atom_match_positions_survives_a_lengthening_lowercase() {
        let mut index = SearchIndex::new();
        index.set_query("stanbul");
        // "İstanbul": chars İ(0) s(1) t(2) a(3) n(4) b(5) u(6) l(7).
        assert_eq!(
            index.atom_match_positions("İstanbul"),
            vec![1, 2, 3, 4, 5, 6, 7]
        );
        assert_eq!(marked_runs(&index, "İstanbul"), "stanbul");
    }

    /// Smart case rides each ATOM here exactly as it does in the filter, because
    /// these are the filter's own finders: `foo BAR` folds `foo` and does not fold
    /// `BAR`, within one call.
    #[test]
    fn atom_match_positions_keeps_smart_case_per_atom() {
        let mut index = SearchIndex::new();
        index.set_query("foo BAR");

        // The lowercase atom folds (marks `Foo`); the uppercase one does not fold
        // and finds the cased `BAR`.
        assert_eq!(marked_runs(&index, "Foo and BAR"), "Foo+BAR");
        // Same string, lowercase `bar`: the case-SENSITIVE atom must not mark it.
        assert_eq!(marked_runs(&index, "Foo and bar"), "Foo");
    }

    /// An empty or whitespace-only query parses to zero atoms and marks nothing,
    /// and so does empty text.
    #[test]
    fn atom_match_positions_is_empty_without_atoms_or_text() {
        let mut index = SearchIndex::new();

        index.set_query("");
        assert!(index.atom_match_positions("anything at all").is_empty());
        index.set_query("   ");
        assert!(index.atom_match_positions("anything at all").is_empty());
        index.set_query("anything");
        assert!(index.atom_match_positions("").is_empty());
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

    /// `gate_atoms` splits a query into the same atoms nucleo requires: on
    /// unescaped ASCII spaces, dropping empties, and unescaping `\ ` → ` `.
    #[test]
    fn gate_atoms_splits_like_nucleo() {
        assert_eq!(gate_atoms("foo"), vec!["foo"]);
        assert_eq!(gate_atoms("foo bar"), vec!["foo", "bar"]);
        // Runs of spaces produce no empty atoms.
        assert_eq!(gate_atoms("  foo   bar  "), vec!["foo", "bar"]);
        // Empty / whitespace-only yields no atoms (the "match all" case).
        assert!(gate_atoms("").is_empty());
        assert!(gate_atoms("   ").is_empty());
        // An escaped space keeps the phrase as ONE atom (nucleo's `\ ` unescape),
        // so the gate looks for the literal "foo bar" rather than two atoms.
        assert_eq!(gate_atoms(r"foo\ bar"), vec!["foo bar"]);
        // A stray backslash stays literal in the atom, exactly as nucleo keeps it.
        assert_eq!(gate_atoms(r"a\b"), vec![r"a\b".to_string()]);
    }

    /// `atoms_match` requires every atom as a BYTE substring (not a subsequence)
    /// of the haystack its own case decision selects; an empty atom list
    /// trivially passes.
    #[test]
    fn atoms_match_requires_every_atom_as_byte_substring() {
        let finders: Vec<AtomFinder> = ["foo", "bar"].iter().map(|a| AtomFinder::new(a)).collect();
        // All-lowercase atoms are case-insensitive, so they read the lowercased
        // haystack; here the two haystacks carry the same text.
        assert!(atoms_match("a foo and a bar", "a foo and a bar", &finders));
        assert!(
            !atoms_match("only foo here", "only foo here", &finders),
            "a missing atom fails the match"
        );
        // No atoms => trivially passes (the empty query is handled earlier).
        assert!(atoms_match("anything", "anything", &[]));
        // Substring, not subsequence: "ac" is not a byte substring of "abc".
        let ac: Vec<AtomFinder> = vec![AtomFinder::new("ac")];
        assert!(
            !atoms_match("abc", "abc", &ac),
            "a scattered subsequence must not match"
        );

        // The PER-ATOM haystack selection. The two haystacks are fed DELIBERATELY
        // different text ("a BAR" vs "a bar"), because identical arguments would
        // pass whichever haystack the code picked and could not tell the branches
        // apart — the exact fixture mistake that hides an inclusion regression.
        let upper: Vec<AtomFinder> = vec![AtomFinder::new("BAR")];
        assert!(
            atoms_match("a BAR", "a bar", &upper),
            "an uppercase atom is case-SENSITIVE and reads the cased haystack"
        );
        assert!(
            !atoms_match("a bar", "a bar", &upper),
            "a case-sensitive atom must not match lowercase text"
        );
        let lower: Vec<AtomFinder> = vec![AtomFinder::new("bar")];
        assert!(
            atoms_match("a BAR", "a bar", &lower),
            "a lowercase atom is case-INSENSITIVE and reads the lowercased haystack"
        );
    }

    /// Multi-atom content query: two whitespace-separated atoms present only in
    /// the CONTENT match in name+content mode; if only one atom is present, the
    /// entry is excluded (every atom is required).
    #[test]
    fn multi_atom_content_query_requires_all_atoms() {
        let sessions = [session(
            "s0",
            "unrelated title",
            "the deploy pipeline ran green",
        )];
        assert_eq!(
            filter("deploy pipeline", &sessions, SearchMode::NameAndContent),
            vec![0],
            "both atoms present only in content must match in name+content mode"
        );
        assert!(
            filter("deploy rollback", &sessions, SearchMode::NameAndContent).is_empty(),
            "a missing atom (rollback) must exclude the entry"
        );
    }

    /// The gate is case-insensitive over content: a lowercase query matches
    /// mixed-case content (nucleo agrees, since a lowercase query is smart-case
    /// insensitive), through the prebuilt lowercased haystack.
    #[test]
    fn gate_is_case_insensitive_over_content() {
        let sessions = [session("s0", "notes", "Deployed the API Gateway")];
        assert_eq!(
            filter("gateway", &sessions, SearchMode::NameAndContent),
            vec![0],
            "a lowercase query matches mixed-case content case-insensitively"
        );
    }

    /// A query matches an ASCII substring embedded in NON-ASCII content without
    /// panicking, kept off the tail so the upstream non-ASCII substring
    /// off-by-one (noted in `set_query`) never applies.
    #[test]
    fn non_ascii_content_substring_matches_without_panic() {
        let sessions = [session(
            "s0",
            "café notes",
            "réfléchi ☕ the deploy pipeline shipped ☕ fin",
        )];
        // ASCII query, ASCII substring mid-way through non-ASCII content: the
        // memmem filter finds it by byte scan. Not at the tail.
        assert_eq!(
            filter("deploy", &sessions, SearchMode::NameAndContent),
            vec![0],
            "an ASCII substring inside non-ASCII content matches"
        );
        // A non-ASCII query over the non-ASCII label also matches without
        // panicking (again not at the tail: " notes" follows).
        assert_eq!(
            filter("café", &sessions, SearchMode::NameOnly),
            vec![0],
            "a non-ASCII substring at the label head matches"
        );
    }

    /// A name hit and a content-only hit are BOTH members in name+content mode —
    /// where the hit landed is not a contract.
    ///
    /// This test used to assert the name hit RANKED ABOVE the content-only hit,
    /// under a heading claiming ranking was "preserved through the gate". Same
    /// finding as [`filter_membership_ignores_match_closeness`]: that order was
    /// re-sorted away by `App::order_filtered` before any of it reached a row, so
    /// it pinned nothing a user could observe. Both entries being INCLUDED is the
    /// real contract, and it is the one that would actually break if name+content
    /// mode stopped folding the label into the content haystack.
    #[test]
    fn membership_includes_both_name_hit_and_content_only_hit() {
        // s0 carries "deploy" in its NAME; s1 only deep in its CONTENT.
        let sessions = [
            session("name_hit", "deploy dashboard", "unrelated body prose"),
            session(
                "content_hit",
                "weekly status",
                "a long note that eventually mentions deploy near the end",
            ),
        ];
        let matched = filter("deploy", &sessions, SearchMode::NameAndContent);
        assert_eq!(
            as_set(&matched),
            BTreeSet::from([0, 1]),
            "a name hit and a content-only hit are both members: {matched:?}"
        );
    }

    /// PARITY: the memmem filter returns nucleo's OWN match set, across every
    /// query shape that has its own code path, in BOTH modes.
    ///
    /// Proved against [`nucleo_match_set`] — the pre-change scoring path itself —
    /// rather than against hand-written expectations, so the assertion cannot
    /// quietly encode the author's belief about smart case instead of nucleo's
    /// behaviour.
    ///
    /// The corpus is built so the module's documented divergences never fire (no
    /// accent-folded match, no match on a non-ASCII haystack's last char); those
    /// are pinned separately and deliberately, not swept in here.
    #[test]
    fn membership_matches_nucleo_across_query_shapes_and_modes() {
        let sessions = [
            // Lowercase `npx`, content-only: the uppercase trap's bait.
            session("s0", "build notes", "ran npx create-react-app here"),
            // Uppercase `NPX` in content, and `Npx` in a label.
            session("s1", "release checklist", "the NPX shim is pinned"),
            session("s2", "Npx wrapper", "unrelated body prose"),
            // The per-atom smart-case pair, for the query `foo BAR`. The capital
            // `F` in the LABEL is load-bearing and must not be "tidied" to `foo`:
            // it is the only thing that makes per-ATOM and per-QUERY case
            // distinguishable. Correct (per atom): `foo` folds and matches `Foo`,
            // `BAR` does not fold and matches s3 only. Broken (per query): the
            // uppercase `BAR` turns the WHOLE query case-sensitive, `foo` stops
            // matching `Foo`, and s3 silently drops out. With a lowercase `foo`
            // label both readings agree and the bug walks straight through.
            session("s3", "Foo dashboard", "the BAR chart shipped"),
            session("s4", "Foo dashboard", "the bar chart shipped"),
            // An escaped-space phrase, plus the two atoms apart (never adjacent).
            session("s5", "deploy pipeline notes", "shipped green"),
            session(
                "s6",
                "pipeline for deploy",
                "atoms present but not adjacent",
            ),
            // Non-ASCII haystack, matched away from the tail.
            session("s7", "café notes", "réfléchi ☕ the deploy ran ☕ fin"),
            // Matches nothing.
            session("s8", "unrelated", "nothing to see"),
        ];

        let queries = [
            "npx",               // all-lowercase
            "NPX",               // uppercase (smart-case => case-sensitive)
            "Npx",               // mixed-case
            "foo BAR",           // mixed-case MULTI-atom: per-atom smart case
            "deploy pipeline",   // multi-atom
            r"deploy\ pipeline", // escaped space => ONE atom, the literal phrase
            "café",              // non-ASCII query
            "",                  // empty
            "   ",               // whitespace-only
            "zzzqqq",            // matches nothing
        ];

        for query in queries {
            for mode in [SearchMode::NameOnly, SearchMode::NameAndContent] {
                assert_eq!(
                    as_set(&filter(query, &sessions, mode)),
                    nucleo_match_set(query, &sessions, mode),
                    "membership must equal nucleo's match set for {query:?} in {mode:?}"
                );
            }
        }
    }

    /// The measured INCLUSION-REGRESSION trap: an uppercase-bearing query must
    /// return the EMPTY set over a corpus whose lowercased haystack is full of
    /// matches.
    ///
    /// This is not a hypothetical. Against the real corpus, `NPX` finds **6**
    /// entries in the lowercased haystack and nucleo matches **0** — so a filter
    /// that answered from the lowercased haystack regardless of case (or branched
    /// on case per QUERY rather than per ATOM) would wrongly surface 6 rows.
    /// nucleo's `CaseMatching::Smart` makes an uppercase-bearing atom
    /// case-SENSITIVE, and this test fails loudly if that branch ever regresses.
    #[test]
    fn uppercase_query_excludes_lowercase_only_matches() {
        let sessions = [
            session("s0", "build notes", "ran npx create-react-app"),
            session("s1", "more notes", "npx tsc --noEmit"),
        ];

        // The bait: lowercased, every entry matches. If this ever stops holding,
        // the assertions below would pass vacuously over an empty corpus.
        assert_eq!(
            as_set(&filter("npx", &sessions, SearchMode::NameAndContent)),
            BTreeSet::from([0, 1]),
            "the lowercase query must match both entries"
        );

        // The trap: uppercase atoms are case-sensitive, and no entry carries
        // `NPX`/`Npx` in its cased text.
        for query in ["NPX", "Npx"] {
            let matched = filter(query, &sessions, SearchMode::NameAndContent);
            assert!(
                matched.is_empty(),
                "{query:?} is case-sensitive under smart case and must match nothing, got {matched:?}"
            );
        }
    }

    /// The ACCEPTED divergence: a match ending at the very LAST char of a
    /// non-ASCII haystack is found by the filter but missed by nucleo's
    /// highlight, so such a row renders with no highlight.
    ///
    /// Upstream `nucleo-matcher` 0.3.1 scans candidate starts only to
    /// `haystack.len() - needle.len()` EXCLUSIVE on its non-ASCII substring path
    /// (`exact.rs::substring_match_non_ascii`), so it never considers the final
    /// start position. Filter and highlight used to share one `Pattern` and so
    /// missed it identically; they no longer do.
    ///
    /// This is tolerated on purpose, per the resolved design decision. The filter
    /// is the MORE CORRECT of the two — it errs toward more matches — the case
    /// needs a non-ASCII haystack AND a tail-anchored match, and an un-highlighted
    /// row is already an expected sight (a content-only match shows none, by
    /// design). Reproducing the upstream bug in the filter to keep the two
    /// symmetrical was explicitly rejected. This test exists so the divergence can
    /// never become silent drift.
    #[test]
    fn non_ascii_tail_match_filters_in_though_nucleo_highlight_misses_it() {
        // "café fin": non-ASCII (é), and "fin" ends at the very last char.
        let sessions = [session("s0", "café fin", "")];

        assert_eq!(
            filter("fin", &sessions, SearchMode::NameOnly),
            vec![0],
            "the memmem filter finds a tail-anchored match in a non-ASCII haystack"
        );

        // The pre-change filter path DROPPED this row outright: this is a
        // deliberate inclusion change, not merely a highlight quirk.
        assert!(
            nucleo_match_set("fin", &sessions, SearchMode::NameOnly).is_empty(),
            "the old nucleo-scored filter excluded this row; the memmem filter keeps it"
        );

        let mut index = SearchIndex::new();
        index.set_query("fin");
        assert!(
            index.match_indices("café fin").is_empty(),
            "nucleo's highlight misses the same match (upstream tail off-by-one), \
             so the row draws unhighlighted — accepted, not a bug to fix here"
        );

        // The divergence is confined to the tail: move the match off the last
        // char and nucleo agrees again, which is what makes it narrow.
        let off_tail = [session("s0", "café fin.", "")];
        assert_eq!(
            filter("fin", &off_tail, SearchMode::NameOnly),
            vec![0],
            "filter still matches one char off the tail"
        );
        index.set_query("fin");
        assert_eq!(
            index.match_indices("café fin."),
            vec![5, 6, 7],
            "and nucleo highlights it once the match is not tail-anchored"
        );
    }

    /// A refresh rebuilds the lowercased GATE haystack only for changed sessions:
    /// the reused entry keeps a working gate, and the rebuilt one reflects the
    /// new text.
    #[test]
    fn refresh_rebuilds_lowercased_gate_only_for_changed_session() {
        let v1 = [
            session("s1", "alpha", "steady gate body"),
            session("s2", "beta", "changing gate body"),
        ];
        let mut index = SearchIndex::build(&v1);
        index.set_mode(SearchMode::NameAndContent);

        // v2: s1 is byte-identical (reused); s2's content changed.
        let v2 = [
            session("s1", "alpha", "steady gate body"),
            session("s2", "beta", "now mentions redeploy"),
        ];
        index.refresh(&v2);
        assert_eq!(
            index.last_rebuilt(),
            1,
            "only the changed session's lowercased haystack is rebuilt"
        );

        // The REUSED entry still matches through its preserved lowercased gate.
        index.set_query("steady");
        assert_eq!(
            index.results(),
            vec![0],
            "the reused session still gates via its preserved lowercased haystack"
        );

        // The REBUILT entry reflects its new content through the gate.
        index.set_query("redeploy");
        assert_eq!(
            index.results(),
            vec![1],
            "the rebuilt session matches its new content"
        );
    }

    /// `results_within` ranks ONLY the given candidate entries: a session that
    /// matches the query but is absent from `candidates` is excluded, even though
    /// the whole-index `results` surfaces it.
    #[test]
    fn results_within_restricts_to_candidates() {
        let sessions = [
            session("s0", "deploy dashboard", ""),
            session("s1", "deploy pipeline", ""),
            session("s2", "unrelated notes", ""),
        ];
        let mut index = SearchIndex::build(&sessions);
        index.set_query("deploy");

        // Whole-index results surface both matches (s0, s1), not s2.
        let all = index.results();
        assert!(
            all.contains(&0) && all.contains(&1) && !all.contains(&2),
            "results ranks both matching entries and excludes the non-match: {all:?}"
        );

        // Restricting the candidate set to {1, 2} drops the matching s0 (not a
        // candidate) and the non-matching s2, leaving only s1.
        assert_eq!(
            index.results_within(&[1, 2]),
            vec![1],
            "results_within keeps only candidates that match; the out-of-set s0 is excluded"
        );
    }

    /// Parity: `results_within` over EVERY index yields the same match SET as the
    /// whole-index `results` for the same query and mode.
    ///
    /// Set-equality, not sequence-equality: the two entry points share one core
    /// and the contract they must agree on is which entries match. Asserting the
    /// sequence would additionally pin the traversal order of a list
    /// `App::order_filtered` re-sorts anyway.
    #[test]
    fn results_within_over_all_indices_matches_results() {
        let sessions = [
            session("s0", "deploy dashboard", "shipped the release"),
            session(
                "s1",
                "weekly status",
                "a note mentioning deploy near the end",
            ),
            session("s2", "unrelated", "nothing to see"),
        ];
        let mut index = SearchIndex::build(&sessions);
        index.set_mode(SearchMode::NameAndContent);
        index.set_query("deploy");

        let all: Vec<usize> = (0..sessions.len()).collect();
        let within = index.results_within(&all);
        assert_eq!(
            as_set(&within),
            as_set(&index.results()),
            "results_within over all indices must match results for the same query/mode"
        );
        assert_eq!(
            as_set(&within),
            BTreeSet::from([0, 1]),
            "and both must be the real match set, not a shared empty one: {within:?}"
        );
    }

    /// An empty candidate slice ranks nothing, and an out-of-range candidate
    /// index is skipped fail-soft (never a panic / out-of-bounds).
    #[test]
    fn results_within_empty_and_out_of_range_candidates_are_safe() {
        let sessions = [
            session("s0", "deploy dashboard", ""),
            session("s1", "deploy pipeline", ""),
        ];
        let mut index = SearchIndex::build(&sessions);
        index.set_query("deploy");

        // No candidates => no results, even though the query matches entries.
        assert!(
            index.results_within(&[]).is_empty(),
            "an empty candidate set yields no results"
        );

        // Out-of-range indices (>= len) are skipped without panicking; only the
        // valid, matching candidate survives.
        assert_eq!(
            index.results_within(&[0, 99, 42]),
            vec![0],
            "out-of-range candidate indices are skipped fail-soft"
        );
    }

    /// The gate contract holds INSIDE a restricted candidate set: multi-atom
    /// (every atom required), case-insensitive, and substring — not a scattered
    /// subsequence.
    #[test]
    fn results_within_preserves_gate_semantics() {
        let sessions = [
            session("s0", "ignored", "the Deploy Pipeline ran green"),
            session("s1", "ignored", "deploy only, nothing else"),
            session("s2", "ignored", "deploy pipeline present here too"),
        ];
        let mut index = SearchIndex::build(&sessions);
        index.set_mode(SearchMode::NameAndContent);

        // Candidate set excludes s2, proving restriction AND gate semantics at
        // once. Multi-atom + case-insensitive: "deploy pipeline" matches s0
        // (mixed-case) but not s1 (missing the "pipeline" atom).
        index.set_query("deploy pipeline");
        assert_eq!(
            index.results_within(&[0, 1]),
            vec![0],
            "both atoms required, case-insensitively, within the candidate set"
        );

        // Substring, not subsequence: "dpp" is a scattered subsequence of
        // "deploy pipeline" but never a contiguous run, so it matches nothing.
        index.set_query("dpp");
        assert!(
            index.results_within(&[0, 1]).is_empty(),
            "a scattered subsequence must not match within the candidate set"
        );
    }

    /// An empty / whitespace-only query returns the given candidates unchanged
    /// (order preserved), mirroring results()'s empty-query "return all"; an
    /// out-of-range index is still dropped fail-soft.
    #[test]
    fn results_within_empty_query_returns_candidates_as_is() {
        let sessions = [
            session("s0", "first", ""),
            session("s1", "second", ""),
            session("s2", "third", ""),
        ];
        let mut index = SearchIndex::build(&sessions);

        // Empty query: candidates returned as-is, in the order supplied.
        index.set_query("");
        assert_eq!(
            index.results_within(&[2, 0]),
            vec![2, 0],
            "an empty query returns the candidates unchanged, order preserved"
        );

        // Whitespace-only parses to zero atoms and behaves the same; the
        // out-of-range 99 is dropped fail-soft.
        index.set_query("   ");
        assert_eq!(
            index.results_within(&[1, 99]),
            vec![1],
            "whitespace-only returns in-range candidates as-is, skipping out-of-range"
        );
    }
}
