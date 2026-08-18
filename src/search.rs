//! Substring search over sessions: ONE SIMD membership matcher that admits the
//! rows AND marks both surfaces that show why.
//!
//! Isolates every `memchr` call so the matcher lives in one module (Risks
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
//! An atom matches case-sensitively iff **that atom** carries an uppercase char
//! — `foo BAR` matches `foo` case-insensitively and `BAR` case-sensitively,
//! within one query. (This is nucleo's `CaseMatching::Smart` rule, adopted
//! wholesale when nucleo answered membership and kept because it is the right
//! rule, not because anything still compares against it at runtime.) The
//! decision therefore rides each [`AtomFinder`], never the query as a whole, and
//! each atom picks its own haystack accordingly. A per-QUERY branch would look
//! equivalent and silently break mixed queries.
//!
//! This branch is **mandatory, not an optimization**. Answering an
//! uppercase-bearing query from the lowercased haystack would include rows smart
//! case excludes: measured, `NPX` finds 6 entries there while the smart-case rule
//! matches 0. That is an inclusion regression, not a ranking nuance.
//!
//! # The content AND is bounded to a PROXIMITY WINDOW
//!
//! In [`SearchMode::NameAndContent`], a MULTI-atom query matches when every atom
//! occurs in the LABEL, or when every atom occurs within one window of the others
//! inside the content haystack. The window is
//! `max(`[`PROXIMITY_WINDOW_MIN_BYTES`]`, `[`PROXIMITY_WINDOW_QUERY_MULTIPLIER`]`
//! × the query's byte length)`. A SINGLE-atom query takes the plain [`atoms_match`]
//! path unchanged, and [`SearchMode::NameOnly`] is untouched in both cases.
//!
//! State the window arm's haystack plainly, because it is part of the rule and not
//! an implementation detail: it runs over the COMBINED string — the label at byte
//! 0, a newline, then the content index — so a pair STRADDLING that seam
//! co-occurs. One atom in the label and one in the opening lines of the transcript
//! is a match that NEITHER literal arm admits on its own. That is deliberate: the
//! label is the transcript's own opening words, so a hit spanning the join is near
//! in exactly the sense the window means. It also makes the rule a strict SUPERSET
//! of the label arm rather than an alternative to it.
//!
//! The unbounded AND it replaces barely filtered anything. Measured over the real
//! store (a ONE-OFF probe, 2026-08-18, 310-330 sessions carrying readable text —
//! a dated sample, not a maintained metric), a random TWO-word query matched a
//! median **44.2%** of the corpus, and **48.3%** within the default
//! current-folder scope. Bounding the SPAN the AND is evaluated over takes that
//! to **8.2%** at 200 B, **14.4%** at 400 B and **21.5%** at 800 B.
//!
//! The defect is that SPAN, not the whitespace splitting. Name-only mode applies
//! the identical atom rule over a `store::label::LABEL_MAX` (180-char) label and
//! shows none of it: two words co-occurring in a 12-75 KB transcript say almost
//! nothing, while two words within a paragraph of each other say the user is
//! remembering one exchange. So the window rides the CONTENT arm alone.
//!
//! Exact-phrase-by-default was considered and REJECTED: 12.3% / 21.4% / 32.0% of
//! remembered 3 / 5 / 8-word runs contain a NEWLINE, and would silently return
//! nothing; it also breaks the pasted-snippet path, which flattens newlines to
//! spaces before it ever reaches here. No query syntax is added either — no
//! quotes, no phrase mode — for the reason [`set_query`](SearchIndex::set_query)
//! already gives: what matching MEANS must never depend on what the user types.
//! The proportional half of the window is what carries a paste instead, and it
//! recovered 108/108 flattened 3-line snippets in the same probe.
//!
//! Mechanically ([`atoms_co_occur_within`]): count each atom's occurrences over
//! the PREBUILT haystack — that is what today's AND already costs — anchor on the
//! RAREST atom, and answer the others from a bounded slice around each of its
//! occurrences. The rarest atom occurred p50=3, p90=11, p99=34 times per matching
//! session (max 79), so that is a few dozen short scans.
//!
//! [`PROXIMITY_MAX_ANCHORS`] bounds that, and it is asked of the ANCHOR — the
//! RAREST atom — and of nothing else, since only the rarest bounds every other.
//! Applying the cap per atom would fire on any query carrying an ordinary word
//! (`the` occurs four figures of times in a real transcript) while the rarest atom
//! still had a handful of hits, and hand that query straight back the unbounded
//! AND this section exists to replace. The anchor itself overflows on a
//! pathological GENERATED file and, harmlessly, on an all-common-word QUERY over
//! an ORDINARY one — atoms that frequent co-occur inside the window regardless, so
//! the unbounded answer admits nothing. The other atoms' counts are not
//! load-bearing either way: the scan costs (anchor hits × atoms × window), so
//! capping them would buy nothing, and every list is bounded as it is collected
//! so that finding the rarest never materialises a runaway one.
//!
//! The MARKS do not move: [`atom_match_positions`](SearchIndex::atom_match_positions)
//! stays per atom, per rendered line. The window only makes it likelier that
//! co-occurring atoms land on ONE line, so a bounded AND produces fewer
//! "match outside preview" notices, not more.
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
//! The two also share ONE BYTE SPACE, which is now load-bearing rather than
//! incidental: the proximity window above measures a distance between a hit in
//! the cased haystack and a hit in the lowercased one, and that is meaningless
//! unless byte offset `n` names the same place in both. So the lowercased sibling
//! is folded PER CHAR by [`lowercase_preserving_byte_len`], which keeps any char
//! whose lowercase form would change its UTF-8 width (`K` U+212A → `k`, 3 bytes →
//! 1; `Å` U+212B → `å`, 3 → 2; `ẞ` U+1E9E → `ß`, 3 → 2) rather than shifting
//! every byte that follows it.
//!
//! **That fold is the module's ONE fold, and every seam takes it.** The filter
//! builds its haystacks with it, the row-label highlight gates on it, and the
//! preview marks search it. This is not tidiness: the surfaces share the
//! [`AtomFinder`]s, and a finder only answers the SAME question on two surfaces if
//! each was handed a haystack lowercased by the same rule. Fold one seam with
//! [`str::to_lowercase`] instead and the two part company on the narrow class the
//! two rules disagree about — a label reading `ΟΔΟΣ notes` is admitted under
//! `οδοσ` (per char, always `σ`) and would then draw with NOTHING marked (per
//! string, a word-final `Σ` is `ς`), and a `K` U+212A label the filter rejects
//! would draw marked. An admitted row that cannot show why it is on the board is
//! the defect this module already fixed once.
//!
//! Lowercasing happens ONCE per entry at build/refresh, never per keystroke. The
//! cost is ~2× the extracted content in memory — bounded by `store::parse`'s
//! `CONTENT_INDEX_CAP` and cheap at this scale (tens of MB store-wide). The
//! marking seams fold per CALL instead — they are handed one short rendered line
//! or one row label, not a transcript.
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
//! # ONE mechanism, TWO rules
//!
//! There is a single matcher below — the per-atom [`AtomFinder`]s — and every
//! surface reads it. What differs between surfaces is the RULE applied, and the
//! rule follows from the SHAPE of what is being asked about:
//!
//! * **WHOLE-STRING, for a row LABEL.**
//!   [`match_indices`](SearchIndex::match_indices) answers only when EVERY atom
//!   occurs in the one string it was given, because a label IS one string and the
//!   row is on the board because that string matched. The predicate is the
//!   filter's own [`atoms_match`] — literally the question that admitted the row.
//! * **PER ATOM, for a previewed TRANSCRIPT.** A transcript is MANY strings, and
//!   the filter admitted the session because the atoms occur ANYWHERE in it. So
//!   [`atom_match_positions`](SearchIndex::atom_match_positions) marks each atom
//!   wherever it lands on one rendered line, union across atoms. Demanding every
//!   atom per LINE would mark nothing the moment a two-word query's words sat on
//!   different lines, and an unmarked pane cannot explain why the row is there.
//!
//! Both rules mark through the SAME per-atom machinery and both take their atoms
//! from the one splitter ([`gate_atoms`]), so "which words" and "where do they
//! occur" are each decided once. A consequence worth stating: a label marks EVERY
//! occurrence of an atom, not one chosen run — `fix search, then fix preview`
//! searched for `fix` marks both, exactly as the preview would.
//!
//! Matching is **substring, not fuzzy**, and that is a property of the mechanism
//! rather than a setting: memmem searches substrings by nature, so a query marks
//! only where it appears as a contiguous run (smart-case), and no user-typed atom
//! syntax can change it. Incrementality comes from the prebuilt haystack strings,
//! untouched across keystrokes (only the tiny finder list is rebuilt); a
//! `SessionsChanged` refresh rebuilds only the entries whose session actually
//! changed.
//!
//! Every `memchr` call is contained below. The rest of the crate sees only
//! [`SearchIndex`], [`SearchMode`], and [`filter`].
//!
//! # nucleo survives as the ORACLE, not as the matcher
//!
//! nucleo used to answer membership, then only the label highlight; it now
//! answers neither and is a **dev-dependency**. What it is still good for is
//! being unable to be wrong about itself: the parity test
//! `membership_matches_nucleo_across_query_shapes_and_modes` compares this
//! module's match set against nucleo's own, so the smart-case rule above is a
//! proof rather than a belief (hand-written expectations would encode whatever
//! the author believed smart case to be — precisely the thing under test; the
//! `NPX` trap is a measured case where that belief is wrong).
//!
//! nucleo's AND is UNBOUNDED, so the oracle pins parity only WITHIN the
//! proximity window: its corpus strings sit far under
//! [`PROXIMITY_WINDOW_MIN_BYTES`], which is what keeps the two comparable at all.
//! Widening a corpus entry past that bound would make the oracle disagree by
//! design — the disagreement IS the fix — so the bounded rule is pinned by its
//! own tests instead.
//!
//! The oracle's corpus is built so the following never fire, because on each of
//! them this module deliberately differs from nucleo:
//!
//! * **Unicode normalization.** nucleo's `Normalization::Smart` accent-folds
//!   (`resume` matching `résumé`); a byte search does not, so such a match is
//!   excluded — from admission AND from both marking rules, which is what keeps
//!   them consistent.
//! * **Length-preserving lowercasing.** Every lowercased haystack in the module —
//!   the filter's, the label gate's, the preview marks' — is folded per char by
//!   [`lowercase_preserving_byte_len`], which KEEPS any char whose
//!   lowercase form would change its UTF-8 byte length — the price of the one
//!   shared byte space the proximity window needs. nucleo folds per char with no
//!   such constraint, so a char like `İ` (lowercase: two chars) folds for nucleo
//!   and not here. For that narrow class of non-ASCII text this module is the more
//!   restrictive of the two.
//! * **The non-ASCII tail off-by-one.** nucleo-matcher 0.3.1's non-ASCII
//!   substring path (`exact.rs::substring_match_non_ascii`) scans candidate
//!   starts only up to `haystack.len() - needle.len()` EXCLUSIVE, so it misses a
//!   match ending at the very LAST char of a haystack holding any non-ASCII char
//!   (`café fin` searched for `fin`). memmem has no such bound, so here this
//!   module is the more CORRECT of the two.
//! * **The smart-case predicate on a non-ASCII atom.** The rule here is the
//!   Unicode `Uppercase` property (`char::is_uppercase`), which over ASCII — the
//!   realistic input space — is exactly nucleo's `is_ascii_uppercase`. nucleo
//!   consults a case-folding table instead, so the two can differ on exotica such
//!   as titlecase digraphs (`ǅ`), where nucleo would match case-sensitively and
//!   this module matches case-insensitively.
//!
//! # A note on `#[allow(dead_code)]`
//!
//! `snapback` is a *binary* crate, so `pub` does not make an item reachable — the
//! `dead_code` lint fires on any public API the `main` runtime path does not
//! call, even when the item is fully exercised by this module's unit tests. A
//! few items below are exactly that: the deliberate, unit-tested search API
//! surface (the single matcher isolation seam per the Risks table) that the TUI
//! either reaches through a sibling method or does not yet consume. Each such
//! item carries a *narrowly-scoped* `#[allow(dead_code)]` with a reason — never
//! a crate- or module-wide blanket — so the lint stays sharp everywhere else.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::ops::Range;

