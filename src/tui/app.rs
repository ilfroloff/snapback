//! The `App` model.
//!
//! Holds all TUI state: `sessions`, `filtered` indices, `selected` session id,
//! `scroll` offset, `query`, `search_mode` (name | name+content), `scope`
//! (current-folder | all), and the preview cache. Selection is tracked by
//! stable `session_id` (not list index) so it survives autorefresh.
//!
//! Everything in this module is pure state manipulation with no terminal I/O,
//! so it is unit-testable without a real terminal. The terminal-driving loop
//! lives in [`crate::tui`] (run) and the key/event dispatch in
//! [`crate::tui::update`].

use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ratatui::layout::Rect;
use ratatui::text::{Line, Text};
use time::OffsetDateTime;

use crate::agents::LiveAgent;
use crate::search::{SearchIndex, SearchMode};
use crate::store::{preview, Session};

/// Lines the preview scrolls per mouse-wheel notch. Small (the terminal reports
/// discrete notches, not pixel deltas) so a trackpad flick stays controllable.
const PREVIEW_WHEEL_STEP: i32 = 2;

/// Rows the list selection moves per mouse-wheel notch.
const LIST_WHEEL_STEP: isize = 1;

/// Minimum columns either pane keeps when the list/preview splitter is
/// dragged, so neither pane can be crushed to zero width or dragged past the
/// other (which would invert the layout).
pub const MIN_PANE_WIDTH: u16 = 15;

/// The list pane's share of the body width before the user has ever dragged
/// the splitter — matches the historical `Constraint::Percentage(48)` split
/// this feature replaces.
const DEFAULT_LIST_PERCENT: u32 = 48;

/// Which set of sessions the list shows.
///
/// The default is [`Scope::CurrentFolder`]: only sessions whose canonicalized
/// `cwd` equals the canonicalized launch directory. [`Scope::All`] shows every
/// session (still grouped by folder). Toggled by a keybinding and by the
/// `--all`/`-a` CLI flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Only sessions launched from the current working directory (exact
    /// canonical `cwd` match).
    CurrentFolder,
    /// Every session, grouped by folder.
    All,
}

impl Scope {
    /// Flip between the two scopes.
    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            Scope::CurrentFolder => Scope::All,
            Scope::All => Scope::CurrentFolder,
        }
    }
}

/// A rendered row in the grouped list: either a group head (shown ONCE per
/// repo->branch group) or a session row addressing `sessions[index]`.
///
/// [`build_rows`] guarantees rows of the same group are contiguous and each
/// group emits exactly one head, so the git-log-style folder head appears once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// A group head for a repo->branch group.
    Group {
        /// The repo grouping label.
        repo: String,
        /// The branch label (`(detached)` when absent).
        branch: String,
    },
    /// A session row; the payload indexes into [`App::sessions`].
    Session(usize),
}

/// One of the three routes offered when `Enter` lands on a LIVE session — one
/// that `claude -r` would refuse to plain-resume.
///
/// Modeled as explicit state (rather than an ad-hoc branch) so the running-session
/// choice is a small, unit-testable state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveChoice {
    /// Reattach to the running session by id (`claude attach <id>`); fail-soft
    /// and funnelled through the shared `launch` round trip.
    Attach,
    /// Fork the session (`claude -r <id> --fork-session`).
    Fork,
    /// Dismiss the overlay and stay on the board.
    Cancel,
}

impl LiveChoice {
    /// The choices in overlay order (used for cycling the highlight).
    pub const ORDER: [LiveChoice; 3] = [LiveChoice::Attach, LiveChoice::Fork, LiveChoice::Cancel];

    /// Position of this choice within [`ORDER`](Self::ORDER).
    fn index(self) -> usize {
        match self {
            LiveChoice::Attach => 0,
            LiveChoice::Fork => 1,
            LiveChoice::Cancel => 2,
        }
    }

    /// The button label shown in the overlay.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            LiveChoice::Attach => "Attach",
            LiveChoice::Fork => "Fork",
            LiveChoice::Cancel => "Cancel",
        }
    }
}

/// The open running-session choice overlay: which session it targets and which
/// option is currently highlighted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingLive {
    /// Stable `session_id` of the live session the choice acts on.
    pub session_id: String,
    /// The currently highlighted choice.
    pub selected: LiveChoice,
}