use memchr::memmem;

use crate::store::Session;

/// Floor for the proximity window, in BYTES: how far apart two atoms may sit in
/// the content haystack and still count as co-occurring.
///
/// 200 B is about a paragraph — the span a user who remembers one exchange is
/// actually pointing at. Unbounded, a two-word AND matched a median 44.2% of the
/// corpus; at this floor it matches 8.2% (one-off probe, 2026-08-18 — see the
/// module docs). Lower starts dropping a genuine memory of a single exchange;
/// higher drifts back toward "both words appear in this session somewhere",
/// which is the defect being fixed.
const PROXIMITY_WINDOW_MIN_BYTES: usize = 200;

/// How many times the query's own byte length the window grows to, when that
/// beats the floor above.
///
/// A LONG query describes a LONG span of text — a pasted snippet is the case
/// that matters — so its atoms are legitimately further apart than a typed
/// pair's. 3x is the slack that recovered 108/108 flattened 3-line pastes in the
/// same probe while leaving short typed queries sitting on the floor.
const PROXIMITY_WINDOW_QUERY_MULTIPLIER: usize = 3;

/// How many occurrences of the RAREST atom the window scan anchors on before it
/// gives up and answers the UNBOUNDED AND instead.
///
/// A SCAN-COST guard, not a tuning knob, and it is asked of the RAREST atom
/// ALONE — the one the scan anchors on, and the only one whose count bounds all
/// of them (the rarest overflowing means EVERY atom does). Cap every atom
/// instead and any query carrying an English word fires it (`the` turns up four
/// figures of times in an ordinary transcript) while the rarest atom still has a
/// handful of hits, handing a query the window could answer cheaply straight
/// back the unbounded AND the window exists to replace.
///
/// Anchored there, the number is sized by the right statistic: over the real
/// store the RAREST atom occurred p50=3, p90=11, p99=34 times per matching
/// session, with an observed maximum of 79 (a one-off probe, 2026-08-18 — a
/// dated sample, not a maintained metric), so ~6x that maximum clears any query
/// that names something specific.
///
/// TWO things overflow it, and the fallback is sound for both. A pathological
/// GENERATED file gets today's unbounded answer rather than a scan proportional
/// to its own pathology. And an all-common-word QUERY trips it over a perfectly
/// ORDINARY file — `the a` against ~100 KB of prose puts even the rarest atom in
/// four figures — which is why this is not exclusively a bad-file guard. That
/// case is harmless rather than a hole: atoms that frequent already sit within
/// one window of each other, so the unbounded answer admits nothing the window
/// would have rejected.
const PROXIMITY_MAX_ANCHORS: usize = 512;

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
        // Lowercase ONCE here (build/refresh), never per keystroke — and in a way
        // that keeps the lowercased haystack byte-for-byte addressable against the
        // cased one, because the proximity window measures DISTANCES across the
        // two. See [`lowercase_preserving_byte_len`].
        let name_lower = lowercase_preserving_byte_len(&session.label);
        let content_lower = lowercase_preserving_byte_len(&content);
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