/// Canonicalize `p`, falling back to the raw path when it cannot be resolved
/// (e.g. a session whose worktree was deleted). Used by the exact-cwd scope
/// predicate so a launch dir and a session `cwd` are compared in the same
/// resolved form (symlinks, `.`/`..`, and `/tmp`->`/private/tmp` collapsed).
#[must_use]
pub fn resolve_dir(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// The folder-scoping predicate (Task 5.4): is `session` in `scope`?
///
/// [`Scope::All`] always matches. [`Scope::CurrentFolder`] matches only when the
/// session's resolved `cwd` is byte-equal to `launch` — an EXACT canonical match
/// (design decision: precise, a repo's other worktree folders do not appear).
/// `launch` MUST already be resolved via [`resolve_dir`].
#[must_use]
pub fn in_scope(scope: Scope, session: &Session, launch: &Path) -> bool {
    match scope {
        Scope::All => true,
        Scope::CurrentFolder => resolve_dir(&session.cwd) == launch,
    }
}

/// Flatten `filtered` (indices into `sessions`, in scope-aware display order)
/// into rows for the list.
///
/// In [`Scope::All`] this emits a group head the first time a repo->branch group
/// appears, then that group's session rows. Because `filtered` is kept in
/// display order (group-most-recent-desc, then timestamp-desc within a group)
/// same-group rows are contiguous, so each group yields exactly ONE head. In
/// [`Scope::CurrentFolder`] group heads are suppressed entirely and the result
/// is a flat, timestamp-desc list of session rows.
#[must_use]
pub fn build_rows(sessions: &[Session], filtered: &[usize], scope: Scope) -> Vec<Row> {
    let mut rows = Vec::with_capacity(filtered.len() + 8);
    // Current-folder scope is a flat, head-less list.
    if scope == Scope::CurrentFolder {
        rows.extend(filtered.iter().map(|&i| Row::Session(i)));
        return rows;
    }
    let mut current: Option<(String, String)> = None;
    for &i in filtered {
        let session = &sessions[i];
        let key = (session.repo.clone(), session.branch_display().to_string());
        if current.as_ref() != Some(&key) {
            rows.push(Row::Group {
                repo: key.0.clone(),
                branch: key.1.clone(),
            });
            current = Some(key);
        }
        rows.push(Row::Session(i));
    }
    rows
}

/// Clamp a requested list-pane width into `[MIN_PANE_WIDTH, body_width -
/// MIN_PANE_WIDTH]`, so the preview pane always keeps at least
/// `MIN_PANE_WIDTH` columns too. A `body_width` too narrow to fit both
/// minimums degrades to an even half-split rather than inverting the panes or
/// underflowing. Pure so a fresh drag ([`App::drag_split_to`]) and every
/// render's re-clamp ([`resolve_list_width`], called from
/// `tui::view::render_body`) share the exact same rule.
#[must_use]
pub fn clamp_list_width(requested: u16, body_width: u16) -> u16 {
    if body_width < MIN_PANE_WIDTH * 2 {
        return body_width / 2;
    }
    requested.clamp(MIN_PANE_WIDTH, body_width - MIN_PANE_WIDTH)
}

/// Resolve the list pane's width in columns for a body `body_width` columns
/// wide: the persisted drag width when one exists (re-clamped against the
/// CURRENT `body_width`, so a stale width from a wider terminal never
/// survives a resize into a degenerate layout), or the historical 48% default
/// when the user has never dragged the splitter. Called every render
/// (`tui::view::render_body`) rather than trusting a stored value on its own.
#[must_use]
pub fn resolve_list_width(list_width: Option<u16>, body_width: u16) -> u16 {
    let requested =
        list_width.unwrap_or_else(|| (u32::from(body_width) * DEFAULT_LIST_PERCENT / 100) as u16);
    clamp_list_width(requested, body_width)
}

/// The full TUI state model.
///
/// Selection is stored as a stable `session_id` (never a list index) so it
/// survives an autorefresh reload; `scroll` is likewise preserved across
/// reloads and only clamped to bounds.
pub struct App {
    /// Every loaded session, in the store's stable repo->branch->timestamp
    /// order.
    pub sessions: Vec<Session>,
    /// Indices into [`sessions`](Self::sessions) that pass scope+query, kept in
    /// scope-aware DISPLAY order: flat timestamp-desc for the current-folder
    /// scope; group-most-recent-desc then timestamp-desc for the all scope.
    pub filtered: Vec<usize>,
    /// Stable id of the selected session (survives reload); `None` when the
    /// filtered list is empty.
    pub selected: Option<String>,
    /// First visible list row (scroll offset); preserved across reloads.
    pub scroll: usize,
    /// Live search query text.
    pub query: String,
    /// Name-only vs. name+content search (mirrors the search index mode).
    pub search_mode: SearchMode,
    /// Current-folder vs. all scope.
    pub scope: Scope,
    /// Canonicalized launch directory for the current-folder predicate.
    pub launch_dir: PathBuf,
    /// Whether the preview pane is visible.
    pub show_preview: bool,
    /// Vertical scroll offset (in WRAPPED rows) of the preview pane. Requested by
    /// the scroll keys and clamped to content bounds by `view::render_preview`,
    /// which writes the clamped value back (mirroring `scroll`/`ListState`).
    pub preview_scroll: u16,
    /// When set, the preview stays pinned to the BOTTOM (newest turn): the next
    /// render resolves the offset to `max_offset`. Set on any selection change
    /// and when the preview is toggled on; cleared by an explicit up/page/Home
    /// scroll; re-enabled by `End`.
    pub preview_follow_bottom: bool,
    /// Last inner viewport height of the preview (rows), written back by the view
    /// so page / quarter-page scrolling can size a page without knowing the layout.
    pub preview_viewport_h: u16,
    /// Last-rendered list-pane rectangle, written back by the view each frame so
    /// a mouse wheel can be hit-tested to the list without a terminal. Empty
    /// (`Rect::default`) until the first render.
    pub list_rect: Rect,
    /// Last-rendered preview-pane rectangle (mouse hit-testing). Set to an EMPTY
    /// rect when the preview is hidden so it never matches a hit-test.
    pub preview_rect: Rect,
    /// List pane width in columns, set the first time the user drags the
    /// splitter between the list and preview panes. `None` until the first
    /// drag: the view falls back to the historical 48% split
    /// ([`resolve_list_width`]). Re-clamped against the CURRENT body width on
    /// every render, so a stale width from a wider terminal never survives a
    /// resize into a degenerate layout.
    pub list_width: Option<u16>,
    /// Transient board status (e.g. a resume refusal for a deleted worktree).
    /// Rendered on the help line and cleared on the next actionable keypress.
    pub status: Option<String>,
    /// Currently-running sessions, keyed by full `session_id` (joined from
    /// `claude agents --json`). Refreshed OFF the UI thread; drives the live
    /// badges and the smart-Enter choice. Empty when the signal is unavailable.
    pub live: HashMap<String, LiveAgent>,
    /// The open running-session choice overlay, if any. `Some` while the
    /// Attach/Fork/Cancel prompt owns the keyboard.
    pub pending_live: Option<PendingLive>,
    /// Whether the list/preview splitter is currently being dragged (mouse
    /// button down on the seam). Private: only the drag methods below need
    /// it, mirroring `scoped`/`preview_cache`/`index`.
    dragging_split: bool,
    /// Indices (into `sessions`) that pass the scope predicate, cached so a
    /// per-keystroke query re-filter never re-canonicalizes paths.
    scoped: Vec<usize>,
    /// Readable, markdown-styled transcript preview, keyed by `session_id`.
    ///
    /// The rendered layout (GFM tables shrink-to-fit) depends on the preview
    /// pane's inner width, so the cache is scoped to a single width tracked in
    /// [`preview_width`](Self::preview_width): a width change CLEARS it rather
    /// than keying every entry by `(id, width)`. This keeps the cache to one
    /// `Text` per session and mirrors the reload-clear in `apply_sessions`;
    /// re-render on resize is cheap because only the selected session is ever
    /// rendered. Each entry carries the styled `Text` AND its clickable link
    /// regions (see [`preview::RenderedPreview`]); the two are produced from one
    /// pass at a fixed width, so a region's columns always match the drawn text.
    preview_cache: HashMap<String, preview::RenderedPreview>,
    /// Inner content width the `preview_cache` entries were rendered for. A
    /// change invalidates the whole cache (see `preview_cache`). `None` until
    /// the first preview render.
    preview_width: Option<u16>,
    /// Live nucleo index over `sessions` (isolated in [`crate::search`]).
    index: SearchIndex,
}

impl App {
    /// Build an app over `sessions` with the given `scope` and canonicalized
    /// `launch_dir`. Computes the initial filter and selects the first row.
    #[must_use]
    pub fn new(sessions: Vec<Session>, scope: Scope, launch_dir: PathBuf) -> Self {
        let index = SearchIndex::build(&sessions);
        let search_mode = index.mode();
        let mut app = App {
            sessions,
            filtered: Vec::new(),
            selected: None,
            scroll: 0,
            query: String::new(),
            search_mode,
            scope,
            launch_dir,
            show_preview: true,
            preview_scroll: 0,
            preview_follow_bottom: true,
            preview_viewport_h: 0,
            list_rect: Rect::default(),
            preview_rect: Rect::default(),
            list_width: None,
            status: None,
            live: HashMap::new(),
            pending_live: None,
            dragging_split: false,
            scoped: Vec::new(),
            preview_cache: HashMap::new(),
            preview_width: None,
            index,
        };
        app.recompute_scope();
        app.recompute_filtered();
        app.select_first();
        app
    }

    // --- selection (by stable id) -----------------------------------------

    /// The selected session, if any.
    #[must_use]
    pub fn selected_session(&self) -> Option<&Session> {
        let id = self.selected.as_ref()?;
        self.sessions.iter().find(|s| &s.session_id == id)
    }

    /// Position of the selected session within [`filtered`](Self::filtered).
    #[must_use]
    pub fn selected_pos(&self) -> Option<usize> {
        let id = self.selected.as_ref()?;
        self.filtered
            .iter()
            .position(|&i| &self.sessions[i].session_id == id)
    }

    /// The single selection setter: assign the selected id and, when it actually
    /// CHANGES, re-anchor the preview to the newest turn (follow-bottom) so a new
    /// session always opens at its most-recent message. A no-op reassignment of
    /// the same id (common on a query keystroke that keeps the row) leaves any
    /// manual preview scroll intact.
    fn set_selected(&mut self, next: Option<String>) {
        if self.selected != next {
            self.selected = next;
            self.preview_follow_bottom = true;
            self.preview_scroll = 0;
        }
    }

    /// Select the session at filtered position `pos` (no-op if out of range).
    fn select_pos(&mut self, pos: usize) {
        if let Some(&i) = self.filtered.get(pos) {
            let id = self.sessions[i].session_id.clone();
            self.set_selected(Some(id));
        }
    }

    /// Select the first filtered row, or clear selection when empty.
    fn select_first(&mut self) {
        let next = self
            .filtered
            .first()
            .map(|&i| self.sessions[i].session_id.clone());
        self.set_selected(next);
    }

    /// Move the selection by `delta` rows, clamped to the filtered bounds.
    pub fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            self.set_selected(None);
            return;
        }
        let cur = self.selected_pos().unwrap_or(0) as isize;
        let last = self.filtered.len() as isize - 1;
        let next = (cur + delta).clamp(0, last) as usize;
        self.select_pos(next);
    }

    /// Move the list selection one mouse-wheel notch (`up = true` toward the
    /// top), reusing the clamped [`move_selection`](Self::move_selection).
    pub fn list_wheel(&mut self, up: bool) {
        let step = if up {
            -LIST_WHEEL_STEP
        } else {
            LIST_WHEEL_STEP
        };
        self.move_selection(step);
    }

    // --- query / mode / scope ---------------------------------------------

    /// Append a character to the query and re-filter (type-to-search).
    pub fn push_query_char(&mut self, c: char) {
        self.query.push(c);
        self.index.set_query(&self.query);
        self.reapply_preserving_selection();
    }

    /// Delete the last query character and re-filter.
    pub fn pop_query_char(&mut self) {
        if self.query.pop().is_some() {
            self.index.set_query(&self.query);
            self.reapply_preserving_selection();
        }
    }

    /// Toggle name-only vs. name+content search and re-filter.
    pub fn toggle_search_mode(&mut self) {
        self.search_mode = self.index.toggle_mode();
        self.reapply_preserving_selection();
    }

    /// Toggle current-folder vs. all scope and re-filter (recomputes the scope
    /// membership set, which is what canonicalizes paths).
    pub fn toggle_scope(&mut self) {
        self.scope = self.scope.toggled();
        self.recompute_scope();
        self.reapply_preserving_selection();
    }

    /// Toggle the preview pane visibility. Showing it (re)anchors to the newest
    /// turn so it opens at the bottom, matching a fresh selection.
    pub fn toggle_preview(&mut self) {
        self.show_preview = !self.show_preview;
        if self.show_preview {
            self.preview_follow_bottom = true;
            self.preview_scroll = 0;
        }
    }

    // --- preview scroll ----------------------------------------------------

    /// Move the preview by `rows` (positive = down/toward newer). Any explicit
    /// scroll drops follow-bottom and sets a concrete offset; the view clamps it
    /// to `[0, max_offset]` on the next render. Saturating so it can never
    /// underflow below zero or overflow `u16`.
    fn preview_scroll_by(&mut self, rows: i32) {
        self.preview_follow_bottom = false;
        let next = (i32::from(self.preview_scroll) + rows).clamp(0, i32::from(u16::MAX));
        self.preview_scroll = next as u16;
    }

    /// The page size for page/quarter-page scrolling: the last known viewport
    /// height, at least one row so a page always makes progress.
    fn preview_page(&self) -> u16 {
        self.preview_viewport_h.max(1)
    }

    /// Scroll the preview up one page (`PgUp`).
    pub fn preview_page_up(&mut self) {
        self.preview_scroll_by(-i32::from(self.preview_page()));
    }

    /// Scroll the preview down one page (`PgDn`).
    pub fn preview_page_down(&mut self) {
        self.preview_scroll_by(i32::from(self.preview_page()));
    }

    /// Scroll the preview up a quarter page (`Ctrl-U`).
    pub fn preview_half_up(&mut self) {
        self.preview_scroll_by(-i32::from((self.preview_page() / 4).max(1)));
    }

    /// Scroll the preview down a quarter page (`Ctrl-D`).
    pub fn preview_half_down(&mut self) {
        self.preview_scroll_by(i32::from((self.preview_page() / 4).max(1)));
    }

    /// Jump the preview to the top (`Home`); drops follow-bottom.
    pub fn preview_top(&mut self) {
        self.preview_follow_bottom = false;
        self.preview_scroll = 0;
    }

    /// Jump the preview to the bottom (`End`); re-enables follow-bottom so the
    /// view pins the offset to the newest turn.
    pub fn preview_bottom(&mut self) {
        self.preview_follow_bottom = true;
    }

    /// Scroll the preview one mouse-wheel notch (`up = true` toward older turns).
    /// Reuses the saturating clamp in [`preview_scroll_by`](Self::preview_scroll_by),
    /// so rapid notches neither underflow below zero nor overflow `u16`.
    pub fn preview_wheel(&mut self, up: bool) {
        let step = if up {
            -PREVIEW_WHEEL_STEP
        } else {
            PREVIEW_WHEEL_STEP
        };
        self.preview_scroll_by(step);
    }

    // --- splitter drag -------------------------------------------------------

    /// Begin dragging the list/preview splitter (mouse-down on the seam). A
    /// no-op while the running-session choice overlay owns input, so a stray
    /// click during the overlay can never start a drag that a later `Drag`
    /// event would then apply once the overlay closes.
    pub fn begin_split_drag(&mut self) {
        if self.pending_live.is_none() {
            self.dragging_split = true;
        }
    }

    /// Whether the splitter is currently being dragged — gates `Drag` event
    /// routing in `tui::update::handle_mouse`.
    #[must_use]
    pub fn is_dragging_split(&self) -> bool {
        self.dragging_split
    }

    /// Recompute and persist the list pane's width from an absolute mouse
    /// column and the current body width, via [`clamp_list_width`] so neither
    /// pane is crushed below [`MIN_PANE_WIDTH`] or inverted. Safe to call at
    /// any time — even a degenerate `body_width` (e.g. before the first
    /// render, or the preview hidden) yields a well-formed width, which is
    /// re-clamped again on the next render regardless.
    pub fn drag_split_to(&mut self, col: u16, body_width: u16) {
        self.list_width = Some(clamp_list_width(col, body_width));
    }

    /// End a splitter drag (mouse-up). Defensive: clears the flag even if no
    /// [`begin_split_drag`](Self::begin_split_drag) preceded it, so a stray
    /// `Up` can never wedge the drag state.
    pub fn end_split_drag(&mut self) {
        self.dragging_split = false;
    }

    // --- transient status --------------------------------------------------

    /// Set a transient board status (e.g. a resume refusal). Shown until the
    /// next actionable keypress clears it.
    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status = Some(message.into());
    }

    /// Clear any transient board status. Called at the start of handling an
    /// actionable keypress so a message survives exactly until the next input.
    pub fn clear_status(&mut self) {
        self.status = None;
    }

    // --- live agents + running-session choice overlay ---------------------

    /// Replace the live-agent set (delivered off-thread by the agents poller).
    /// Keyed by full `session_id`; the poller refreshes the whole map each cycle
    /// so stale entries self-heal without an explicit clear.
    pub fn set_live(&mut self, live: HashMap<String, LiveAgent>) {
        self.live = live;
    }

    /// The live-agent record for `session_id`, if that session is running now
    /// (drives the badge; `None` for a non-live row).
    #[must_use]
    pub fn live_agent(&self, session_id: &str) -> Option<&LiveAgent> {
        self.live.get(session_id)
    }

    /// Whether `session_id` is currently a live agent — the smart-Enter gate
    /// (join is a strict full-`session_id` match).
    #[must_use]
    pub fn is_live(&self, session_id: &str) -> bool {
        self.live.contains_key(session_id)
    }

    /// Look up a loaded session by its stable id.
    #[must_use]
    pub fn session_by_id(&self, session_id: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.session_id == session_id)
    }

    /// Open the Attach/Fork/Cancel overlay for a running session (Enter on a
    /// live row), defaulting the highlight to Attach.
    pub fn open_live_choice(&mut self, session_id: String) {
        self.pending_live = Some(PendingLive {
            session_id,
            selected: LiveChoice::Attach,
        });
    }

    /// Move the overlay highlight forward (wraps). No-op if no overlay is open.
    pub fn live_choice_next(&mut self) {
        self.cycle_live_choice(1);
    }

    /// Move the overlay highlight backward (wraps). No-op if no overlay is open.
    pub fn live_choice_prev(&mut self) {
        self.cycle_live_choice(-1);
    }

    /// Shift the highlighted choice by `delta` around [`LiveChoice::ORDER`].
    fn cycle_live_choice(&mut self, delta: isize) {
        if let Some(pending) = self.pending_live.as_mut() {
            let n = LiveChoice::ORDER.len() as isize;
            let next = (pending.selected.index() as isize + delta).rem_euclid(n) as usize;
            pending.selected = LiveChoice::ORDER[next];
        }
    }

    /// Dismiss the overlay, returning to the board.
    pub fn live_choice_cancel(&mut self) {
        self.pending_live = None;
    }

    // --- autorefresh reload -----------------------------------------------

    /// Replace the session list (a `SessionsChanged` reload), re-apply the
    /// active query+scope, and PRESERVE the selection by stable id and the
    /// scroll offset. If the selected id vanished, the selection clamps to the
    /// nearest surviving row.
    pub fn apply_sessions(&mut self, sessions: Vec<Session>) {
        let prev_id = self.selected.clone();
        let prev_pos = self.selected_pos();

        self.sessions = sessions;
        self.index.refresh(&self.sessions);
        // The transcript on disk may have changed; drop stale preview text.
        self.preview_cache.clear();
        self.recompute_scope();
        self.recompute_filtered();

        self.restore_selection(prev_id, prev_pos);
        self.clamp_scroll();
    }

    // --- internals --------------------------------------------------------

    /// Recompute the scope membership set (`scoped`). This is the only path
    /// that canonicalizes `cwd`s, so it runs on reload / scope-toggle, never on
    /// a per-keystroke query change.
    fn recompute_scope(&mut self) {
        let scope = self.scope;
        let launch = self.launch_dir.as_path();
        self.scoped = (0..self.sessions.len())
            .filter(|&i| in_scope(scope, &self.sessions[i], launch))
            .collect();
    }

    /// Recompute `filtered` from the cached scope set and the active query, then
    /// sort it into scope-aware display order via
    /// [`order_filtered`](Self::order_filtered).
    ///
    /// An empty query takes the whole scope set; a non-empty query keeps only
    /// the scope members that the nucleo index matched. Both paths funnel
    /// through [`order_filtered`](Self::order_filtered), so display ordering is
    /// applied uniformly and the result is never left in raw store order.
    fn recompute_filtered(&mut self) {
        if self.query.is_empty() {
            self.filtered = self.scoped.clone();
        } else {
            let matched: std::collections::HashSet<usize> =
                self.index.results().into_iter().collect();
            self.filtered = self
                .scoped
                .iter()
                .copied()
                .filter(|i| matched.contains(i))
                .collect();
        }
        self.order_filtered();
    }

    /// Sort [`filtered`](Self::filtered) into scope-aware DISPLAY order.
    ///
    /// [`Scope::CurrentFolder`] is a flat list ordered by timestamp DESC (with
    /// `None` last), tie-broken by `session_id` ascending for determinism.
    /// [`Scope::All`] orders groups by each group's most-recent (max) timestamp
    /// DESC (a group whose sessions are all `None` sorts last), then by group
    /// key ascending so same-group rows stay contiguous, then by session
    /// timestamp DESC (`None` last), then `session_id` ascending. The per-group
    /// max is precomputed once so the sort stays O(n log n).
    fn order_filtered(&mut self) {
        match self.scope {
            Scope::CurrentFolder => {
                let sessions = &self.sessions;
                self.filtered.sort_by_cached_key(|&i| {
                    let s = &sessions[i];
                    (Reverse(s.timestamp), s.session_id.clone())
                });
            }
            Scope::All => {
                // Precompute each group's most-recent timestamp once. Option's
                // Ord gives `Some > None` and later-time-greater, so `max`
                // yields the group's newest session (or `None` if all are).
                let mut group_max: HashMap<(String, String), Option<OffsetDateTime>> =
                    HashMap::new();
                for &i in &self.filtered {
                    let s = &self.sessions[i];
                    let key = (s.repo.clone(), s.branch_display().to_string());
                    let entry = group_max.entry(key).or_default();
                    *entry = (*entry).max(s.timestamp);
                }
                let sessions = &self.sessions;
                self.filtered.sort_by_cached_key(|&i| {
                    let s = &sessions[i];
                    let key = (s.repo.clone(), s.branch_display().to_string());
                    let gmax = group_max.get(&key).copied().flatten();
                    (
                        Reverse(gmax),
                        key,
                        Reverse(s.timestamp),
                        s.session_id.clone(),
                    )
                });
            }
        }
    }

    /// Re-filter after a query/mode/scope change while preserving the selection
    /// by id (clamping to nearest if it left the filtered set).
    fn reapply_preserving_selection(&mut self) {
        let prev_id = self.selected.clone();
        let prev_pos = self.selected_pos();
        self.recompute_filtered();
        self.restore_selection(prev_id, prev_pos);
        self.clamp_scroll();
    }

    /// Restore the selection after `filtered` was recomputed: keep the same id
    /// if it survived, else clamp the previous position into the new list.
    fn restore_selection(&mut self, prev_id: Option<String>, prev_pos: Option<usize>) {
        let survived = prev_id.and_then(|id| {
            self.filtered
                .iter()
                .position(|&i| self.sessions[i].session_id == id)
        });
        match survived {
            Some(pos) => self.select_pos(pos),
            None if self.filtered.is_empty() => self.set_selected(None),
            None => {
                let pos = prev_pos.unwrap_or(0).min(self.filtered.len() - 1);
                self.select_pos(pos);
            }
        }
    }

    /// Clamp the scroll offset into the current row bounds.
    fn clamp_scroll(&mut self) {
        let max = self.filtered.len().saturating_sub(1);
        self.scroll = self.scroll.min(max);
    }

    // --- render helpers (consumed by tui::view) ---------------------------

    /// The rows for the current filtered list — grouped in the all scope, flat
    /// in the current-folder scope (see [`build_rows`]).
    #[must_use]
    pub fn rows(&self) -> Vec<Row> {
        build_rows(&self.sessions, &self.filtered, self.scope)
    }

    /// The row index (into [`rows`](Self::rows)) of the selected session, for
    /// driving the `ListState` highlight.
    #[must_use]
    pub fn selected_row(&self, rows: &[Row]) -> Option<usize> {
        let sel = self.selected_pos()?;
        let target = *self.filtered.get(sel)?;
        rows.iter()
            .position(|r| matches!(r, Row::Session(i) if *i == target))
    }

    /// The CHAR indices within `display` (a row's visible label) that the
    /// active query matches, for search-match highlighting in the list.
    ///
    /// Delegates to the nucleo seam in [`crate::search`], which scores against
    /// the display string itself (decoupled from the filtering haystack) and
    /// returns sorted, deduplicated char positions. Empty when the query is
    /// empty or does not appear in the visible label (e.g. a content-only hit).
    #[must_use]
    pub fn match_indices(&mut self, display: &str) -> Vec<u32> {
        self.index.match_indices(display)
    }

    /// The width-scoped preview cache entry for the current selection, rendering
    /// and inserting it on a miss.
    ///
    /// A change in `inner_width` CLEARS the whole cache first: GFM tables
    /// shrink-to-fit, so both the layout and the link-region columns depend on the
    /// width (see `preview_cache`). `None` when nothing is selected or the selected
    /// id is no longer among the loaded sessions. This is the single source both
    /// [`preview_text`](Self::preview_text) and
    /// [`preview_hit_context`](Self::preview_hit_context) read, so the text drawn
    /// and the regions hit-tested can never come from different renders.
    fn ensure_preview(&mut self, inner_width: u16) -> Option<&preview::RenderedPreview> {
        if self.preview_width != Some(inner_width) {
            self.preview_cache.clear();
            self.preview_width = Some(inner_width);
        }
        let id = self.selected.clone()?;
        if !self.preview_cache.contains_key(&id) {
            let session = self.sessions.iter().find(|s| s.session_id == id)?;
            let rendered = preview::render(session, usize::from(inner_width));
            self.preview_cache.insert(id.clone(), rendered);
        }
        self.preview_cache.get(&id)
    }

    /// Readable, markdown-styled transcript for the selected session, fit to
    /// `inner_width` (the preview pane's inner content width, in columns) so GFM
    /// tables shrink-to-fit and never wrap. Lazily rendered and cached by id;
    /// a change in `inner_width` clears the cache first (see `preview_cache`).
    /// Empty `Text` when nothing is selected.
    pub fn preview_text(&mut self, inner_width: u16) -> Text<'static> {
        self.ensure_preview(inner_width)
            .map(|p| p.text.clone())
            .unwrap_or_default()
    }

    /// The wrapped-layout context needed to hit-test a mouse click into a preview
    /// link: each content line's DISPLAY width (feeding the SAME wrap model the
    /// scrollbar/`content_h` path uses, via `view::wrapped_line_height`) and the
    /// clickable [`LinkRegion`](preview::LinkRegion)s — both pulled from the SAME
    /// width-scoped cache the view drew from, so a hit-test can never disagree with
    /// what is on screen. Empty when nothing is selected.
    pub fn preview_hit_context(
        &mut self,
        inner_width: u16,
    ) -> (Vec<usize>, Vec<preview::LinkRegion>) {
        match self.ensure_preview(inner_width) {
            Some(p) => (
                p.text.lines.iter().map(Line::width).collect(),
                p.links.clone(),
            ),
            None => (Vec::new(), Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Synthetic session with an explicit id, repo, branch, and cwd.
    fn session(id: &str, repo: &str, branch: Option<&str>, cwd: &str) -> Session {
        Session {
            file: PathBuf::from(format!("/tmp/{id}.jsonl")),
            session_id: id.to_string(),
            cwd: PathBuf::from(cwd),
            git_branch: branch.map(str::to_string),
            timestamp: None,
            repo: repo.to_string(),
            label: format!("label for {id}"),
            content_index: String::new(),
        }
    }

    /// Like [`session`] but with a concrete timestamp (unix seconds), for
    /// exercising the scope-aware display ordering.
    fn session_ts(
        id: &str,
        repo: &str,
        branch: Option<&str>,
        cwd: &str,
        unix_secs: i64,
    ) -> Session {
        let mut s = session(id, repo, branch, cwd);
        s.timestamp = Some(OffsetDateTime::from_unix_timestamp(unix_secs).unwrap());
        s
    }

    /// An app over `sessions` in [`Scope::All`] (scope does not interfere) with
    /// a throwaway launch dir.
    fn app_all(sessions: Vec<Session>) -> App {
        App::new(sessions, Scope::All, PathBuf::from("/tmp/launch"))
    }

    fn unique_temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!("snapback-app-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    // --- selection preservation by id across a reload ---------------------

    #[test]
    fn reload_preserves_selection_by_id_not_index() {
        // Distinct single-session groups; All-scope display order is driven by
        // each group's timestamp, most-recent first -> [a, b, c, d].
        let v1 = vec![
            session_ts("a", "r1", Some("main"), "/tmp/a", 400),
            session_ts("b", "r2", Some("main"), "/tmp/b", 300),
            session_ts("c", "r3", Some("main"), "/tmp/c", 200),
            session_ts("d", "r4", Some("main"), "/tmp/d", 100),
        ];
        let mut app = app_all(v1);
        // Select "c" at display position 2.
        app.move_selection(2);
        assert_eq!(app.selected.as_deref(), Some("c"));
        assert_eq!(app.selected_pos(), Some(2));

        // Reload with the SAME ids but new timestamps that float "c" to the top
        // (display order becomes [c, d, b, a]); the raw array index of "c" (2)
        // no longer matches its display position.
        let v2 = vec![
            session_ts("a", "r1", Some("main"), "/tmp/a", 100),
            session_ts("b", "r2", Some("main"), "/tmp/b", 200),
            session_ts("c", "r3", Some("main"), "/tmp/c", 400),
            session_ts("d", "r4", Some("main"), "/tmp/d", 300),
        ];
        app.apply_sessions(v2);

        // The stable id survived; its position tracked the new display order.
        assert_eq!(
            app.selected.as_deref(),
            Some("c"),
            "selection must follow the stable id across a reorder"
        );
        assert_eq!(app.selected_pos(), Some(0));
    }

    #[test]
    fn reload_clamps_to_nearest_when_selected_id_vanishes() {
        // Display order [a, b, c, d] by timestamp desc.
        let v1 = vec![
            session_ts("a", "r1", Some("main"), "/tmp/a", 400),
            session_ts("b", "r2", Some("main"), "/tmp/b", 300),
            session_ts("c", "r3", Some("main"), "/tmp/c", 200),
            session_ts("d", "r4", Some("main"), "/tmp/d", 100),
        ];
        let mut app = app_all(v1);
        // Select "c" at display position 2.
        app.move_selection(2);
        assert_eq!(app.selected.as_deref(), Some("c"));
        assert_eq!(app.selected_pos(), Some(2));

        // Reload with "c" removed; display order is [a, b, d]. Previous position
        // 2 clamps to the last row, which is "d" — the nearest surviving row.
        let v3 = vec![
            session_ts("a", "r1", Some("main"), "/tmp/a", 400),
            session_ts("b", "r2", Some("main"), "/tmp/b", 300),
            session_ts("d", "r4", Some("main"), "/tmp/d", 100),
        ];
        app.apply_sessions(v3);
        assert_eq!(
            app.selected.as_deref(),
            Some("d"),
            "a vanished id must clamp to the nearest surviving row by position"
        );
        assert_eq!(app.selected_pos(), Some(2));
    }

    // --- preview scroll anchoring + bounds --------------------------------

    #[test]
    fn a_new_app_anchors_the_preview_to_the_bottom() {
        let app = app_all(vec![session("a", "r1", Some("main"), "/tmp/a")]);
        assert!(
            app.preview_follow_bottom,
            "a fresh app must open the preview at the newest turn"
        );
        assert_eq!(app.preview_scroll, 0);
    }

    #[test]
    fn changing_selection_reanchors_the_preview_to_the_bottom() {
        let mut app = app_all(vec![
            session("a", "r1", Some("main"), "/tmp/a"),
            session("b", "r2", Some("main"), "/tmp/b"),
        ]);
        // Simulate the user having scrolled up in the current preview.
        app.preview_follow_bottom = false;
        app.preview_scroll = 7;

        // Selecting a DIFFERENT session re-anchors to the bottom.
        app.move_selection(1);
        assert_ne!(app.selected.as_deref(), Some("a"));
        assert!(
            app.preview_follow_bottom,
            "a selection change must re-anchor the preview to the bottom"
        );
        assert_eq!(app.preview_scroll, 0);
    }

    #[test]
    fn reselecting_the_same_session_keeps_manual_scroll() {
        let mut app = app_all(vec![session("a", "r1", Some("main"), "/tmp/a")]);
        app.preview_follow_bottom = false;
        app.preview_scroll = 4;
        // A no-op move (already at the only row) must not disturb the scroll.
        app.move_selection(0);
        assert!(!app.preview_follow_bottom, "same id must not re-anchor");
        assert_eq!(app.preview_scroll, 4);
    }

    #[test]
    fn toggling_the_preview_on_reanchors_to_the_bottom() {
        let mut app = app_all(vec![session("a", "r1", Some("main"), "/tmp/a")]);
        app.preview_follow_bottom = false;
        app.preview_scroll = 9;
        app.toggle_preview(); // hide
        app.toggle_preview(); // show again
        assert!(app.show_preview);
        assert!(
            app.preview_follow_bottom,
            "re-showing the preview re-anchors it to the newest turn"
        );
        assert_eq!(app.preview_scroll, 0);
    }

    #[test]
    fn preview_scroll_keys_clear_follow_and_saturate_at_bounds() {
        let mut app = app_all(vec![session("a", "r1", Some("main"), "/tmp/a")]);
        app.preview_viewport_h = 10; // set by the view; sizes a page

        app.preview_page_down();
        assert!(
            !app.preview_follow_bottom,
            "an explicit scroll drops follow"
        );
        assert_eq!(app.preview_scroll, 10, "a page is one viewport height");

        app.preview_half_up();
        assert_eq!(
            app.preview_scroll, 8,
            "Ctrl-U scrolls a quarter page (viewport/4)"
        );
        app.preview_half_down();
        assert_eq!(
            app.preview_scroll, 10,
            "Ctrl-D scrolls a quarter page back down (viewport/4)"
        );

        app.preview_page_up();
        assert_eq!(app.preview_scroll, 0, "cannot scroll above the top");
        app.preview_page_up();
        assert_eq!(app.preview_scroll, 0, "top saturates (no underflow)");

        app.preview_top();
        assert!(!app.preview_follow_bottom);
        assert_eq!(app.preview_scroll, 0);

        app.preview_bottom();
        assert!(
            app.preview_follow_bottom,
            "End re-enables follow-bottom so the view pins to the newest turn"
        );
    }

    // --- splitter drag: clamp math + state machine -------------------------

    #[test]
    fn clamp_list_width_clamps_to_the_minimum_on_both_sides() {
        // Dragging far left clamps to MIN_PANE_WIDTH.
        assert_eq!(clamp_list_width(0, 100), MIN_PANE_WIDTH);
        // Dragging far right leaves the preview MIN_PANE_WIDTH columns.
        assert_eq!(clamp_list_width(1000, 100), 100 - MIN_PANE_WIDTH);
        // A mid-range request passes through untouched.
        assert_eq!(clamp_list_width(40, 100), 40);
    }

    #[test]
    fn clamp_list_width_degrades_to_a_half_split_when_too_narrow_for_both_minimums() {
        // A body narrower than 2*MIN_PANE_WIDTH cannot fit both minimums;
        // falling back to an even half-split never inverts or underflows.
        let body = MIN_PANE_WIDTH; // less than MIN_PANE_WIDTH * 2
        let width = clamp_list_width(0, body);
        assert_eq!(width, body / 2);
        assert!(width <= body, "must never exceed the body width");
    }

    #[test]
    fn resolve_list_width_defaults_to_48_percent_until_a_drag_sets_one() {
        assert_eq!(resolve_list_width(None, 100), 48);
        // A stale drag width from a WIDER terminal re-clamps to the current,
        // narrower body rather than overflowing it.
        assert_eq!(resolve_list_width(Some(90), 40), 40 - MIN_PANE_WIDTH);
        // A drag width that already fits passes through untouched.
        assert_eq!(resolve_list_width(Some(30), 100), 30);
    }

    #[test]
    fn begin_split_drag_is_a_no_op_while_the_live_choice_overlay_is_open() {
        let mut app = app_all(vec![session("a", "r1", Some("main"), "/tmp/a")]);
        app.open_live_choice("a".to_string());
        app.begin_split_drag();
        assert!(
            !app.is_dragging_split(),
            "a stray click during the overlay must not start a drag"
        );
    }

    #[test]
    fn begin_split_drag_starts_dragging_when_no_overlay_is_open() {
        let mut app = app_all(vec![session("a", "r1", Some("main"), "/tmp/a")]);
        assert!(!app.is_dragging_split());
        app.begin_split_drag();
        assert!(app.is_dragging_split());
    }

    #[test]
    fn drag_split_to_never_inverts_or_panics_on_a_narrow_body() {
        let mut app = app_all(vec![session("a", "r1", Some("main"), "/tmp/a")]);
        // Dragging far left/right on a normal body clamps to the minimums.
        app.drag_split_to(0, 100);
        assert_eq!(app.list_width, Some(MIN_PANE_WIDTH));
        app.drag_split_to(u16::MAX, 100);
        assert_eq!(app.list_width, Some(100 - MIN_PANE_WIDTH));
        // A degenerate (very narrow) body must never panic or invert.
        app.drag_split_to(5, 3);
        assert_eq!(app.list_width, Some(1));
        // A zero-width body (e.g. before the first render) is also safe.
        app.drag_split_to(5, 0);
        assert_eq!(app.list_width, Some(0));
    }

    #[test]
    fn end_split_drag_clears_the_flag_even_without_a_preceding_begin() {
        let mut app = app_all(vec![session("a", "r1", Some("main"), "/tmp/a")]);
        assert!(!app.is_dragging_split());
        app.end_split_drag(); // defensive: no panic, stays cleared
        assert!(!app.is_dragging_split());

        app.begin_split_drag();
        assert!(app.is_dragging_split());
        app.end_split_drag();
        assert!(!app.is_dragging_split());
    }

    #[test]
    fn reload_to_empty_clears_selection() {
        let mut app = app_all(vec![session("a", "r1", Some("main"), "/tmp/a")]);
        assert_eq!(app.selected.as_deref(), Some("a"));
        app.apply_sessions(vec![]);
        assert_eq!(app.selected, None, "an empty reload clears the selection");
        assert!(app.filtered.is_empty());
    }

    // --- scope predicate (exact canonical cwd match) ----------------------

    #[test]
    fn scope_predicate_matches_exact_canonical_cwd() {
        let here = unique_temp_dir("scope-here");
        let other = unique_temp_dir("scope-other");
        let launch = resolve_dir(&here);

        let inside = session("in", "r", Some("main"), here.to_str().unwrap());
        let outside = session("out", "r", Some("main"), other.to_str().unwrap());

        // Current-folder: only the exact-cwd session is in scope.
        assert!(in_scope(Scope::CurrentFolder, &inside, &launch));
        assert!(!in_scope(Scope::CurrentFolder, &outside, &launch));

        // All: everything is in scope.
        assert!(in_scope(Scope::All, &inside, &launch));
        assert!(in_scope(Scope::All, &outside, &launch));

        let _ = std::fs::remove_dir_all(&here);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn scope_predicate_falls_back_to_raw_path_for_missing_cwd() {
        // A deleted worktree: the cwd no longer resolves, so resolve_dir falls
        // back to the raw path and an exact raw match still scopes it in.
        let launch = PathBuf::from("/nonexistent/snapback-test/gone");
        let gone = session("gone", "r", Some("main"), "/nonexistent/snapback-test/gone");
        let elsewhere = session(
            "else",
            "r",
            Some("main"),
            "/nonexistent/snapback-test/other",
        );
        assert!(in_scope(Scope::CurrentFolder, &gone, &launch));
        assert!(!in_scope(Scope::CurrentFolder, &elsewhere, &launch));
    }

    #[test]
    fn current_folder_scope_filters_the_list() {
        let here = unique_temp_dir("filter-here");
        let launch = resolve_dir(&here);
        let sessions = vec![
            session("in", "r1", Some("main"), here.to_str().unwrap()),
            session("out", "r2", Some("main"), "/tmp/somewhere-else"),
        ];
        let app = App::new(sessions, Scope::CurrentFolder, launch);
        assert_eq!(
            app.filtered.len(),
            1,
            "only the current-folder session shows"
        );
        assert_eq!(app.selected.as_deref(), Some("in"));
        let _ = std::fs::remove_dir_all(&here);
    }

    // --- grouping for render ----------------------------------------------

    #[test]
    fn grouping_emits_one_head_per_repo_branch_group() {
        let sessions = vec![
            session("s0", "repo-a", Some("main"), "/tmp/s0"),
            session("s1", "repo-a", Some("main"), "/tmp/s1"),
            session("s2", "repo-a", Some("dev"), "/tmp/s2"),
            session("s3", "repo-b", Some("main"), "/tmp/s3"),
        ];
        let filtered = vec![0usize, 1, 2, 3];
        let rows = build_rows(&sessions, &filtered, Scope::All);

        // Distinct groups: (repo-a,main), (repo-a,dev), (repo-b,main) -> 3 heads.
        let heads: Vec<&Row> = rows
            .iter()
            .filter(|r| matches!(r, Row::Group { .. }))
            .collect();
        assert_eq!(heads.len(), 3, "one head per repo->branch group: {rows:?}");

        // Every session row is preceded (somewhere above) by its own head, and a
        // head never repeats for the same group.
        assert_eq!(
            rows,
            vec![
                Row::Group {
                    repo: "repo-a".into(),
                    branch: "main".into()
                },
                Row::Session(0),
                Row::Session(1),
                Row::Group {
                    repo: "repo-a".into(),
                    branch: "dev".into()
                },
                Row::Session(2),
                Row::Group {
                    repo: "repo-b".into(),
                    branch: "main".into()
                },
                Row::Session(3),
            ]
        );
    }

    #[test]
    fn detached_branch_groups_under_detached_head() {
        let sessions = vec![session("s0", "repo-a", None, "/tmp/s0")];
        let rows = build_rows(&sessions, &[0], Scope::All);
        assert_eq!(
            rows[0],
            Row::Group {
                repo: "repo-a".into(),
                branch: "(detached)".into()
            },
            "a session with no branch groups under the (detached) head"
        );
    }

    #[test]
    fn selected_row_points_at_the_session_row_not_a_head() {
        let sessions = vec![
            session("s0", "repo-a", Some("main"), "/tmp/s0"),
            session("s1", "repo-b", Some("main"), "/tmp/s1"),
        ];
        let mut app = app_all(sessions);
        app.move_selection(1); // select s1
        let rows = app.rows();
        let row = app.selected_row(&rows).expect("selected row");
        assert!(
            matches!(rows[row], Row::Session(1)),
            "selected_row must land on the session row, not a group head: {rows:?}"
        );
    }

    // --- scope-aware display ordering -------------------------------------

    #[test]
    fn all_scope_orders_groups_by_most_recent_then_ts_desc() {
        // group-a: max 300 (members 300, 100); group-b: max 400 (members 400,
        // 200); group-c: max 50. Most-recent-desc across groups: b, a, c.
        let sessions = vec![
            session_ts("a-old", "repo-a", Some("main"), "/tmp/a1", 100),
            session_ts("a-new", "repo-a", Some("main"), "/tmp/a2", 300),
            session_ts("b-old", "repo-b", Some("main"), "/tmp/b1", 200),
            session_ts("b-new", "repo-b", Some("main"), "/tmp/b2", 400),
            session_ts("c-only", "repo-c", Some("main"), "/tmp/c1", 50),
        ];
        let app = app_all(sessions);
        let rows = app.rows();

        // Groups ordered by most-recent desc (b, a, c), one head each, same-group
        // rows contiguous and timestamp-desc within the group.
        assert_eq!(
            rows,
            vec![
                Row::Group {
                    repo: "repo-b".into(),
                    branch: "main".into()
                },
                Row::Session(3), // b-new (400)
                Row::Session(2), // b-old (200)
                Row::Group {
                    repo: "repo-a".into(),
                    branch: "main".into()
                },
                Row::Session(1), // a-new (300)
                Row::Session(0), // a-old (100)
                Row::Group {
                    repo: "repo-c".into(),
                    branch: "main".into()
                },
                Row::Session(4), // c-only (50)
            ],
            "groups most-recent-desc, rows ts-desc, one head per group: {rows:?}"
        );
    }

    #[test]
    fn current_folder_scope_is_flat_timestamp_desc() {
        let here = unique_temp_dir("flat-order");
        let launch = resolve_dir(&here);
        let cwd = here.to_str().unwrap();
        // Same cwd, DIFFERENT branches, different timestamps: order must be pure
        // timestamp-desc regardless of branch, with no group heads.
        let sessions = vec![
            session_ts("s-mid", "repo-a", Some("main"), cwd, 200),
            session_ts("s-new", "repo-a", Some("dev"), cwd, 300),
            session_ts("s-old", "repo-a", Some("feature"), cwd, 100),
        ];
        let app = App::new(sessions, Scope::CurrentFolder, launch);
        let rows = app.rows();

        assert!(
            rows.iter().all(|r| matches!(r, Row::Session(_))),
            "current-folder scope must be a flat, head-less list: {rows:?}"
        );
        assert_eq!(
            rows,
            vec![Row::Session(1), Row::Session(0), Row::Session(2)],
            "flat list ordered purely by timestamp desc: {rows:?}"
        );
        let _ = std::fs::remove_dir_all(&here);
    }

    #[test]
    fn build_rows_suppresses_heads_in_current_folder_scope() {
        let sessions = vec![
            session("s0", "repo-a", Some("main"), "/tmp/s0"),
            session("s1", "repo-b", Some("dev"), "/tmp/s1"),
        ];
        let rows = build_rows(&sessions, &[0, 1], Scope::CurrentFolder);
        assert_eq!(
            rows,
            vec![Row::Session(0), Row::Session(1)],
            "the folder scope emits only session rows, no heads: {rows:?}"
        );
    }

    // --- live-agent join + overlay state ----------------------------------

    fn live_agent(kind: &str) -> LiveAgent {
        LiveAgent {
            kind: kind.to_string(),
            id: None,
            state: None,
            status: None,
            name: None,
        }
    }

    /// Task VERIFY-2: a session whose id matches a live `sessionId` is flagged
    /// live with the correct kind; a non-matching id is NOT.
    #[test]
    fn live_set_joins_strictly_by_full_session_id() {
        let mut app = app_all(vec![
            session("live-id", "r1", Some("main"), "/tmp/a"),
            session("dead-id", "r2", Some("main"), "/tmp/b"),
        ]);
        let mut live = HashMap::new();
        live.insert("live-id".to_string(), live_agent("background"));
        app.set_live(live);

        assert!(app.is_live("live-id"), "the matching id is live");
        assert_eq!(
            app.live_agent("live-id").map(LiveAgent::kind_label),
            Some("bg"),
            "kind carries through the join"
        );
        assert!(!app.is_live("dead-id"), "a non-matching id is not live");
        assert!(app.live_agent("dead-id").is_none());
        // A partial / prefix id must NOT match (strict full-id join).
        assert!(!app.is_live("live"));
    }

    #[test]
    fn opening_and_cycling_the_choice_overlay_is_a_wrapping_state_machine() {
        let mut app = app_all(vec![session("s", "r", Some("main"), "/tmp/s")]);
        assert!(app.pending_live.is_none());

        app.open_live_choice("s".to_string());
        let pending = app.pending_live.clone().expect("overlay open");
        assert_eq!(pending.session_id, "s");
        assert_eq!(pending.selected, LiveChoice::Attach, "defaults to Attach");

        app.live_choice_next();
        assert_eq!(
            app.pending_live.as_ref().unwrap().selected,
            LiveChoice::Fork
        );
        app.live_choice_next();
        assert_eq!(
            app.pending_live.as_ref().unwrap().selected,
            LiveChoice::Cancel
        );
        app.live_choice_next();
        assert_eq!(
            app.pending_live.as_ref().unwrap().selected,
            LiveChoice::Attach,
            "the highlight wraps"
        );
        app.live_choice_prev();
        assert_eq!(
            app.pending_live.as_ref().unwrap().selected,
            LiveChoice::Cancel,
            "prev wraps the other way"
        );

        app.live_choice_cancel();
        assert!(app.pending_live.is_none(), "cancel returns to the board");
    }
}