/// One query atom compiled to a SIMD substring searcher, carrying **that atom's
/// own** smart-case decision.
///
/// `case_sensitive` is decided **PER ATOM**, never per query: an atom matches
/// case-sensitively iff it carries an uppercase char, so `foo BAR` searches
/// case-insensitively for `foo` and case-sensitively for `BAR` in the one query.
/// The flag rides the atom precisely so that stays true — a per-query decision
/// would look equivalent and silently break every mixed-case query.
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
    /// The uppercase test is the Unicode `Uppercase` property, which over ASCII —
    /// the realistic input space for a search box — is exactly an
    /// `is_ascii_uppercase` test. See the module docs for the narrow non-ASCII
    /// divergence from the oracle.
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
///   only the per-atom finders are rebuilt, the haystacks are untouched;
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
    /// The active query's atoms compiled to SIMD substring searchers, each
    /// carrying its own smart-case decision, rebuilt once per keystroke by
    /// [`set_query`](Self::set_query). This IS the search: [`results`](Self::results)
    /// reuses these across every entry so no per-entry needle setup or allocation
    /// happens during a scan, and BOTH marking rules — the row label's
    /// ([`match_indices`](Self::match_indices)) and the preview's
    /// ([`atom_match_positions`](Self::atom_match_positions)) — reuse the SAME
    /// finders, which is what makes a mark and an admission the same decision.
    atom_finders: Vec<AtomFinder>,
    /// The active query's proximity window in BYTES, derived once per keystroke
    /// by [`proximity_window`] and read by every entry in the scan. It depends on
    /// the QUERY alone, so computing it per entry would be the same number paid
    /// for hundreds of times.
    proximity_window: usize,
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
            // No atoms => the empty query: `results` returns all, and neither
            // marking rule marks anything.
            atom_finders: Vec::new(),
            proximity_window: proximity_window(0),
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
        // `self.query`, `self.mode`, and `self.atom_finders` are intentionally
        // left untouched: the query survives the refresh.
    }

    /// Set the active query, rebuilding the per-atom SIMD finders.
    ///
    /// Called once per keystroke. Only that one tiny structure is rebuilt — the
    /// prebuilt haystacks are never touched here.
    ///
    /// Matching is SUBSTRING, not fuzzy: memmem searches substrings by nature, so
    /// a query matches only a contiguous run of characters (literal, no scattered
    /// subsequence). [`gate_atoms`] does the only splitting — on unescaped
    /// whitespace, so `foo bar` requires both `foo` and `bar` — and never
    /// interprets any `'`/`^`/`$`/`!` atom syntax, so what matching means can
    /// never depend on what the user types. Smart case is then decided PER ATOM
    /// by [`AtomFinder::new`].
    pub fn set_query(&mut self, query: &str) {
        self.query.clear();
        self.query.push_str(query);
        // Compile the substring searchers ONCE per keystroke, reused across every
        // entry by the scan in `matching_candidates` and by both marking rules.
        self.atom_finders = gate_atoms(query)
            .into_iter()
            .filter(|atom| !atom.is_empty())
            .map(|atom| AtomFinder::new(&atom))
            .collect();
        // Derived from the QUERY, so it is computed once here rather than
        // recomputed identically for every entry in the scan.
        self.proximity_window = proximity_window(query.len());
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
        let window = self.proximity_window;

        candidates
            .iter()
            .copied()
            .filter(|&i| {
                // Fail-soft: skip any candidate index out of range (never panic).
                entries.get(i).is_some_and(|entry| match mode {
                    // A SIMD byte-substring scan per atom, over whichever haystack
                    // that atom's smart-case decision calls for. No UTF-32
                    // conversion, no allocation, no scoring: membership is the
                    // only bit anything downstream reads.
                    SearchMode::NameOnly => {
                        atoms_match(entry.haystack(mode), entry.gate_haystack(mode), finders)
                    }
                    // The same scan, but a MULTI-atom AND is bounded to a
                    // proximity window — a whole transcript is too wide a span for
                    // co-occurrence to mean anything. Name-only is deliberately
                    // untouched: its haystack is one short label already.
                    SearchMode::NameAndContent => content_mode_matches(entry, finders, window),
                })
            })
            .collect()
    }

    /// The CHAR indices within `display` that the active query matches — the
    /// ROW-LABEL highlight seam, under the WHOLE-STRING rule.
    ///
    /// Asked about the given DISPLAY string (a row's visible label)
    /// SPECIFICALLY — deliberately decoupled from the filtering haystack (which,
    /// in name+content mode, also spans `content_index`) — so a highlight only
    /// ever marks what is actually visible in the row. A content-only match
    /// therefore returns an empty set (the term is absent from the visible
    /// label), and the row shows no highlight, which is the intended behaviour.
    ///
    /// WHOLE-STRING: it answers only when EVERY atom occurs in `display`, and
    /// that predicate is the FILTER's own [`atoms_match`] — the same question,
    /// over the same [`AtomFinder`]s, that admitted the row. That rule is what a
    /// LABEL wants (one string, on the board because that string matched) and
    /// what a transcript LINE must not be asked — see
    /// [`atom_match_positions`](Self::atom_match_positions), which is the other
    /// RULE over this one mechanism.
    ///
    /// Positions come from the per-atom marking that seam already owns, so the
    /// label marks EVERY occurrence of every atom rather than one "best" run: a
    /// label reading `fix search, then fix preview` searched for `fix` marks
    /// both. That is the preview's marking rule, and reproducing it costs no new
    /// code.
    ///
    /// Returns CHAR indices into `display` (never byte offsets), sorted
    /// ascending and deduplicated. An empty/whitespace query parses to zero
    /// atoms and yields an empty set — nothing to highlight.
    pub fn match_indices(&self, display: &str) -> Vec<u32> {
        // No atoms => empty/whitespace query: nothing is highlighted.
        if self.atom_finders.is_empty() {
            return Vec::new();
        }
        // The WHOLE-STRING gate, asked of the filter's own predicate over the
        // pair of haystacks each atom's smart-case decision selects. One atom
        // missing from `display` means no highlight at all, even where the
        // others land.
        // Lowercased the way the FILTER lowercases its haystacks, not by
        // `to_lowercase`: the gate is only "the same question" if it folds the
        // same way the label arm that admitted the row does.
        if !atoms_match(
            display,
            &lowercase_preserving_byte_len(display),
            &self.atom_finders,
        ) {
            return Vec::new();
        }
        // Past the gate, WHICH chars is the per-atom question, and the preview's
        // seam already answers it in ascending, deduplicated CHAR positions.
        // `try_from` cannot realistically fail (a row label is display-sized);
        // dropping an out-of-range position is the fail-soft way to say so.
        self.atom_match_positions(display)
            .into_iter()
            .filter_map(|pos| u32::try_from(pos).ok())
            .collect()
    }

    /// The CHAR positions within `text` that the active query's atoms cover — the
    /// PER-ATOM rule, and the marking core both rules share.
    ///
    /// Marks **PER ATOM**, through the very [`AtomFinder`]s the FILTER answers
    /// membership with: every occurrence of every atom, wherever it falls, and the
    /// UNION of those runs is what the caller marks. Asked with NO whole-string
    /// precondition, which is what makes it right for a previewed transcript and
    /// is why [`match_indices`](Self::match_indices) applies that precondition
    /// itself before reusing this.
    ///
    /// Why a transcript LINE must not be asked the whole-string question — it
    /// would mark nothing whenever a two-word query's words sat on different lines
    /// — is argued once in the module docs ("ONE mechanism, TWO rules"). The short
    /// of it: marking per atom reproduces the FILTER's own admission rule, so a
    /// marked pane explains why the row is on the board.
    ///
    /// Smart case rides each atom exactly as it does in the filter — these ARE the
    /// filter's finders — so `foo BAR` marks `foo` case-insensitively and `BAR`
    /// case-sensitively inside one call, each against the haystack casing its own
    /// decision calls for. The case-INSENSITIVE branch folds `text` with
    /// [`lowercase_preserving_byte_len`], the SAME fold the filter builds its
    /// haystacks with, because sharing the finders is only half of asking the same
    /// question — the other half is folding the haystack the same way.
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
        // ONE byte -> CHAR map serves BOTH case branches, because the lowercased
        // sibling is folded by the same [`lowercase_preserving_byte_len`] the
        // FILTER builds its haystacks with. That fold is byte-length preserving per
        // char, so byte offset `n` names the same char in both strings and a second
        // map would be a copy of this one. Folding the way the filter folds is not
        // an optimization: a finder only answers the same question on both surfaces
        // if the haystack it is handed was lowercased by the same rule, or a row the
        // filter admitted draws with nothing marked.
        let starts = char_starts(text);
        let lowered = lowercase_preserving_byte_len(text);
        for atom in &self.atom_finders {
            let haystack = if atom.case_sensitive {
                text
            } else {
                lowered.as_str()
            };
            mark_atom(atom, haystack, &starts, &mut marked);
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

/// Split `query` into the substring atoms every rule below demands.
///
/// The ONE splitter, and now the rule itself rather than a mirror of anyone
/// else's: the filter, the label highlight
/// ([`match_indices`](SearchIndex::match_indices)) and the preview marks
/// ([`atom_match_positions`](SearchIndex::atom_match_positions)) all take their
/// atoms from the finders built here. Re-splitting a query anywhere else is how
/// two surfaces start disagreeing about which words the user typed.
///
/// Split on an ASCII space that is NOT escaped by an immediately preceding
/// backslash, drop the empty atoms, and unescape `\ ` → ` ` inside each atom (any
/// other backslash stays literal in the needle). Backslash-free queries — the
/// entire realistic input space for the search box — reduce to a plain space
/// split; the escape handling is what keeps a phrase typed with an escaped space
/// from being wrongly torn in two. It matches nucleo's `pattern_atoms` splitter,
/// which is what lets the parity oracle compare like for like.
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
                // atom is the phrase to look for, not two atoms.
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

/// Lowercase `text` PER CHAR, keeping any char whose lowercase form would change
/// its UTF-8 byte length.
///
/// The point is not the casing, it is the BYTE SPACE. A case-sensitive atom
/// searches the cased haystack and a case-insensitive one searches this
/// lowercased sibling, so the proximity window in [`atoms_co_occur_within`]
/// measures a distance between hits found in DIFFERENT strings. That distance is
/// meaningless unless byte offset `n` names the same place in both, which
/// [`str::to_lowercase`] does not guarantee: it can SHRINK a char (`K` U+212A →
/// `k`, 3 bytes → 1; `Å` U+212B → `å`, 3 → 2; `ẞ` U+1E9E → `ß`, 3 → 2) and it can
/// LENGTHEN one into several (`İ` → `i` + a combining dot). Either shifts every
/// later byte and turns a mixed-case query's window into garbage. Keeping such a
/// char as-is costs only the fold on that char and buys one shared byte space.
///
/// Folding per char rather than per string also drops [`str::to_lowercase`]'s one
/// context-sensitive rule — a Greek capital sigma lowercases to `ς` at a word end
/// and `σ` elsewhere, where per char it is always `σ` — and THAT is the narrowing
/// a real user can hit. Per STRING a label reading `ΟΔΟΣ` folded to `οδος`, the
/// natural lowercase spelling, so a Greek searcher typing `οδος` matched it; per
/// char the label folds to `οδοσ`, so `οδος` no longer matches and `οδοσ` does.
/// Always taking `σ` is not a claim about which spelling a searcher means — it is
/// the only choice that is context-FREE, and therefore the same on both sides of
/// the window.
///
/// The other narrowing is reachable only from a spelling nobody types: a query
/// `i̇stanbul` (`i` + U+0307) no longer matches a label reading `İstanbul`,
/// because `İ` lowercases to TWO chars and is therefore left alone. `stanbul`
/// still finds it. Both are the price of ONE byte space, and this is the only
/// place in the module that decides it.
fn lowercase_preserving_byte_len(text: &str) -> String {
    let mut lowered = String::with_capacity(text.len());
    for ch in text.chars() {
        let mut folded = ch.to_lowercase();
        match (folded.next(), folded.next()) {
            // Exactly one char AND the same encoded width: safe to substitute.
            (Some(one), None) if one.len_utf8() == ch.len_utf8() => lowered.push(one),
            // Anything else would move every byte after it. Keep the original.
            _ => lowered.push(ch),
        }
    }
    lowered
}

/// The proximity window, in BYTES, for a query of `query_len` bytes.
///
/// `max(floor, multiplier × query length)`: short typed queries sit on the floor,
/// and a long query — a pasted snippet — earns a window proportional to the span
/// of text it is describing. See the two constants for the measurements behind
/// each number.
fn proximity_window(query_len: usize) -> usize {
    PROXIMITY_WINDOW_MIN_BYTES.max(query_len.saturating_mul(PROXIMITY_WINDOW_QUERY_MULTIPLIER))
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

/// Membership in [`SearchMode::NameAndContent`]: every atom in the LABEL, or
/// every atom co-occurring inside one `window`-byte neighbourhood of the content
/// haystack.
///
/// FAST PATH: a query of ONE atom takes exactly [`atoms_match`], the pre-window
/// path, byte for byte. A single atom cannot be far from itself, so the window
/// has nothing to say about it — and the single-atom query is the most common
/// keystroke there is, which must not get slower.
///
/// BOTH arms are load-bearing. The label arm is not implied by the window arm: a
/// label is capped at `store::label::LABEL_MAX` CHARS, which can exceed the
/// window in BYTES, so without it a row could stop being findable by its own
/// name.
///
/// The window arm runs over the COMBINED haystack (label at byte 0, a newline,
/// then the content index) rather than the content index alone, so a pair
/// STRADDLING that seam co-occurs. Say it out loud, because it admits rows
/// neither literal arm does: one atom in the label and one in the transcript's
/// opening lines is a match. That is the intent — the label IS the transcript's
/// opening words, so a hit spanning the join is near in the sense the window
/// means — and it makes this arm a strict superset of the label arm rather than
/// an alternative to it.
fn content_mode_matches(entry: &Entry, finders: &[AtomFinder], window: usize) -> bool {
    let mode = SearchMode::NameAndContent;
    if finders.len() < 2 {
        return atoms_match(entry.haystack(mode), entry.gate_haystack(mode), finders);
    }
    atoms_match(&entry.name, &entry.name_lower, finders)
        || atoms_co_occur_within(
            entry.haystack(mode),
            entry.gate_haystack(mode),
            finders,
            window,
        )
}

/// True iff some occurrence of EVERY atom sits within `window` bytes of one
/// shared anchor — the bounded replacement for an AND evaluated over a whole
/// transcript.
///
/// Distance is measured START to START, in either direction, so the admitted
/// neighbourhood spans at most twice `window`. Why bounding it at all is the
/// whole point of this change is argued in the module docs; the short of it is
/// that two words co-occurring somewhere in a 12-75 KB transcript matched a
/// median 44.2% of the real corpus and told the user nothing.
///
/// Shape: count each atom's occurrences over the PREBUILT haystack (a full pass
/// per atom, which is what today's AND already costs when an atom is absent),
/// anchor on the RAREST atom, and answer the others from a BOUNDED slice around
/// each of its occurrences. A missing atom fails outright — no window can rescue
/// a word that is not there — and that is the common bail.
///
/// [`PROXIMITY_MAX_ANCHORS`] then bounds the anchor's own occurrence count, and
/// the ANCHOR's alone — see that const for why asking it of a common atom would
/// re-open the defect this whole path exists to close.
///
/// The two haystacks MUST be the same text differing only in case AND sharing one
/// byte space; [`lowercase_preserving_byte_len`] is what guarantees the second
/// half, without which every distance computed here is garbage. The
/// `debug_assert` states it, and the `lo < hi` slice guard keeps a future
/// violation to a wrong answer rather than a panic.
fn atoms_co_occur_within(
    cased: &str,
    lowercased: &str,
    finders: &[AtomFinder],
    window: usize,
) -> bool {
    debug_assert_eq!(
        cased.len(),
        lowercased.len(),
        "the cased and lowercased haystacks must share ONE byte space, or every \
         distance measured across them is meaningless"
    );
    let haystack_for = |atom: &AtomFinder| -> &[u8] {
        if atom.case_sensitive {
            cased.as_bytes()
        } else {
            lowercased.as_bytes()
        }
    };

    // Occurrences per atom, each list bounded so no atom can make the scan pay
    // for its own frequency. Collecting one PAST the cap is how an atom that
    // overflows stays distinguishable from one that exactly fills it — and the
    // bound is also why finding the rarest below never materialises a huge list.
    let mut occurrences: Vec<Vec<usize>> = Vec::with_capacity(finders.len());
    for atom in finders {
        let hits: Vec<usize> = atom
            .finder
            .find_iter(haystack_for(atom))
            .take(PROXIMITY_MAX_ANCHORS + 1)
            .collect();
        if hits.is_empty() {
            return false;
        }
        occurrences.push(hits);
    }

    // The rarest atom makes the fewest anchors, hence the fewest bounded slices.
    let anchor = occurrences
        .iter()
        .enumerate()
        .min_by_key(|(_, hits)| hits.len())
        .map_or(0, |(index, _)| index);

    // Scan-cost guard, asked of the ANCHOR alone. The anchor is by construction
    // the RAREST atom, so its overflowing means EVERY atom does. Asking it of
    // every atom instead would fire on any query carrying an ordinary word (`the`
    // occurs four figures of times in a real transcript) while the rarest still
    // had a handful of hits, and hand that query back the unbounded AND this
    // exists to replace. Two things reach it: a pathological GENERATED file, and
    // an all-common-word QUERY over an ORDINARY one - the latter harmlessly,
    // since atoms that frequent co-occur inside the window anyway.
    // Capping the others would buy nothing regardless: the scan costs
    // (anchor hits x atoms x window), and their own lists are already bounded
    // above.
    if occurrences[anchor].len() > PROXIMITY_MAX_ANCHORS {
        return true;
    }

    occurrences[anchor].iter().any(|&at| {
        finders.iter().enumerate().all(|(index, atom)| {
            if index == anchor {
                return true;
            }
            let haystack = haystack_for(atom);
            let lo = at.saturating_sub(window).min(haystack.len());
            // The slice runs one needle PAST the furthest allowed start, so an
            // occurrence beginning exactly at the edge still fits inside it whole.
            let hi = at
                .saturating_add(window)
                .saturating_add(atom.finder.needle().len())
                .min(haystack.len());
            lo < hi && atom.finder.find(&haystack[lo..hi]).is_some()
        })
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
///
/// ONE map answers for the cased text AND its lowercased sibling. That is a
/// consequence of [`lowercase_preserving_byte_len`], not an assumption:
/// the fold substitutes a char only when the replacement encodes to the SAME
/// number of bytes, so byte offset `n` names the same char in both strings. A
/// per-string [`str::to_lowercase`] would not license this — it can lengthen a
/// char (`İ` → `i` + a combining dot) and shift every later byte — which is
/// exactly why the marking seam folds the way the filter does.
fn char_starts(text: &str) -> Vec<usize> {
    text.char_indices().map(|(byte, _)| byte).collect()
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

    // The ONLY nucleo import left in the crate, and it is test-scoped: nucleo is
    // a dev-dependency backing [`nucleo_match_set`], the membership ORACLE. The
    // runtime below never touches it.
    use nucleo::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
    use nucleo::{Config, Matcher, Utf32Str};

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
    /// Asking nucleo cannot be wrong about nucleo. This is the ONLY reason nucleo
    /// is still a (dev-)dependency at all.
    ///
    /// The deliberate divergences are NOT covered by this oracle and must be kept
    /// out of any corpus compared against it — see the module docs.
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
        // — a naive byte-offset approach would report [1, 5].
        index.set_query("🚀b");
        assert_eq!(
            index.match_indices("a🚀bc"),
            vec![1, 2],
            "the run spans a multi-byte char and stays on char indices"
        );
    }

    /// EVERY occurrence of an atom is highlighted in the label, not just one.
    ///
    /// The label seam marks through the same per-atom machinery the preview does
    /// (pinned at
    /// [`atom_match_positions_marks_every_occurrence_including_overlaps`]), so a
    /// word said twice in a label is emphasized twice. Every other
    /// `match_indices` test uses a single-occurrence label, so this is the one
    /// that would catch a seam that reported only its "best" hit.
    #[test]
    fn match_indices_marks_every_occurrence_of_an_atom() {
        let mut index = SearchIndex::new();
        index.set_query("fix");

        // "fix search, then fix preview": `fix` at chars 0..3 AND 17..20.
        assert_eq!(
            index.match_indices("fix search, then fix preview"),
            vec![0, 1, 2, 17, 18, 19],
            "both occurrences of the atom must be marked, not just one"
        );

        // Per atom, per occurrence: each atom of a multi-atom query marks every
        // place IT lands, and the union comes out sorted and deduplicated.
        index.set_query("fix run");
        assert_eq!(
            index.match_indices("fix run, fix run"),
            vec![0, 1, 2, 4, 5, 6, 9, 10, 11, 13, 14, 15],
            "every occurrence of every atom is marked"
        );

        // Overlapping occurrences merge into one span rather than leaving the
        // tail char plain — the same union rule the preview marks by.
        index.set_query("aa");
        assert_eq!(
            index.match_indices("aaa"),
            vec![0, 1, 2],
            "overlapping occurrences merge into one marked span"
        );
    }

    /// A tail-anchored match in a NON-ASCII label is highlighted.
    ///
    /// The filter has always found this (`memmem` has no tail off-by-one); the
    /// highlight used to miss it, so the row drew unhighlighted. Both sides now
    /// answer from the same memmem finders, so filter and highlight agree and
    /// there is no tail case left to except.
    #[test]
    fn match_indices_marks_a_tail_anchored_non_ascii_match() {
        let sessions = [session("s0", "café fin", "")];
        assert_eq!(
            filter("fin", &sessions, SearchMode::NameOnly),
            vec![0],
            "the filter finds a tail-anchored match in a non-ASCII haystack"
        );

        let mut index = SearchIndex::new();
        index.set_query("fin");
        // "café fin": c(0) a(1) f(2) é(3) ' '(4) f(5) i(6) n(7) — `é` is two
        // BYTES but one char, and `fin` ends at the very last char.
        assert_eq!(
            index.match_indices("café fin"),
            vec![5, 6, 7],
            "the row the filter admitted must draw highlighted, tail or not"
        );
    }

    /// The row-LABEL seam and the FILTER must fold the same way, or a row the
    /// filter admitted draws with nothing marked (and one it rejected draws
    /// marked).
    ///
    /// Both surfaces read the same finders, but a finder only answers the same
    /// question if the haystack it is handed was lowercased by the same rule. The
    /// filter folds PER CHAR ([`lowercase_preserving_byte_len`]); marking must
    /// too. The cases below are exactly where a per-STRING [`str::to_lowercase`]
    /// parts company with it: the context-sensitive word-final sigma, and the
    /// three chars whose lowercase is SHORTER than themselves.
    #[test]
    fn the_label_seam_folds_the_way_the_filter_folds() {
        // (label, query, whether the FILTER admits the row on its label)
        let cases = [
            // Word-final sigma: per STRING `Σ` closing a word lowercases to `ς`,
            // per CHAR always to `σ`. The filter admits `οδοσ`, so the label must
            // draw marked.
            ("ΟΔΟΣ notes", "οδοσ", true),
            // ...and the per-STRING spelling is the one the filter rejects, so it
            // must draw plain.
            ("ΟΔΟΣ notes", "οδος", false),
            // `K` U+212A lowercases to a SHORTER `k`, so the fold keeps it and the
            // filter never admits the ASCII spelling.
            ("\u{212A}elvin scale", "kelvin", false),
            // `Å` U+212B → `å` (3 bytes → 2): kept, so likewise.
            ("\u{212B}ngstrom unit", "\u{e5}ngstrom", false),
            // `ẞ` U+1E9E → `ß` (3 bytes → 2): kept, so likewise.
            ("\u{1E9E}trasse map", "\u{df}trasse", false),
        ];

        for (label, query, admitted) in cases {
            let sessions = [session("s0", label, "")];
            assert_eq!(
                !filter(query, &sessions, SearchMode::NameOnly).is_empty(),
                admitted,
                "the filter's verdict on {query:?} over {label:?} is the premise here"
            );

            let mut index = SearchIndex::new();
            index.set_query(query);
            assert_eq!(
                !index.match_indices(label).is_empty(),
                admitted,
                "the row-label highlight must agree with the filter on {query:?} \
                 over {label:?}"
            );
            assert_eq!(
                !index.atom_match_positions(label).is_empty(),
                admitted,
                "the per-atom marking beneath it must agree too — the \
                 match-outside-preview nudge reads this one directly"
            );
        }
    }

    /// Accent folding is not part of the highlight: the label seam matches the
    /// same BYTES the filter does.
    ///
    /// A row whose label reads `résumé` can only be on the board under a
    /// `resume` query because its CONTENT matched, and a content-only hit shows
    /// no label highlight by design. Folding here would have marked the label of
    /// a row the filter never admitted on its label at all.
    #[test]
    fn match_indices_does_not_accent_fold() {
        let mut index = SearchIndex::new();
        index.set_query("resume");
        assert!(
            index.match_indices("résumé notes").is_empty(),
            "the highlight matches bytes, so an accent-folded near-miss marks nothing"
        );
        // The unaccented spelling still marks, so the assertion above is not
        // vacuously true over a query that matches nothing anywhere.
        assert_eq!(
            index.match_indices("resume notes"),
            vec![0, 1, 2, 3, 4, 5],
            "the literal spelling still highlights"
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

        // A match at the very LAST char of a non-ASCII string is found: a byte
        // scan has no tail off-by-one to work around.
        index.set_query("fin");
        assert_eq!(index.atom_match_positions("café fin"), vec![5, 6, 7]);
    }

    /// A char whose lowercase form is LONGER than itself does not shift the marks.
    ///
    /// `İ` (U+0130) lowercases to TWO chars (`i` + a combining dot), which under
    /// [`str::to_lowercase`] would move every byte after it and mark the wrong
    /// chars — here two positions past the word and off the end of the string.
    /// [`lowercase_preserving_byte_len`] KEEPS such a char instead, so one
    /// byte -> char map serves both branches; this pins that the marks land where
    /// the source text says, not where a shifted fold would put them.
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

    /// THE CHANGE: a multi-atom content AND is bounded to a proximity window.
    ///
    /// Two atoms 50 KB apart in one transcript are a coincidence, not a match;
    /// the same two atoms a paragraph apart are the exchange the user remembers.
    /// Unbounded — the previous rule — the first case matched too, which is the
    /// shape that put a random two-word query at a median 44.2% of the real
    /// corpus.
    #[test]
    fn content_atoms_must_co_occur_within_the_window() {
        let far = [session(
            "far",
            "unrelated title",
            &format!("alpha{}omega", "x".repeat(50_000)),
        )];
        let near = [session(
            "near",
            "unrelated title",
            &format!("alpha{}omega", "x".repeat(100)),
        )];

        // Both atoms ARE present in the far session, so what excludes it below is
        // the DISTANCE between them and not a missing word. Without this the
        // assertion could pass over a corpus that never contained the terms.
        assert_eq!(filter("alpha", &far, SearchMode::NameAndContent), vec![0]);
        assert_eq!(filter("omega", &far, SearchMode::NameAndContent), vec![0]);

        assert!(
            filter("alpha omega", &far, SearchMode::NameAndContent).is_empty(),
            "atoms 50 KB apart in one transcript must not match"
        );
        assert_eq!(
            filter("alpha omega", &near, SearchMode::NameAndContent),
            vec![0],
            "the same atoms ~100 bytes apart co-occur and must match"
        );
        // The anchor is the RAREST atom, not the first one typed, so reversing
        // the query cannot change the answer.
        assert_eq!(
            filter("omega alpha", &near, SearchMode::NameAndContent),
            vec![0],
            "atom order must not decide membership"
        );
    }

    /// A SINGLE-atom query takes the pre-window path exactly: one atom cannot be
    /// far from itself, and this is the most common keystroke there is.
    #[test]
    fn a_single_atom_query_is_unaffected_by_the_window() {
        // The very span the two-atom query above is excluded over.
        let sessions = [session(
            "s0",
            "unrelated title",
            &format!("alpha{}omega", "x".repeat(50_000)),
        )];
        for query in ["alpha", "omega"] {
            assert_eq!(
                filter(query, &sessions, SearchMode::NameAndContent),
                vec![0],
                "{query:?} is one atom and must match at any depth"
            );
        }
        // An escaped space is ONE atom too (the literal phrase), so it takes the
        // same unwindowed path rather than being torn into a windowed pair.
        let phrase = [session("s1", "unrelated title", "ran the deploy pipeline")];
        assert_eq!(
            filter(r"deploy\ pipeline", &phrase, SearchMode::NameAndContent),
            vec![0],
            "an escaped-space phrase is a single atom, not a windowed AND"
        );
    }

    /// The LABEL arm is a separate arm, not a consequence of the window: a label
    /// whose atoms are further apart in BYTES than the window still matches.
    ///
    /// `store::label::LABEL_MAX` caps a label at 180 CHARS, which is up to 720
    /// BYTES, so "a label always fits inside one window" is false and cannot be
    /// relied on. This label is 162 chars but 312 bytes.
    #[test]
    fn a_label_hit_matches_even_when_the_label_outruns_the_window() {
        let label = format!("alpha {} omega", "é".repeat(150));
        assert!(
            label.chars().count() <= 180,
            "the fixture must stay a realistic label under LABEL_MAX"
        );
        assert!(
            label.len() > PROXIMITY_WINDOW_MIN_BYTES,
            "...while outrunning the window in BYTES, or it pins nothing"
        );

        let sessions = [session(
            "s0",
            &label,
            &format!("noise{}noise", "x".repeat(50_000)),
        )];
        assert_eq!(
            filter("alpha omega", &sessions, SearchMode::NameAndContent),
            vec![0],
            "a row must stay findable by its own name in content mode"
        );
    }

    /// [`SearchMode::NameOnly`] is untouched: no window, and content is still
    /// never consulted.
    ///
    /// The oversized label is deliberate — a production label is capped — so the
    /// assertion is about the RULE rather than about labels happening to be
    /// shorter than the window.
    #[test]
    fn name_only_mode_ignores_the_proximity_window() {
        let oversized = [session(
            "s0",
            &format!("alpha{}omega", "x".repeat(50_000)),
            "",
        )];
        assert_eq!(
            filter("alpha omega", &oversized, SearchMode::NameOnly),
            vec![0],
            "name-only keeps the UNBOUNDED AND over its label"
        );

        let content_only = [session("s1", "unrelated", "alpha omega, adjacent here")];
        assert!(
            filter("alpha omega", &content_only, SearchMode::NameOnly).is_empty(),
            "name-only still never searches content, windowed or not"
        );
    }

    /// Once the RAREST atom passes [`PROXIMITY_MAX_ANCHORS`] occurrences the scan
    /// abandons the window and answers the UNBOUNDED AND — a pathological-FILE
    /// guard, so a generated file degrades to the previous behaviour instead of
    /// costing a scan proportional to its own pathology.
    ///
    /// The rarest atom is the one that must trip it, and both atoms here are
    /// equally common precisely so it does. A file where the rarest of the typed
    /// words still turns up 500+ times is not a transcript anyone wrote; a file
    /// where only the COMMON word does is every transcript, which is why
    /// [`a_common_partner_atom_does_not_lift_the_window`] pins that case instead.
    #[test]
    fn a_runaway_occurrence_count_falls_back_to_the_unbounded_and() {
        let gap = "x".repeat(50_000);

        // Just UNDER the cap on BOTH atoms, so the rarest is under it too: the
        // window still decides, and it excludes a 50 KB separation.
        let modest = format!(
            "{}{gap}{}",
            "alpha ".repeat(PROXIMITY_MAX_ANCHORS - 10),
            "omega ".repeat(PROXIMITY_MAX_ANCHORS - 10)
        );
        let bounded = [session("s0", "unrelated title", &modest)];
        assert!(
            filter("alpha omega", &bounded, SearchMode::NameAndContent).is_empty(),
            "under the anchor cap the window still excludes a 50 KB separation"
        );

        // Past it on both — hence past it on the rarest — the same shape is
        // admitted rather than scanned.
        let flood = format!(
            "{}{gap}{}",
            "alpha ".repeat(PROXIMITY_MAX_ANCHORS + 10),
            "omega ".repeat(PROXIMITY_MAX_ANCHORS + 10)
        );
        let runaway = [session("s1", "unrelated title", &flood)];
        assert_eq!(
            filter("alpha omega", &runaway, SearchMode::NameAndContent),
            vec![0],
            "past the anchor cap the window is abandoned for the unbounded answer"
        );
    }

    /// Only the ANCHOR atom's occurrence count may trip the cap. A COMMON partner
    /// word must not buy a rare one a free pass across a whole transcript.
    ///
    /// This is the defect the whole change exists to close, re-entered through a
    /// side door: an ordinary English word turns up four figures of times in a
    /// real transcript, so capping EVERY atom means any query carrying one falls
    /// back to the unbounded AND. The rarest atom is the one the scan anchors on
    /// and the only one whose count says anything about the FILE being
    /// pathological.
    #[test]
    fn a_common_partner_atom_does_not_lift_the_window() {
        let common = "the ".repeat(PROXIMITY_MAX_ANCHORS + 100);
        assert!(
            common.matches("the").count() > PROXIMITY_MAX_ANCHORS,
            "the common atom must outrun the cap, or this pins nothing"
        );

        let far = [session(
            "far",
            "unrelated title",
            &format!("{common}{}frobnicate", "x".repeat(50_000)),
        )];
        // Both atoms ARE present, so what must exclude the row is the DISTANCE
        // between them and not a missing word.
        assert_eq!(filter("the", &far, SearchMode::NameAndContent), vec![0]);
        assert_eq!(
            filter("frobnicate", &far, SearchMode::NameAndContent),
            vec![0]
        );
        assert!(
            filter("the frobnicate", &far, SearchMode::NameAndContent).is_empty(),
            "a common partner atom must not lift the window off a 50 KB separation"
        );

        // The same pair still matches where it genuinely co-occurs, so the
        // assertion above is not "common atoms never match".
        let near = [session(
            "near",
            "unrelated title",
            &format!("{common}frobnicate"),
        )];
        assert_eq!(
            filter("the frobnicate", &near, SearchMode::NameAndContent),
            vec![0],
            "the common atom next to the rare one still co-occurs"
        );
    }

    /// The window is a FLOOR with a proportional escape hatch: a typed query sits
    /// on the floor, a long (pasted) one earns room to spread out.
    #[test]
    fn the_proximity_window_is_a_floor_that_grows_with_the_query() {
        assert_eq!(proximity_window(0), PROXIMITY_WINDOW_MIN_BYTES);
        assert_eq!(
            proximity_window("alpha omega".len()),
            PROXIMITY_WINDOW_MIN_BYTES,
            "a typed two-word query sits on the floor"
        );
        // Past floor/multiplier the query's own length sets the window.
        assert_eq!(
            proximity_window(400),
            400 * PROXIMITY_WINDOW_QUERY_MULTIPLIER
        );
        // An absurd length saturates rather than overflowing.
        assert!(proximity_window(usize::MAX) >= PROXIMITY_WINDOW_MIN_BYTES);

        // End to end, over ONE corpus: a longer query buys its atoms the right to
        // sit further apart. This is what carries a pasted snippet
        // (`tui::update::flatten_for_query`), whose words legitimately span more
        // text than a typed pair's.
        let gap = PROXIMITY_WINDOW_MIN_BYTES + 20;
        let tail = "omega reconciliation instrumentation orchestration normalization \
                    deduplication serialization backpressure idempotency";
        let sessions = [session(
            "s0",
            "unrelated title",
            &format!("alpha{}{tail}", "x".repeat(gap)),
        )];

        let short = "alpha omega";
        assert!(
            proximity_window(short.len()) < gap,
            "the short query must sit on the floor, below the gap"
        );
        assert!(
            filter(short, &sessions, SearchMode::NameAndContent).is_empty(),
            "a short query's window does not reach across the gap"
        );

        let long = format!("alpha {tail}");
        assert!(
            proximity_window(long.len()) > gap,
            "the long query must earn a wider window, or it pins nothing"
        );
        assert_eq!(
            filter(&long, &sessions, SearchMode::NameAndContent),
            vec![0],
            "a long query's window grows to cover the span it describes"
        );
    }

    /// THE OFFSET TRAP: a MIXED-case query measures its window across TWO
    /// haystacks, so the two must share ONE byte space.
    ///
    /// A case-SENSITIVE atom searches the cased haystack while a
    /// case-INSENSITIVE one searches the lowercased sibling. `str::to_lowercase`
    /// SHRINKS `K` U+212A (3 bytes → 1), `Å` U+212B (3 → 2) and `ẞ` U+1E9E
    /// (3 → 2), so under it every byte past such a char sits at a different offset
    /// in the two strings and the distance between the two hits is garbage. The
    /// prefix here shifts by more than a whole window, so a naive fold reports two
    /// ADJACENT words as far apart and drops the row.
    #[test]
    fn the_proximity_window_survives_a_length_changing_lowercase() {
        let shrinking: String = ['\u{212A}', '\u{212B}', '\u{1E9E}']
            .into_iter()
            .cycle()
            .take(300)
            .collect();
        assert!(
            shrinking.len() - shrinking.to_lowercase().len() > PROXIMITY_WINDOW_MIN_BYTES,
            "the fixture must shift offsets by MORE than one window, or it pins nothing"
        );

        let sessions = [session(
            "s0",
            "unrelated title",
            &format!("{shrinking} ALPHA beta"),
        )];
        // `ALPHA` carries an uppercase char (case-SENSITIVE, cased haystack);
        // `beta` does not (case-INSENSITIVE, lowercased haystack). In the text
        // they are six bytes apart.
        assert_eq!(
            filter("ALPHA beta", &sessions, SearchMode::NameAndContent),
            vec![0],
            "adjacent words must stay adjacent across the cased/lowercased pair"
        );
    }

    /// The three measured counter-examples to "same char count means same byte
    /// length": each lowercases to exactly ONE char that is SHORTER, so a char
    /// count alone cannot catch them and each must be kept as-is.
    #[test]
    fn the_lowercase_fold_keeps_chars_whose_lowercase_changes_byte_length() {
        for ch in ['\u{212A}', '\u{212B}', '\u{1E9E}'] {
            let cased = ch.to_string();
            let lowered: String = ch.to_lowercase().collect();
            assert_eq!(
                lowered.chars().count(),
                1,
                "{ch:?} lowercases to ONE char, so a char count cannot catch it"
            );
            assert!(
                lowered.len() < cased.len(),
                "...but a SHORTER one ({} -> {} bytes)",
                cased.len(),
                lowered.len()
            );
            assert_eq!(
                lowercase_preserving_byte_len(&cased),
                cased,
                "{ch:?} must be kept as-is; folding it would shift every later byte"
            );
        }

        // Positive control: the fold is not simply "keep everything". ASCII folds,
        // and so does a 2-byte char whose lowercase is also 2 bytes.
        assert_eq!(lowercase_preserving_byte_len("ABC"), "abc");
        // Folding PER CHAR also drops `to_lowercase`'s one context-sensitive rule:
        // a Greek capital sigma CLOSING a word lowercases to the final form `ς`
        // per string, and to the plain `σ` per char. Both are 2 bytes, so this
        // costs the byte space nothing — it is simply the more predictable of the
        // two for someone typing `σ` into a search box.
        assert_eq!(
            "ΟΣ".to_lowercase(),
            "ος",
            "per STRING, a word-final sigma is `ς`"
        );
        assert_eq!(
            lowercase_preserving_byte_len("ΟΣ"),
            "οσ",
            "per CHAR it is always `σ`"
        );

        // The invariant the whole proximity window rests on.
        let mixed = "Mixed \u{212A}\u{212B}\u{1E9E} Σ İ Text";
        assert_eq!(
            lowercase_preserving_byte_len(mixed).len(),
            mixed.len(),
            "the lowercased haystack must share the cased one's byte length"
        );
    }

    /// The narrow BEHAVIOUR CHANGE the length-preserving fold introduces, pinned
    /// so it stays a decision rather than a surprise.
    ///
    /// `İ` (U+0130) lowercases to TWO chars (`i` + a combining dot), so the fold
    /// keeps it — and a query spelled with that two-char lowercase stops matching
    /// a label spelled with `İ`. The plain substring still finds the row.
    #[test]
    fn a_two_char_lowercase_no_longer_folds_onto_its_uppercase() {
        let sessions = [session("s0", "İstanbul notes", "")];
        for mode in [SearchMode::NameOnly, SearchMode::NameAndContent] {
            assert!(
                filter("i\u{307}stanbul", &sessions, mode).is_empty(),
                "the two-char lowercase spelling no longer folds onto İ in {mode:?}"
            );
            assert_eq!(
                filter("stanbul", &sessions, mode),
                vec![0],
                "the row stays findable by the substring that avoids the kept char"
            );
        }
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
    ///
    /// nucleo's AND is UNBOUNDED, so what this pins is parity WITHIN the
    /// proximity window: every corpus string sits far under
    /// [`PROXIMITY_WINDOW_MIN_BYTES`], which is what keeps the two comparable.
    /// Widening one past that bound would make the oracle disagree BY DESIGN —
    /// the disagreement is the bounded rule working — so the bound is pinned by
    /// [`content_atoms_must_co_occur_within_the_window`] and its neighbours
    /// instead. Do not shrink this corpus or drop a mode to keep it green.
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
