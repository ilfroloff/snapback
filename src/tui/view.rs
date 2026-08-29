//! View rendering.
//!
//! Draws the two-pane layout: a session list on the left, a readable transcript
//! preview on the right, plus a header/help line and a search input line. The
//! right pane is not always a transcript: the compose editor docks into its
//! bottom while composing, and a `Ctrl-N` background draft replaces the
//! transcript outright with a placeholder card ([`draft_card`]), since the session
//! it stands for does not exist yet. Two of the three scopes show group heads
//! (git-log-style, once per group): the all-folders scope heads each repo ->
//! branch group, and the project scope heads its branch groups under the ONE
//! resolved project label, since every row it draws belongs to that project. The
//! current-folder scope is the flat, datestamp-led, newest-first list with no
//! group heads, and the ONLY scope that draws flat — see
//! [`super::app::build_rows`], which owns why an unresolved project scope is not
//! a second one. Every session row leads with its datestamp column. Groups and
//! selection are styled with ratatui (no hand-written ANSI).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};
use ratatui::Frame;
use time::OffsetDateTime;
use unicode_segmentation::UnicodeSegmentation;

use crate::agents::{self, AgentActivity, ReportedAgent};
use crate::search::SearchMode;
use crate::store::preview::{self, LinkRegion};

use super::app::{
    resolve_list_width, App, Modal, ModalChoice, ModalLayout, NewSessionDraft, Row, Scope,
};
use super::compose::{ComposeState, ComposeTarget};

/// Render the whole UI for one frame.
///
/// Takes `&mut App` so the list's scroll offset (managed by ratatui's
/// `ListState`) can be written back into the model, keeping scroll preserved
/// across reloads, and so the preview text can be lazily rendered + cached.
pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    // A docked compose zone lives INSIDE the preview pane (no extra top-level row);
    // only when the pane is too short does compose claim a full-width bottom bar
    // between the body and the search line.
    let (header_area, body_area, compose_bar, search_area, help_area) =
        if compose_uses_bottom_bar(app.is_composing(), area.height) {
            // header (1) | body (fill) | compose bar (grows with the draft) | search (1) | help (1)
            let [header_area, body_area, compose_area, search_area, help_area] =
                Layout::vertical([
                    Constraint::Length(1),
                    Constraint::Fill(1),
                    Constraint::Length(compose_zone_height(app)),
                    Constraint::Length(1),
                    Constraint::Length(1),
                ])
                .areas(area);
            (
                header_area,
                body_area,
                Some(compose_area),
                search_area,
                help_area,
            )
        } else {
            // header (1) | body (fill) | search (1) | help (1)
            let [header_area, body_area, search_area, help_area] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Fill(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .areas(area);
            (header_area, body_area, None, search_area, help_area)
        };

    render_header(frame, app, header_area);
    render_body(frame, app, body_area);
    if let Some(compose_bar) = compose_bar {
        render_compose_zone(frame, app, compose_bar);
    }
    render_search(frame, app, search_area);
    render_help(frame, app, help_area);
    // A modal (the running-session choice or the new-session agent picker) sits
    // ON TOP of the board when open. The two overlays are now one `Option<Modal>`,
    // so at most one ever draws — a fact made structural, not conventional.
    if let Some(modal) = &app.modal {
        render_modal(frame, modal);
    }
    // The "stop the waiting agent?" confirmation overlays the board before compose
    // opens; likewise mutually exclusive with the other modals.
    if app.pending_stop.is_some() {
        render_stop_confirm(frame, app);
    }
    // The "stop this agent?" interrupt confirmation (Ctrl-K); mutually exclusive with
    // the other modals (each owns the keyboard while open).
    if app.pending_interrupt.is_some() {
        render_interrupt_confirm(frame, app);
    }
}

/// What sits between two header segments: a middot with breathing room either
/// side. Declared once so every segment — including the counter's optional
/// `· N hidden` tail — is joined by the SAME string, rather than by a literal
/// copied per call site that can drift a space.
const HEADER_SEPARATOR: &str = "  ·  ";

/// Prefix for a release build's version indicator (`v0.1.0`); the leading `v`
/// is the conventional marker readers expect before a semver string.
const RELEASE_VERSION_PREFIX: &str = "v";
/// Prefix for a local debug build's indicator (`dev+a1b2c3d`). The `+` is
/// semver build-metadata syntax carrying the source commit the binary was built
/// from, flagging at a glance that this is a hand-built dev binary, not a
/// shipped release.
const DEV_VERSION_PREFIX: &str = "dev+";
/// Suffix appended to a dev indicator when the working tree had uncommitted
/// changes at build time, so a hacked-on build (`dev+a1b2c3d-dirty`) is never
/// mistaken for a clean checkout of that commit.
const DEV_DIRTY_SUFFIX: &str = "-dirty";

/// Source commit short hash captured at build time (see `build.rs`), or
/// `unknown` when built outside a git repository. Only rendered for dev builds.
const GIT_HASH: &str = env!("SNAPBACK_GIT_HASH");
/// `"1"` when the working tree had uncommitted changes at build time, else
/// `"0"` (see `build.rs`). Only consulted for dev builds.
const GIT_DIRTY: &str = env!("SNAPBACK_GIT_DIRTY");

/// How many `AppEvent::Tick`s each phase of the live-badge pulse lasts.
///
/// The pulse is driven by the board's own redraw cadence rather than by the
/// terminal: 2 x [`crate::watch::TICK`] (250ms) = 500ms shown + 500ms hidden,
/// so one full cycle is 1000ms (~1Hz) — the classic cursor-blink rate, and the
/// cadence asked for. The two are MULTIPLIED, so this is only meaningful next to
/// `watch::TICK`: if that cadence ever changes, retune this to keep ~1Hz.
const BLINK_TICKS: u64 = 2;

/// The default badge glyph: a filled dot.
///
/// The glyph a row draws is chosen per BUCKET by [`badge_glyph`] — this `●` for
/// every bucket EXCEPT [`AgentActivity::NeedsInput`], which draws
/// [`BADGE_NEEDS_INPUT`] instead. WITHIN a row the glyph is then fixed: the pulse
/// alternates the badge's COLOR and must never touch its symbol (see
/// [`pulse_color`] for why), so whichever glyph a row picked is drawn identically
/// in both pulse phases, active or not.
const BADGE_DOT: &str = "\u{25cf}";

/// The badge glyph marking the ONE bucket that wants the user:
/// [`AgentActivity::NeedsInput`].
///
/// A second, SHAPE channel on top of the yellow-only color signal: `!` still
/// stands out in a monochrome terminal, or to a color-blind reader, where the
/// yellow dot reads the same as every other. Plain ASCII on purpose — it renders
/// everywhere, unlike an emoji or a wide glyph, and stays one cell wide so it
/// causes no layout shift against [`BADGE_DOT`]. Chosen strictly by BUCKET (see
/// [`badge_glyph`]); `NeedsInput` is steady, so this is drawn identically in both
/// pulse phases — the pulse still only ever changes color, never the symbol.
const BADGE_NEEDS_INPUT: &str = "!";

/// The red accent color of the [`AgentActivity::NeedsInput`] badge glyph (`!`).
///
/// Confined to that single `!` cell (see [`badge_glyph_color`]): the kind label
/// and qualifier keep [`badge_color`]'s yellow, so red is an ACCENT that lifts the
/// one bucket that wants the user above the palette, NOT a row-wide alarm — a
/// steady red on one cell, not the pulsing red the design deliberately avoids.
///
/// A NAMED ANSI color, never RGB (TERMINAL-SAFE STYLING), so it adapts to the
/// terminal theme; red reads as "act now" across themes.
const BADGE_NEEDS_INPUT_COLOR: Color = Color::Red;

/// The base badge color of the buckets that PULSE (`Working`, and `Other`
/// tracking it — see [`crate::agents::is_active`]).
///
/// Named rather than spelled inline in [`badge_color`] so [`pulse_color`] can
/// declare its dim partner against the SAME value the palette hands out: the two
/// are one pair, and a pulse that dimmed a color `badge_color` no longer emits
/// would silently stop pulsing.
const BADGE_WORKING: Color = Color::Gray;
/// [`BADGE_WORKING`]'s dim partner — the color its dot alternates to on the
/// pulse's off phase.
///
/// `DarkGray` is the NAMED ANSI dim gray, so it reads as the same badge at lower
/// intensity on any theme (TERMINAL-SAFE STYLING) rather than as a second state.
const BADGE_WORKING_DIM: Color = Color::DarkGray;

/// The badge color of the TERMINAL [`AgentActivity::Ended`] bucket — a background
/// job claude reports as `stopped` or `failed`.
///
/// `DarkGray` reads as DORMANT: dim and quiet, so an ENDED job sits visibly below
/// the live palette (yellow / green / working-gray) without claiming a state it no
/// longer holds. It is DELIBERATELY not green — green is [`AgentActivity::Done`]'s
/// "finished cleanly", and a stopped-or-failed job did not necessarily finish, so
/// it must not read as ready.
///
/// STEADY, with NO pulse partner: [`crate::agents::is_active`] is false for
/// `Ended`, so its dot never dims and [`pulse_color`] needs no arm for it (the
/// identity fallback is correct here, and `Ended` being a RESTING bucket is exactly
/// why `every_pulsing_buckets_badge_color_has_a_distinct_dim_partner` skips it).
///
/// A NAMED ANSI color, never RGB (TERMINAL-SAFE STYLING), so it adapts to the
/// terminal theme and survives a light background.
const BADGE_ENDED: Color = Color::DarkGray;

/// The selection marker `List` draws at the left of the highlighted row.
///
/// Named because it is also RESERVED width: ratatui pads EVERY row by this
/// symbol's columns (blanking it on unselected rows) before drawing the item, so
/// a row's real drawable width is the block's inner width less this. The label
/// fit ([`fit_label`]) has to subtract it, and reading it off the same const the
/// `List` is configured with means the two can never drift apart.
const LIST_HIGHLIGHT_SYMBOL: &str = "› ";

/// A session row's left gutter: two columns of breathing room between the
/// selection marker and the timestamp.
const ROW_GUTTER: &str = "  ";
/// An expanded lineage CHILD's left gutter, replacing [`ROW_GUTTER`].
///
/// One more level of indent plus a `↳`, so the row reads as subordinate to the
/// head above it. Sized against `ROW_GUTTER` rather than in absolute columns:
/// the extra indent is what makes a child visibly hang off its head, and the
/// glyph is what says which direction it hangs. Rows with no lineage keep
/// `ROW_GUTTER` and are therefore untouched by any of this.
const CHILD_GUTTER: &str = "   ↳ ";

/// The gap between a folded head's label and its `(+N)` marker.
///
/// Part of the marker's own reserved width (see [`lineage_marker`]) rather than a
/// separate span, so the width [`fit_label`] holds back is exactly the width the
/// marker later draws — there is one number, and it cannot be reserved wrongly.
const LINEAGE_MARKER_GAP: &str = "  ";

/// What a width-truncated label ends with. One column, so the arithmetic in
/// [`fit_label`] stays in columns without a width table.
const LABEL_ELLIPSIS: &str = "…";

/// The footnote a soft-hidden session row wears while the show-hidden toggle is
/// on: a dim `[hidden]` marker so the user can see what they hid (and un-hide
/// it). Carries its own leading gap, exactly as [`LINEAGE_MARKER_GAP`] folds its
/// gap into the `(+N)` marker, so the reserved width matches the drawn width.
const HIDDEN_ROW_MARKER: &str = "  [hidden]";

/// How many leading chars of a `session_id` a lineage CHILD row shows.
///
/// Eight: a session id is a uuid, whose first hyphen-delimited group is 8 hex
/// chars — the form these sessions are named by everywhere else (`e4a59d02`), and
/// far more than enough to tell apart the handful of members of ONE lineage,
/// which is the only comparison this row invites.
const CHILD_ID_CHARS: usize = 8;

/// The gap between a lineage CHILD row's id and its turn count.
///
/// Two columns, matching [`LINEAGE_MARKER_GAP`] and the row's other inter-column
/// gaps, so a child's fields sit on the same rhythm as every other row's. Folded
/// into the segment [`child_msgs`] builds, for the same reason the marker folds
/// its own gap in: the width reserved is then the width drawn.
const CHILD_MSGS_GAP: &str = "  ";

/// The unit a lineage CHILD row's turn count wears: `6 msgs`.
///
/// Spelled out rather than left a bare number, because a bare `6` sitting beside
/// an 8-char hex id reads as more id. The unit is what makes the number
/// self-describing at the glance this row is built for. Uniform across every
/// count (`1 msgs` is not special-cased): the plural rule would buy a
/// grammatically nicer edge case at the price of a width that depends on the
/// value, and this segment's width has to be knowable before it is drawn — see
/// [`fit_child_msgs`].
const CHILD_MSGS_SUFFIX: &str = " msgs";

/// The glyph of the search line's cursor.
const SEARCH_CURSOR: &str = "\u{258f}";
/// What the pulsing search cursor renders in its hidden phase: a SAME-WIDTH
/// blank. The cursor is currently the LAST span on its line, so blanking and
/// dropping it paint identical cells today; the blank is what keeps the column
/// held if anything is ever appended after it.
///
/// Show/hide is right HERE and only here: a cursor's whole job is to appear and
/// disappear, and nothing on the search line is auto-detected by the terminal, so
/// mutating this line's text costs nothing. The badge dot deliberately does NOT
/// work this way — see [`pulse_color`].
const SEARCH_CURSOR_HIDDEN: &str = " ";

/// The preview scrollbar's `begin_symbol`, shown ONLY when the preview is
/// pinned to the very top (`offset == 0`) — a clear directional glyph for the
/// boundary-only arrow, chosen deliberately since we set `begin_symbol`
/// explicitly per-offset below rather than relying on `Scrollbar`'s built-in
/// default (which would otherwise glue a static arrow to the track regardless
/// of scroll position).
const SCROLLBAR_BEGIN_ARROW: &str = "↑";
/// The preview scrollbar's `end_symbol`, shown ONLY when scrolled to the last
/// page (`offset >= max_offset`); the `SCROLLBAR_BEGIN_ARROW` counterpart.
const SCROLLBAR_END_ARROW: &str = "↓";
/// Blank stand-in for a hidden boundary arrow. Always passed as `Some(_)`
/// (never `None`) so the reserved arrow row keeps the SAME cell width whether
/// the glyph is showing or hidden: this holds the track's rendered length
/// constant across scroll positions, rather than the thumb's geometry
/// jittering each time an arrow pops in or out at an edge.
const SCROLLBAR_ARROW_HIDDEN: &str = " ";

/// Rows the PINNED status banner reserves at the top of the preview's
/// inner area (see [`preview_split`]). Exactly one: [`preview_banner`] is a
/// single, never-wrapped line, so a taller reservation would only add dead
/// space above the transcript and a shorter one would hide the banner outright.
const PREVIEW_BANNER_ROWS: u16 = 1;

/// How a search match is marked INSIDE the preview transcript.
///
/// A `Modifier`, never a color: a preview line arrives already styled by
/// `store::preview` (headings, DIM code, colored markers), so the mark must
/// COMPOSE onto whatever style it lands on — a fixed foreground would erase that
/// style and, on the wrong terminal theme, the text with it (TERMINAL-SAFE
/// STYLING). `REVERSED` is the one attribute this board already relies on being
/// honored (the list's selection highlight), unlike the blink attribute most
/// terminals silently ignore. It deliberately differs from the row label's
/// blue+BOLD: the label is plain text this view owns, whereas here BOLD/DIM are
/// already spoken for by the markdown pass.
const PREVIEW_MATCH_MODIFIER: Modifier = Modifier::REVERSED;

/// How far down the viewport a jumped-to search match is parked: one
/// [`MATCH_JUMP_LEAD_DIVISOR`]th of the transcript's height from the top, so a
/// pane of height `h` shows `h / 3` rows of LEAD-IN above the match and the rest
/// below it.
///
/// Not the top (`0`), which strands the match with no context above it and reads
/// as if the transcript began there; not the middle (`2`), which spends half the
/// pane on what the user has already been told. A third is the smallest lead that
/// still shows the turn a match belongs to while leaving the majority of the pane
/// for what follows — the direction a transcript is read in.
///
/// It is a MINIMUM, not a promise: the offset is clamped like every other
/// (`clamp_preview_offset`), so a match in the first rows of a transcript keeps
/// its natural position rather than scrolling above the start.
const MATCH_JUMP_LEAD_DIVISOR: u16 = 3;

/// The compose box starts at ONE visible text row and grows with the draft up to
/// [`COMPOSE_MAX_TEXT_ROWS`]; the `TextArea` scrolls internally beyond that.
const COMPOSE_MIN_TEXT_ROWS: u16 = 1;

/// The tallest the compose box grows before it scrolls internally — capped so a
/// long draft never swallows the transcript above a docked box.
const COMPOSE_MAX_TEXT_ROWS: u16 = 6;

/// The TALLEST the bordered compose zone can get: [`COMPOSE_MAX_TEXT_ROWS`] plus the
/// block's top and bottom border. The dock decision reserves room for THIS (not the
/// current height), so a box that grows as the user types never has to flip from
/// docked to bottom-bar mid-draft.
const COMPOSE_MAX_ZONE_HEIGHT: u16 = COMPOSE_MAX_TEXT_ROWS + 2;

/// Transcript rows kept visible above a DOCKED compose zone. A preview pane that
/// cannot spare this many (on top of the banner and a full-height compose zone)
/// falls back to the full-width bottom bar rather than crushing the transcript.
const COMPOSE_MIN_TRANSCRIPT_ROWS: u16 = 3;

/// Minimum preview-pane INNER height (inside the block borders) to DOCK the compose
/// zone in the preview: the pinned banner row, a usable slice of transcript, and a
/// FULL-HEIGHT compose zone (using the max keeps the decision stable as the box
/// grows). Below this, compose renders as a full-width bottom bar (see
/// [`compose_uses_bottom_bar`]) so it is never squeezed against the transcript.
const COMPOSE_MIN_DOCK_HEIGHT: u16 =
    PREVIEW_BANNER_ROWS + COMPOSE_MIN_TRANSCRIPT_ROWS + COMPOSE_MAX_ZONE_HEIGHT;

/// Board rows outside the body — the header, the search line, and the help line
/// (one row each). Lets the compose-placement decision recover the preview pane's
/// height from the whole board without threading laid-out rects into a pure
/// function.
const BOARD_CHROME_ROWS: u16 = 3;

/// The compose box's CURRENT visible text-row count: the draft's own soft-wrapped
/// height, clamped to `[COMPOSE_MIN_TEXT_ROWS, COMPOSE_MAX_TEXT_ROWS]`, so the box
/// starts at one line and grows with the content up to the cap.
///
/// The row count is ASKED OF THE EDITOR
/// ([`super::compose::ComposeState::screen_rows`]) rather than re-derived here, which
/// is also why this takes no width: the widget already knows the width it was drawn
/// at, so the docked box (inside the preview pane's border) and the full-width bottom
/// bar cannot end up sized against different widths. The view deliberately does NOT
/// reuse the transcript's measurement ([`wrapped_text_rows`]) for this: that asks a
/// ratatui `Paragraph` how IT would wrap, and the editor is a different widget with
/// its own wrap mode and tab expansion, so a shared answer would be a coincidence
/// rather than a contract.
fn compose_text_rows(app: &App) -> u16 {
    let rows = app
        .compose
        .as_ref()
        .map_or(0, super::compose::ComposeState::screen_rows);
    u16::try_from(rows)
        .unwrap_or(COMPOSE_MAX_TEXT_ROWS)
        .clamp(COMPOSE_MIN_TEXT_ROWS, COMPOSE_MAX_TEXT_ROWS)
}

/// Total rows the bordered compose zone occupies: [`compose_text_rows`] plus the
/// block's two borders.
fn compose_zone_height(app: &App) -> u16 {
    compose_text_rows(app) + 2
}

/// Build the header's version label from compile-time metadata.
///
/// Release builds (`cfg!(debug_assertions)` off — `cargo build --release`,
/// `cargo install`) show `v<crate-version>`, always tracking `Cargo.toml`.
/// Local debug builds (`cargo dev`/`run`/`test`) show `dev+<git-short-hash>`
/// with a trailing `-dirty` when the working tree had uncommitted changes at
/// build time, so a running TUI states whether it is a shipped release or a
/// local build and, if local, exactly which commit it came from.
fn version_label() -> String {
    format_version_label(cfg!(debug_assertions), GIT_HASH, GIT_DIRTY == "1")
}

/// Pure formatter split out of [`version_label`] so the release/dev/dirty
/// branching is unit-testable without a real build profile or git repository.
fn format_version_label(debug_build: bool, git_hash: &str, dirty: bool) -> String {
    if !debug_build {
        return format!("{}{}", RELEASE_VERSION_PREFIX, env!("CARGO_PKG_VERSION"));
    }
    let dirty = if dirty { DEV_DIRTY_SUFFIX } else { "" };
    format!("{DEV_VERSION_PREFIX}{git_hash}{dirty}")
}

/// The launch directory's own name for the header, falling back to its full path
/// when it has no final component (`/`). Shared by the folder- and
/// project-scoped labels so the two can never disagree about what to call the
/// place snapback was started in.
fn launch_dir_name(app: &App) -> String {
    app.launch_dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| app.launch_dir.to_string_lossy().into_owned())
}

/// What to call the project in the header: the label git resolved for the whole
/// worktree set, or the name of the repo ROOT the launch dir sits in when
/// nothing resolved.
///
/// The resolved label is PREFERRED because the project scope spans several
/// folders — naming the one worktree you happened to launch from would
/// misdescribe a list drawn from all of them. The fallback obeys the same
/// argument rather than contradicting it: an unresolved set still leaves the
/// scope spanning the whole repo (`App::in_scope`'s root arm needs no git), so
/// the header names that root, not the branch-named folder inside it. Uses
/// [`crate::worktrees::project_root_name`], the one place that fallback is
/// written, which is what keeps this and `App::project_label` in step.
fn project_name(app: &App) -> String {
    app.worktrees
        .label()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| crate::worktrees::project_root_name(&app.launch_dir))
}

/// The top status line: title, active scope, search mode, and counts on the
/// left, with the crate version indicator right-aligned on the same row.
///
/// BOTH sides of the counter come from [`App::session_counts`], and both count
/// LINEAGES — the renderer does no counting arithmetic of its own. Pairing a
/// local `app.filtered.len()` with that call's denominator is what once printed
/// `115 / 146`: a post-fold row count over a session-FILE count, two units on
/// one line. See [`crate::tui::app::SessionCounts`] for the invariants.
///
/// The denominator is NOT the store's size: it measures the launch PROJECT (the
/// whole store only under [`Scope::All`]), so a folder-scoped board reads
/// `5 / 30` — the lineages it draws over the ones a `Ctrl-A` widen would reach —
/// instead of advertising every session on the machine. Fully soft-hidden
/// lineages leave that denominator for a trailing `· N hidden` segment, drawn
/// only when N is non-zero; with show-hidden on they are back on the board and
/// back inside the denominator, so the segment goes away rather than counting
/// visible rows twice.
///
/// The segment is LAST for a width reason as well as a reading one: the version
/// label is right-aligned over this same `area`, so a narrow terminal loses the
/// rightmost text of this line first. No new width logic guards that — the row
/// has always been two overlaid paragraphs — but the order means the first thing
/// to go is the least load-bearing one, not the counter or the scope.
fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let scope = match app.scope {
        Scope::CurrentFolder => format!("folder:{}", launch_dir_name(app)),
        Scope::Project => format!("project:{}", project_name(app)),
        Scope::All => "all folders".to_string(),
    };
    let mode = match app.search_mode {
        SearchMode::NameOnly => "name",
        SearchMode::NameAndContent => "name+content",
    };
    let counts = app.session_counts();
    let dim = Style::default().add_modifier(Modifier::DIM);

    let mut header = vec![
        Span::styled(
            " snapback ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(scope, Style::default().fg(Color::Green)),
        Span::raw(HEADER_SEPARATOR),
        Span::raw("search: "),
        Span::styled(mode, Style::default().fg(Color::Yellow)),
        Span::raw(HEADER_SEPARATOR),
        Span::styled(
            format!("{} / {} sessions", counts.visible, counts.total),
            dim,
        ),
    ];
    if counts.hidden > 0 {
        header.push(Span::raw(HEADER_SEPARATOR));
        header.push(Span::styled(format!("{} hidden", counts.hidden), dim));
    }
    frame.render_widget(Paragraph::new(Line::from(header)), area);

    let version = Line::from(Span::styled(
        version_label(),
        Style::default().add_modifier(Modifier::DIM),
    ));
    frame.render_widget(Paragraph::new(version).alignment(Alignment::Right), area);
}

/// The two-pane body: grouped list on the left, preview on the right. The
/// split between them is a stateful, draggable width
/// ([`App::list_width`]/[`App::drag_split_to`]) rather than a fixed
/// percentage; [`resolve_list_width`] re-clamps whatever is stored against
/// THIS frame's `area.width` every render (mirroring how `render_preview`
/// re-clamps `preview_scroll` rather than trusting a stale value), so a
/// terminal resize between drags can never leave a pane degenerate.
fn render_body(frame: &mut Frame, app: &mut App, area: Rect) {
    if app.show_preview {
        let list_width = resolve_list_width(app.list_width, area.width);
        let [list_area, preview_area] =
            Layout::horizontal([Constraint::Length(list_width), Constraint::Fill(1)]).areas(area);
        // Persist the pane rects so a mouse wheel/splitter-drag can be
        // hit-tested against a pane (or the seam between them).
        app.list_rect = list_area;
        app.preview_rect = preview_area;
        render_list(frame, app, list_area);
        render_preview(frame, app, preview_area);
    } else {
        // Preview hidden: the list owns the whole body. An empty preview rect
        // never matches a hit-test, so a wheel over the list scrolls the list.
        app.list_rect = area;
        app.preview_rect = Rect::default();
        render_list(frame, app, area);
        // `render_preview` is the match jump's only consumer, and it does not run
        // here — so this frame is where a request armed with no pane on screen has
        // to die. Left armed it would fire on the frame the pane comes BACK on,
        // overriding the newest-turn anchor `App::toggle_preview` just set, for a
        // query the user typed before they re-opened the pane. Dropping it HERE
        // (rather than refusing to arm it) covers every route into the flag,
        // including the explicit `Shift`-arrow step.
        let _ = app.take_preview_match_jump();
    }
}

/// What an empty list says, and where it points next.
///
/// Each narrow scope names the scope `Ctrl-A` ACTUALLY reaches from it — the
/// next state in [`Scope::toggled`], not the widest one. Pure so that claim is
/// assertable: an empty board's only advice is this sentence, and a sentence
/// that names the wrong destination sends the user one key past what they
/// wanted — or, worse, promises a destination the key cannot reach at all,
/// which is what `all_enabled` is here to prevent.
fn empty_list_message(scope: Scope, all_enabled: bool) -> &'static str {
    match scope {
        Scope::CurrentFolder => {
            "No sessions in this folder.\nPress Ctrl-A to widen to this project's worktrees."
        }
        Scope::Project if all_enabled => {
            "No sessions in this project.\nPress Ctrl-A to show all folders."
        }
        // Ctrl-A only NARROWS from here without `-a`, so there is no widening
        // left to offer. Saying nothing beats naming a key that walks back to
        // the scope the user already found empty.
        Scope::Project => "No sessions in this project.",
        // Already the widest scope: there is nothing left to widen to.
        Scope::All => "No sessions found.",
    }
}

/// The grouped session list with git-log-style folder heads and a highlighted
/// selection. The `ListState` offset is seeded from and written back to
/// `app.scroll` so scroll survives reloads.
fn render_list(frame: &mut Frame, app: &mut App, area: Rect) {
    let rows = app.rows();
    let block = Block::default().borders(Borders::ALL).title(" sessions ");

    if rows.is_empty() {
        let msg = empty_list_message(app.scope, app.all_scope_enabled);
        frame.render_widget(
            Paragraph::new(msg)
                .block(block)
                .style(Style::default().add_modifier(Modifier::DIM)),
            area,
        );
        return;
    }

    // The columns a row can actually draw into: the block's inner width, less the
    // selection marker ratatui pads EVERY row with. Derived from the block and
    // the const the `List` below is configured with rather than restated, so a
    // border or marker change cannot leave this arithmetic behind.
    let content_width =
        usize::from(block.inner(area).width).saturating_sub(LIST_HIGHLIGHT_SYMBOL.chars().count());

    // Under a NON-EMPTY query, precompute which CHAR positions of each visible
    // session label the query matched, so the row can highlight them. The
    // highlight seam only READS the index, so this reads each label in place —
    // no snapshot clone and no borrow to sequence around. An empty query skips
    // the work entirely — nothing is highlighted.
    let highlights: HashMap<usize, HashSet<usize>> = if app.query.is_empty() {
        HashMap::new()
    } else {
        rows.iter()
            .filter_map(|row| match row {
                // A child row draws no label (it shows what DIFFERS from its
                // head instead), so it has nothing to highlight.
                Row::Session {
                    index,
                    child: false,
                    ..
                } => Some(*index),
                Row::Session { child: true, .. } | Row::Group { .. } => None,
            })
            .filter_map(|i| {
                let matched = app.match_indices(&app.sessions[i].label);
                if matched.is_empty() {
                    None
                } else {
                    Some((i, matched.into_iter().map(|p| p as usize).collect()))
                }
            })
            .collect()
    };

    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| match row {
            Row::Group { repo, branch } => ListItem::new(Line::from(vec![Span::styled(
                format!("▌ {repo}  ({branch})"),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )])),
            Row::Session {
                index: i,
                hidden,
                child,
            } => {
                let session = &app.sessions[*i];
                // A soft-hidden (persisted) session only reaches here when
                // `show_hidden` is on — `recompute_filtered` drops it otherwise.
                // Such a row is drawn DIM with a `[hidden]` footnote (below) so the
                // user can see what they hid and un-hide it. Note this reads the
                // persisted `hidden_ids` set, NOT the fold's per-row `hidden` count
                // matched above — the two are deliberately distinct.
                let soft_hidden = app.hidden_ids.contains(&session.session_id);
                let mut spans = vec![
                    // An expanded lineage member hangs off the head above it;
                    // every other row keeps the gutter it has always had.
                    Span::raw(if *child { CHILD_GUTTER } else { ROW_GUTTER }),
                    Span::styled(
                        short_time(session.timestamp),
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                    Span::raw("  "),
                ];
                // Compact agent badge in its own column: `● bg` / `● live`, plus
                // the translated qualifier phrase — LOUD for `NeedsInput` (`needs
                // input` at badge weight) and DIM for every other bucket. Rows
                // claude never reported show nothing here. Joined strictly by full
                // session_id.
                //
                // Deliberately keyed on REPORTED, not live: an agent that
                // reported completion must still render its badge — green and
                // steady — so the board shows what claude knows. Only Enter's
                // routing cares about liveness, and it asks claude directly
                // (`App::is_live_now`) rather than reading this map.
                if let Some(agent) = app.reported_agent(&session.session_id) {
                    // Dot and kind label are separate spans purely so they can
                    // differ in both color and pulse: the label carries the badge
                    // base, ONLY the dot pulses, and — for `NeedsInput` — only the
                    // dot reddens (see below). A blinking OR reddening text label
                    // would be noise on a board of live sessions.
                    let base = badge_color(agent);
                    let badge = Style::default().fg(base).add_modifier(Modifier::BOLD);
                    // The glyph's own base color: the red accent for `NeedsInput`
                    // (see [`badge_glyph_color`]), otherwise the badge base. Only
                    // this one cell diverges — the label and qualifier keep `base`.
                    let glyph_base = badge_glyph_color(agent);
                    let glyph = Style::default().fg(glyph_base).add_modifier(Modifier::BOLD);
                    // The pulse is APP-driven off the tick the loop already
                    // redraws on — see [`blink_visible`] for why the terminal
                    // cannot be asked to animate it. Only an ACTIVE agent
                    // pulses; a blocked/idle one is steady in both phases.
                    //
                    // It pulses by COLOR: the glyph itself is drawn every phase,
                    // so this row's TEXT never changes and the terminal is never
                    // forced to re-detect a link in the label beside it — see
                    // [`pulse_color`].
                    let dot = if agents::is_active(agent) && !blink_visible(app.tick) {
                        glyph.fg(pulse_color(glyph_base))
                    } else {
                        glyph
                    };
                    // The glyph is chosen by BUCKET, not by phase: `NeedsInput`
                    // marks its badge with a RED `!` (a shape + color accent that
                    // survives a monochrome or color-blind reader the yellow dot
                    // does not), every other bucket keeps a state-colored `●`. The
                    // pulse still only restyles this cell — whichever glyph the
                    // bucket picked is drawn identically in both phases (see
                    // [`badge_glyph`]).
                    spans.push(Span::styled(badge_glyph(agent), dot));
                    spans.push(Span::styled(format!(" {}", agent.kind_label()), badge));
                    // The translated qualifier phrase, weighted BY BUCKET. The one
                    // bucket that wants the user — `NeedsInput` — draws `needs
                    // input` (via the shared `agents::qualifier_copy`) at the
                    // badge's own color + BOLD, so it reads as loudly as the dot
                    // and kind label it shares that color with, instead of being
                    // the quietest text on the row. Every OTHER bucket keeps its
                    // raw qualifier DIM, exactly as before. Drawn as its OWN span,
                    // never via `friendly_status` — that fuses the kind label into
                    // the phrase, and the label is already its own span above.
                    if let Some(phrase) = agents::qualifier_copy(agent) {
                        let style = if agents::classify(agent) == AgentActivity::NeedsInput {
                            Style::default()
                                .fg(badge_color(agent))
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().add_modifier(Modifier::DIM)
                        };
                        spans.push(Span::raw(" "));
                        spans.push(Span::styled(phrase.to_string(), style));
                    }
                    spans.push(Span::raw("  "));
                }
                if *child {
                    // A child spends its width on what DIFFERS from its head,
                    // never on the label: every member of a lineage carries the
                    // SAME label by construction (one conversation, copied), so
                    // repeating it would spend the row saying nothing. What is
                    // genuinely its own is the timestamp and badge already drawn
                    // above, plus the id below — which is also the id `claude -r`
                    // would resume, i.e. the reason this row is kept reachable.
                    //
                    // Note what is NOT claimed here: the sketch's "plain-resumable"
                    // would be an assertion about claude's gate, and the badge
                    // beside it comes from the ~5s poll (skipped entirely, and
                    // thus stale indefinitely, while the board is idle past
                    // AGENTS_IDLE_AFTER). Liveness is unaskable in a render (see
                    // `preview_split`) and a polled snapshot is not authority
                    // for it, so the row REPORTS what claude said and
                    // leaves the verdict to the hand-off probe.
                    spans.push(Span::raw(short_id(&session.session_id)));

                    // ...and how much conversation it actually holds, which is
                    // the only field on this row carrying real information.
                    // Timestamp and id say WHICH member this is; `6 msgs` beside
                    // a sibling's `171 msgs` says which one is a stub the fork
                    // stalled and which one holds the work — the question the
                    // user is actually asking when they expand a lineage whose
                    // members are, by construction, label-identical.
                    //
                    // DIM, like the timestamp: the id is left the row's one
                    // undimmed field so the eye can scan children by it, and the
                    // count reads as an annotation hanging off it. A named
                    // Modifier, never an embedded escape or an RGB value
                    // (TERMINAL-SAFE STYLING).
                    let used: usize = spans.iter().map(Span::width).sum();
                    if let Some(msgs) = fit_child_msgs(session.msg_count, content_width, used) {
                        spans.push(Span::styled(
                            msgs,
                            Style::default().add_modifier(Modifier::DIM),
                        ));
                    }
                    if soft_hidden {
                        spans.push(Span::styled(
                            HIDDEN_ROW_MARKER,
                            Style::default().add_modifier(Modifier::DIM),
                        ));
                    }
                    return dim_row_if(ListItem::new(Line::from(spans)), soft_hidden);
                }

                // A folded head's `(+N)`, reserved BEFORE the label so a narrow
                // pane clips the (identical, redundant) label rather than the one
                // marker saying this row stands for others — see [`fit_label`].
                // `hidden` is 0 for every row with nothing hidden, and such a row
                // takes the untouched label it always has.
                let marker = (*hidden > 0).then(|| lineage_marker(*hidden));
                let label = match &marker {
                    Some(marker) => {
                        let used: usize = spans.iter().map(Span::width).sum();
                        fit_label(&session.label, content_width, used, marker.chars().count())
                    }
                    None => session.label.clone(),
                };

                // The visible label: under an active query, matched chars are
                // split out into light-blue spans; otherwise it is one raw span.
                // The base style is `default()` — the List's `highlight_style`
                // composes the selection over these spans at render time.
                match highlights.get(i) {
                    Some(matched) => spans.extend(highlight_label_spans(
                        &label,
                        matched,
                        Style::default(),
                        Style::default()
                            .fg(Color::LightBlue)
                            .add_modifier(Modifier::BOLD),
                    )),
                    None => spans.push(Span::raw(label)),
                }
                if let Some(marker) = marker {
                    // DIM: the marker is a footnote on the row, not a competitor
                    // to the label. A named-ANSI Modifier, never an embedded
                    // escape or an RGB value (TERMINAL-SAFE STYLING).
                    spans.push(Span::styled(
                        marker,
                        Style::default().add_modifier(Modifier::DIM),
                    ));
                }
                if soft_hidden {
                    spans.push(Span::styled(
                        HIDDEN_ROW_MARKER,
                        Style::default().add_modifier(Modifier::DIM),
                    ));
                }
                dim_row_if(ListItem::new(Line::from(spans)), soft_hidden)
            }
        })
        .collect();

    let selected_row = app.selected_row(&rows);
    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .add_modifier(Modifier::REVERSED)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(LIST_HIGHLIGHT_SYMBOL);

    let mut state = ListState::default();
    *state.offset_mut() = app.scroll.min(rows.len().saturating_sub(1));
    state.select(selected_row);
    frame.render_stateful_widget(list, area, &mut state);
    // Persist the offset ratatui computed so scroll is stable across redraws
    // and preserved across reloads.
    app.scroll = state.offset();
}

/// Dim an ENTIRE list row when it is a soft-hidden session shown under the
/// show-hidden toggle, so the demoted row reads that way at a glance while its
/// own spans (badge color, `[hidden]` marker) still compose over the base.
///
/// TERMINAL-SAFE STYLING: a named `Modifier`, never RGB or an embedded ANSI
/// escape (AGENTS.md). `DIM` is the same footnote treatment the timestamp,
/// lineage `(+N)` marker, and child id/count already use.
fn dim_row_if(item: ListItem<'_>, hidden: bool) -> ListItem<'_> {
    if hidden {
        item.style(Style::default().add_modifier(Modifier::DIM))
    } else {
        item
    }
}

/// The badge glyph for a reported agent, chosen by BUCKET.
///
/// [`BADGE_NEEDS_INPUT`] (`!`) for the ONE bucket that wants the user
/// ([`AgentActivity::NeedsInput`]), [`BADGE_DOT`] (`●`) for every other bucket.
/// Derived from [`crate::agents::classify`] — the bucket — never from the raw
/// `state`/`status` tokens, so it can never disagree with [`badge_color`] or the
/// pulse about what the qualifier meant.
///
/// This is a SHAPE channel layered on top of the color one: `NeedsInput` already
/// colors its badge `Yellow`, but a shape survives a monochrome terminal or a
/// color-blind reader that a yellow-only signal does not. The choice is by bucket
/// and therefore STABLE across pulse phases: `NeedsInput` is steady, so its `!` is
/// drawn identically in both phases, and the pulse continues to change only the
/// badge's COLOR, never its symbol (see [`pulse_color`]).
#[must_use]
fn badge_glyph(agent: &ReportedAgent) -> &'static str {
    if agents::classify(agent) == AgentActivity::NeedsInput {
        BADGE_NEEDS_INPUT
    } else {
        BADGE_DOT
    }
}

/// The BASE color of a reported agent's badge — the `bg`/`live` kind label, the
/// qualifier phrase, and (via [`badge_glyph_color`]) the `●` dot.
///
/// The dot and the label agree on this color for every bucket EXCEPT
/// [`AgentActivity::NeedsInput`], whose `!` glyph diverges to the
/// [`BADGE_NEEDS_INPUT_COLOR`] red accent while its label and qualifier keep this
/// yellow — see [`badge_glyph_color`]. The divergence is one glyph cell, on
/// purpose.
///
/// Pure, and derived from [`crate::agents::classify`] like every other
/// qualifier-shaped output, so the `state`/`status` value set is never re-matched
/// here — this maps from the BUCKET, not from the raw wire strings. The palette
/// reads as urgency: YELLOW = needs you, GREEN = ready (idle or finished), GRAY =
/// quietly working.
///
/// Color is exactly what marks activity: an ACTIVE bucket's dot alternates
/// between this base and [`pulse_color`]'s dim partner, while a resting bucket's
/// holds this base in both phases (see [`crate::agents::is_active`]). The kind
/// label never pulses, so it always carries this base — which is what keeps the
/// state readable through the dot's off phase.
///
/// Lives here rather than beside `classify` because it is the only
/// qualifier-derived output that is a RENDERING decision: keeping it in `agents`
/// would drag ratatui into the fail-soft JSONL parser layer, which stays
/// framework-independent.
///
/// TERMINAL-SAFE STYLING: these are NAMED ANSI colors, never RGB, so they adapt
/// to the user's terminal theme and survive a light background.
/// [`BADGE_WORKING`] is `Gray` rather than `DarkGray` to keep the working badge
/// legible on dark terminals; `DarkGray` is its PULSE partner, not its resting
/// color (see [`pulse_color`]).
///
/// `pub(crate)` only so [`AgentActivity`]'s docs can point at the palette their
/// buckets feed; `render_list` (the label/qualifier) and [`badge_glyph_color`]
/// (the dot) are its callers.
#[must_use]
pub(crate) fn badge_color(agent: &ReportedAgent) -> Color {
    match agents::classify(agent) {
        AgentActivity::NeedsInput => Color::Yellow,
        // Green reads "nothing is wanted from you": idle is ready to take a turn,
        // done has finished cleanly. Both are steady, so green never has to carry
        // the activity signal on its own.
        AgentActivity::Idle | AgentActivity::Done => Color::Green,
        // A terminal (stopped/failed) job: dim and steady, and NOT green — it
        // ended, so it must not read as a clean finish. See [`BADGE_ENDED`].
        AgentActivity::Ended => BADGE_ENDED,
        // Gray is the working base, and the interrupted bucket shares it: it IS a
        // working `state`, only one claude's own `status` contradicts. It renders
        // the same gray but STEADY (see `is_active`), so the missing pulse — not a
        // second color — is what tells it apart from a genuinely churning agent.
        // That is the split from `Ended` above: same rest, different cause, so it
        // keeps the working gray instead of dimming to `BADGE_ENDED`.
        AgentActivity::Working | AgentActivity::WorkingButIdle | AgentActivity::Other => {
            BADGE_WORKING
        }
    }
}

/// The color of a reported agent's badge GLYPH (the `●`/`!` cell).
///
/// Equal to [`badge_color`] for every bucket EXCEPT
/// [`AgentActivity::NeedsInput`], whose `!` marker ([`badge_glyph`]) diverges to
/// the [`BADGE_NEEDS_INPUT_COLOR`] red accent. The kind label and qualifier keep
/// [`badge_color`]'s yellow, so ONLY this one glyph cell reddens — an accent that
/// lifts the one bucket that wants the user above the palette, layered on top of
/// the shape channel the `!` already provides, without turning the row into an
/// alarm.
///
/// Pure and derived from [`crate::agents::classify`] — the BUCKET — so it can
/// never disagree with [`badge_glyph`] about which bucket earns the accent.
/// `NeedsInput` is steady ([`crate::agents::is_active`] is false for it), so the
/// red never pulses; every other bucket returns exactly [`badge_color`], so the
/// pulse and the resting palette are untouched.
#[must_use]
fn badge_glyph_color(agent: &ReportedAgent) -> Color {
    if agents::classify(agent) == AgentActivity::NeedsInput {
        BADGE_NEEDS_INPUT_COLOR
    } else {
        badge_color(agent)
    }
}

/// The dim partner a PULSING dot alternates to, given its [`badge_color`] base.
///
/// **The pulse changes a cell's STYLE and NEVER its SYMBOL.** That is the whole
/// reason this function exists. The dot used to pulse by swapping its glyph for a
/// blank, which MUTATES the row's text; we emit plain-text URLs (no OSC 8), so
/// the terminal auto-detects links by TEXT PATTERN and a mutated line forces it
/// to re-scan and re-render that line's URL underline — a session label carrying
/// a URL visibly flickered every phase. A style-only change leaves the text
/// identical, so there is nothing to re-detect. Do NOT "optimize" this back to a
/// blank span.
///
/// Pure, and the ONE place a bucket's dim partner is declared: a future bucket
/// that pulses off a different base adds its arm here, next to the pair it dims.
///
/// Both sides are NAMED ANSI colors, never RGB and never `Modifier::DIM`:
/// attribute support is inconsistent across terminals, which is exactly how the
/// ANSI blink attribute shipped inert (see [`blink_visible`]). A named color
/// always renders.
///
/// The fallback is IDENTITY — FAIL-SOFT, so an undeclared base renders steady
/// rather than panicking or guessing at a dim value it cannot know is legible.
/// That is deliberate, but it IS a trap: a future PULSING bucket whose base has
/// no arm above would silently stop pulsing, green-but-broken. The exhaustive
/// bucket walk in `every_pulsing_buckets_badge_color_has_a_distinct_dim_partner`
/// is what turns that silence into a loud test failure.
#[must_use]
fn pulse_color(base: Color) -> Color {
    match base {
        BADGE_WORKING => BADGE_WORKING_DIM,
        // FAIL-SOFT identity — and the trap the walk above pins shut.
        other => other,
    }
}

/// Whether the board's pulse is in its ON phase at `tick`.
///
/// The ONE phase source on the board: the live badge's dot and the search line's
/// cursor both read it, so they move together instead of drifting. What each
/// side DOES with the phase differs, and deliberately so — the dot swaps color
/// ([`pulse_color`]), while the cursor shows/hides ([`SEARCH_CURSOR_HIDDEN`]).
/// The name is the cursor's literal reading and the dot's ON/OFF phase; anything
/// animated later phases off this too.
///
/// Pure, so the pulse's timing is unit-testable without a terminal or a clock:
/// `tick` is just the count of `AppEvent::Tick`s so far ([`App::tick`]), which
/// advances at the [`crate::watch::TICK`] cadence the render loop ALREADY
/// redraws on. Each phase runs [`BLINK_TICKS`] ticks, so ticks 0-1 are ON, 2-3
/// OFF, 4-5 ON, and so on.
///
/// This is the pulse's whole mechanism, and it is app-driven ON PURPOSE. The
/// obvious alternative — style the dot with the ANSI blink attribute (SGR 5,
/// ratatui's slow-blink `Modifier`) and let the terminal animate it — DOES NOT
/// WORK: most modern terminals (iTerm2, Ghostty, WezTerm, Alacritty, macOS
/// Terminal) ignore that attribute, so the dot renders steady and the feature is
/// silently dropped. It was tried, and it is why this function exists; do not
/// "simplify" back to it. That same inconsistency is why the dot's OFF phase is a
/// named color rather than `Modifier::DIM`.
///
/// A wrapping `tick` is harmless here: one full cycle is `2 * BLINK_TICKS`
/// ticks, and `u64::MAX + 1` is a power of two and therefore a whole number of
/// cycles, so the phase stays aligned across the rollover.
#[must_use]
fn blink_visible(tick: u64) -> bool {
    // Which phase of the cycle `tick` falls in: the 2 is the cycle's phase count
    // (shown, then hidden), so the even phases are the shown ones.
    (tick / BLINK_TICKS).is_multiple_of(2)
}

/// The status banner line for the SELECTED session, or `None` when claude never
/// reported that session as an agent (the preview then renders unchanged).
///
/// Read-only over state that already exists: the selected id (`App::selected`)
/// joined through the existing `App::reported_agent` accessor, with the phrasing
/// owned by [`agents::friendly_status`] — no new `App` state, no new I/O, and no
/// second interpretation of the `state`/`status` value set.
///
/// Keyed on REPORTED, not live, so a FINISHED agent still gets its banner (`bg
/// done`) rather than silently losing it.
///
/// Exposed to `super::update` so the link hit-test can ask the SAME question the
/// view does — "does this session have a banner?" — and derive the same
/// transcript rect via [`preview_split`]; the two must agree, or a click would
/// resolve to the wrong transcript row.
///
/// An IN-FLIGHT quick-reply send takes precedence: while `App::sending` names the
/// selected session there is NO pinned banner at all — this returns `None`, and
/// the `cooking…` placeholder renders INLINE at the transcript's tail instead
/// ([`sending_tail`]), so the exchange reads as ordinary turns. Returning `None`
/// is also what keeps the render and the click hit-test agreeing on the geometry.
pub(crate) fn preview_banner(app: &App) -> Option<Line<'static>> {
    // A NEW-SESSION draft owns the pane: the card replaces the transcript, so the
    // SELECTED session's status line has nothing left to sit above and would only
    // describe a session the user is no longer looking at. The click hit-test asks
    // THIS fn for the geometry, so returning `None` keeps render and hit-test
    // agreeing that no banner row is reserved (same contract as the in-flight send
    // below).
    if app.draft.is_some() {
        return None;
    }
    let selected = app.selected.as_deref()?;
    // A quick-reply in flight owns the preview: the message and the
    // `cooking…` placeholder render INLINE at the transcript's tail
    // ([`sending_tail`] / [`preview::pending_reply_turns`]), so there is no pinned
    // banner row. The click hit-test asks THIS fn for the geometry, so returning
    // `None` here keeps render and hit-test agreeing that no banner is drawn.
    if app.sending_to(selected).is_some() {
        return None;
    }
    let agent = app.reported_agent(selected)?;
    // Cyan + BOLD marks the line as the board speaking rather than transcript
    // content (the search prompt uses the same accent). NAMED so it adapts to
    // the terminal theme — no RGB (TERMINAL-SAFE STYLING).
    Some(Line::from(Span::styled(
        agents::friendly_status(agent),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )))
}

/// Braille spinner frames for the in-flight send indicator. Plain glyphs (no ANSI),
/// consistent with the board's other unicode marks; one advances per redraw tick.
const SPINNER_FRAMES: [&str; 10] = [
    "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}", "\u{2827}",
    "\u{2807}", "\u{280f}",
];

/// The spinner glyph for the current `tick` (advances ~every [`crate::watch::TICK`]).
fn spinner_frame(tick: u64) -> &'static str {
    SPINNER_FRAMES[(tick as usize) % SPINNER_FRAMES.len()]
}

/// The optimistic reply turns appended to the transcript of the CURRENTLY selected
/// session while a quick-reply send to it is in flight, or `None` when no send is
/// in flight for the selected row.
///
/// So the reply feels instant, the message you just sent shows immediately under a
/// synthetic `▶ you` turn, followed by a live `● claude` **cooking…** placeholder —
/// INLINE in the transcript flow (see [`preview::pending_reply_turns`]).
///
/// The placeholder says one true thing only: it is claude's pending turn. The old
/// two-phase "sending… / cooking…" wording derived from a ≤5s-stale agents poll,
/// nothing branched on the distinction, and "sending" named what snapback did, not
/// what claude was doing — so it collapsed to a single `cooking…` label.
///
/// The `▶ you` echo is dropped the instant claude writes the REAL user turn to
/// disk — detected by the session's turn count growing past the count captured at
/// send time ([`super::app::Sending::baseline_msg_count`]) — so the real turn (which
/// arrives via the ordinary watcher → reload path and is styled identically) simply
/// takes its place, never doubling the line. The `● claude` placeholder stays until
/// the send completes and [`App::sending`] is cleared.
fn sending_tail(app: &App, inner_width: u16) -> Option<Vec<Line<'static>>> {
    let selected = app.selected.as_deref()?;
    let sending = app.sending_to(selected)?;
    // Once claude has written the real user turn, the session's turn count exceeds
    // the send-time baseline; until then, echo the message so it is visible during
    // the disk-write latency.
    let landed = app
        .session_by_id(selected)
        .is_some_and(|s| s.msg_count > sending.baseline_msg_count);
    let echo = (!landed).then_some(sending.message.as_str());
    // One label only: a `● claude` turn must describe what claude is doing. The
    // old two-phase wording derived from a ≤5s-stale agents poll, nothing branched
    // on it, and "sending" named what snapback did, not claude.
    let label = format!("{} {REPLY_COOKING_LABEL}", spinner_frame(app.tick));
    Some(preview::pending_reply_turns(
        echo,
        &label,
        usize::from(inner_width),
    ))
}

/// The preview pane's INNER rect — inside the block's borders, which steal one cell
/// per side (this mirrors `Block::inner` for `Borders::ALL`).
///
/// The ONE place the pane's border inset is applied, so every rect carved out of the
/// pane — the banner row, the transcript, and the DOCKED compose zone — is measured
/// from the same origin and the same width. That matters most for the compose zone:
/// it is drawn INSIDE this rect and then draws a border of its OWN, so anything that
/// re-derived its geometry from the pane's outer `area` would size it two columns too
/// wide, and a wrapping draft would under-grow.
fn preview_inner(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

/// Split the preview pane's `area` into its `(banner, transcript)` rects.
///
/// The pane's inner area ([`preview_inner`]) is divided into a
/// PINNED banner row and the scrolling transcript beneath it. `has_banner` is
/// [`preview_banner`]`(..).is_some()`, i.e. "claude REPORTED the selected
/// session as an agent" — NOT "the selected session is live". The two parted
/// ways when the shell-out grew `--all`: an agent that reported completion is
/// still reported, so it has a banner, while claude would not call it live.
/// Passing liveness here would desync this geometry from [`super::update`]'s
/// hit-test (which asks [`preview_banner`]) for every `done` session — banner
/// drawn, clicks resolved one row off. It is also unaskable here: liveness now
/// means a shell-out to claude ([`App::is_live_now`]), which a render must never
/// do.
///
/// The banner is a dedicated LAYOUT row rather than a line prepended into the
/// scrolled `Text` because the preview is bottom-anchored by DEFAULT
/// (`App::preview_follow_bottom`, re-armed on every selection change): a
/// prepended line is pinned off the top of the viewport for any transcript
/// taller than the pane — which is every realistic reported session — leaving
/// the banner reachable only via `Home`. As its own row it stays put while the
/// transcript scrolls beneath it.
///
/// When `has_banner` is false the transcript IS the whole inner rect and the
/// banner rect is empty, so a BANNER-LESS session's geometry is exactly what it
/// was before the banner existed.
///
/// Pure, and the ONE place this geometry is derived: `render_preview` draws
/// against these rects and [`super::update`]'s link hit-test resolves clicks
/// against the same transcript rect, so the scroll offset and the cached line
/// widths are measured from the same origin the text was drawn at.
pub(crate) fn preview_split(area: Rect, has_banner: bool) -> (Rect, Rect) {
    let inner = preview_inner(area);
    if !has_banner {
        return (Rect::default(), inner);
    }
    // A pane too short to hold both degrades to a banner-only view rather than
    // overlapping the two: `min` keeps the reservation inside the pane and the
    // transcript collapses to zero rows.
    let banner_h = inner.height.min(PREVIEW_BANNER_ROWS);
    let banner = Rect {
        height: banner_h,
        ..inner
    };
    let transcript = Rect {
        y: inner.y.saturating_add(banner_h),
        height: inner.height.saturating_sub(banner_h),
        ..inner
    };
    (banner, transcript)
}

/// The preview pane's INNER height for a board `board_height` rows tall, in the
/// NORMAL (no bottom-bar) layout: the body is the board minus [`BOARD_CHROME_ROWS`],
/// and the preview block steals two rows of border. Pure so the placement decision
/// is unit-testable without laying out a frame.
fn preview_pane_inner_height(board_height: u16) -> u16 {
    board_height
        .saturating_sub(BOARD_CHROME_ROWS)
        .saturating_sub(2) // preview block top + bottom border
}

/// Whether the compose zone must render as a FULL-WIDTH BOTTOM BAR rather than
/// docking in the preview pane — only when composing AND the preview pane is too
/// short to hold the banner, a usable transcript, and the compose zone together
/// ([`COMPOSE_MIN_DOCK_HEIGHT`]). Pure and unit-testable; the renderer's own dock
/// check ([`render_preview`]) mirrors it against the (possibly shorter) pane area,
/// so the two never disagree.
fn compose_uses_bottom_bar(composing: bool, board_height: u16) -> bool {
    composing && preview_pane_inner_height(board_height) < COMPOSE_MIN_DOCK_HEIGHT
}

/// Split the preview pane's `area` into `(banner, transcript, compose)` rects.
///
/// Reuses [`preview_split`] for the banner + transcript, then carves the bottom
/// `compose_height` rows off the transcript for the docked compose zone.
/// `compose_height == 0` means NOT docking: the compose rect is empty and the
/// transcript is the full [`preview_split`] rect, so a non-composing pane is
/// byte-identical to before this feature. Pure so the geometry is unit-testable.
fn preview_compose_split(area: Rect, has_banner: bool, compose_height: u16) -> (Rect, Rect, Rect) {
    let (banner, transcript) = preview_split(area, has_banner);
    if compose_height == 0 {
        return (banner, transcript, Rect::default());
    }
    // Degrade gracefully: `min` keeps the reservation inside the transcript, so a
    // pane that only just clears the dock threshold collapses the transcript to
    // zero rows rather than overlapping the two.
    let compose_h = transcript.height.min(compose_height);
    let shrunk = Rect {
        height: transcript.height.saturating_sub(compose_h),
        ..transcript
    };
    let compose = Rect {
        y: shrunk.y.saturating_add(shrunk.height),
        height: compose_h,
        ..transcript
    };
    (banner, shrunk, compose)
}

/// The draft pane's title when the picked agent is the "default (no agent)" row —
/// named rather than inlined so the one place that phrase appears is greppable
/// against the picker row it mirrors.
const BG_DRAFT_DEFAULT_AGENT: &str = "default agent";

/// The draft card's headline prefix. Says "new session" rather than naming a row,
/// because there is no row: the session does not exist yet.
const DRAFT_CARD_HEADLINE: &str = "new session";

/// The separator between the draft card's headline and the agent it will run as —
/// the same middot the board uses between header facts, so the card reads as the
/// board speaking rather than as transcript content.
const DRAFT_CARD_SEPARATOR: &str = " \u{b7} ";

/// The draft card's in-flight line, shown after `Enter` while `claude --bg` runs.
/// Prefixed by the shared [`spinner_frame`], so it animates off the board's own
/// tick — no second cadence (PATTERNS §7).
const DRAFT_CARD_LAUNCHING: &str = "starting in the background\u{2026}";

/// The placeholder under a `● claude` turn while a quick-reply send is in flight.
///
/// It names what **claude** is doing on its pending turn, never what snapback did:
/// "sending" would describe snapback's dispatch, which is already represented by the
/// `▶ you` echo directly above. The word is therefore "cooking" and nothing else.
const REPLY_COOKING_LABEL: &str = "cooking\u{2026}";

/// The compose hints of a BACKGROUND draft. One const, two surfaces: the help line
/// ([`compose_hint`]) and the draft card, which must not restate them differently.
///
/// It deliberately does NOT carry the reply arm's "paste keeps newlines" clause,
/// on COLUMN BUDGET alone — a pasted newline was every bit as destructive here (see
/// [`compose_hint`] for the measurement). This string is already 97 columns, so on
/// the one-line help row the clause would be painted past the end of an 80-column
/// terminal, and on the draft card — which wraps rather than clipping — it would
/// cost a further wrapped row of a placeholder whose whole point is to stay
/// near-empty.
const BG_DRAFT_HINT: &str = "Enter start in background · Ctrl-O run interactively · \
                             Ctrl-J newline (or Alt+Enter) · Esc cancel";

/// The draft card's agent segment: `@handle` for a picked agent, or the picker's
/// own [`BG_DRAFT_DEFAULT_AGENT`] wording for its default row (a bare `@` would be
/// a handle that does not exist). A blank/whitespace name degrades to the default
/// rather than rendering an empty `@`. Pure so the wording is unit-testable.
fn draft_agent_label(agent: Option<&str>) -> String {
    match agent.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => format!("@{name}"),
        None => BG_DRAFT_DEFAULT_AGENT.to_string(),
    }
}

/// The PLACEHOLDER card the preview pane shows while a new-session draft is open,
/// in place of the selected session's transcript.
///
/// It is deliberately near-EMPTY, and that is the whole point: it stands for a
/// session that does not exist yet, so anything resembling a conversation would be
/// a lie. Three facts and nothing else — what is being started, WHERE it will run
/// (the launch dir is the one thing a new session commits to that the user cannot
/// otherwise see), and the keys that act on it. Once dispatched, the key hints give
/// way to the in-flight line, since none of those keys still apply.
///
/// Pure (`(&NewSessionDraft, &Path, tick) -> Vec<Line>`), so the card's content is
/// assertable without a terminal. Styled with NAMED colors + modifiers only
/// (TERMINAL-SAFE STYLING).
fn draft_card(draft: &NewSessionDraft, launch_dir: &Path, tick: u64) -> Vec<Line<'static>> {
    let headline = Line::from(vec![
        Span::styled(
            DRAFT_CARD_HEADLINE,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(DRAFT_CARD_SEPARATOR),
        Span::styled(
            draft_agent_label(draft.agent.as_deref()),
            Style::default().fg(Color::Green),
        ),
    ]);
    let dir = Line::from(Span::styled(
        launch_dir.to_string_lossy().into_owned(),
        Style::default().add_modifier(Modifier::DIM),
    ));
    let tail = if draft.is_launching() {
        Line::from(Span::styled(
            format!("{} {DRAFT_CARD_LAUNCHING}", spinner_frame(tick)),
            Style::default().fg(Color::Yellow),
        ))
    } else {
        Line::from(Span::styled(
            BG_DRAFT_HINT,
            Style::default().add_modifier(Modifier::DIM),
        ))
    };
    vec![headline, dir, Line::default(), tail]
}

/// The compose zone's block title for the open draft.
///
/// A REPLY names the TARGET session's label, so the recipient is unambiguous even
/// if the previewed row was scrolled away from it; stop-then-reply mode (a held bg
/// agent) says so, since sending stops the agent first and the title must never
/// imply a plain in-place reply. A BACKGROUND draft names the picked agent instead
/// — there is no session yet to name — and says "background", because `Enter`
/// there starts an agent rather than answering one.
///
/// Pure (a `(&App, &ComposeState) -> String` map) so the wording is assertable
/// without a terminal.
fn compose_title(app: &App, compose: &ComposeState) -> String {
    match &compose.target {
        ComposeTarget::Reply {
            session_id,
            stop_job,
        } => {
            let label = app
                .session_by_id(session_id)
                .map(|s| s.label.as_str())
                .filter(|l| !l.is_empty())
                .unwrap_or(session_id.as_str());
            if stop_job.is_some() {
                format!(" stop & reply to {label} ")
            } else {
                format!(" reply to {label} ")
            }
        }
        ComposeTarget::NewBackgroundAgent { agent } => {
            let name = agent
                .as_deref()
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .unwrap_or(BG_DRAFT_DEFAULT_AGENT);
            format!(" new background agent: {name} ")
        }
    }
}

/// Render the compose zone (a bordered multiline editor) into `area`, titled by
/// [`compose_title`] for whichever draft is open.
///
/// Styled ONLY with ratatui `Style` + NAMED colors (TERMINAL-SAFE STYLING): the
/// cyan border marks the box as the board speaking, like the search prompt and the
/// status banner. The `TextArea` widget draws its own buffer and cursor into the
/// block's inner rect — the one place a `ratatui_textarea` value is rendered,
/// mirroring how it is the one place one is edited (see [`super::compose`]).
fn render_compose_zone(frame: &mut Frame, app: &App, area: Rect) {
    let Some(compose) = &app.compose else {
        return;
    };
    let title = compose_title(app, compose);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(&compose.textarea, inner);
}

/// The readable transcript preview for the selected session, vertically
/// scrollable and anchored to the newest turn by default, under a REPORTED
/// session's PINNED status banner (see [`preview_split`]).
///
/// The scroll offset lives in `App` but its bounds are only known here (the
/// transcript's width/height and the wrapped content height), so — mirroring how
/// `render_list` writes back `app.scroll` — this clamps the offset against the
/// wrapped content and writes both the resolved offset and the viewport height
/// back into `App`. Those are the TRANSCRIPT's bounds, not the pane's: a pinned
/// banner costs the scrollable area one row, so a page key sizes a page from
/// what actually scrolls.
///
/// A vertical scrollbar is drawn over the block's own right border (the
/// idiomatic ratatui composition: the `Scrollbar` widget is rendered as a
/// SEPARATE pass over the transcript's rows at full pane width, so its track
/// lands exactly on the border column rather than stealing a content column)
/// whenever the wrapped content overflows the viewport. When the content fits
/// entirely (`content_h <= inner_height`), the scrollbar is skipped entirely —
/// there is nothing to scroll, so no thumb is drawn — rather than rendering a
/// full-length/inactive thumb, keeping "a scrollbar is visible" a reliable
/// signal that there is more transcript to see.
fn render_preview(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" preview ");

    // A REPORTED session leads with a status banner so the user can see WHY it
    // is stopped (or that it is working, or that it has finished) without
    // decoding the row badge. It is PINNED as its own layout row (see
    // `preview_split`) — the transcript scrolls beneath it — so the default
    // bottom-anchored viewport cannot scroll it away. A session claude never
    // reported reserves no row and renders unchanged.
    let banner = preview_banner(app);
    // Dock the compose zone in the bottom of the pane when composing AND the pane
    // is tall enough; otherwise `render` gave compose a full-width bottom bar and
    // the transcript keeps the whole pane. Mirrors `compose_uses_bottom_bar`,
    // evaluated against THIS pane's height (which is already the shorter body in the
    // bottom-bar layout, so the two agree).
    let dock_compose = app.is_composing() && preview_inner(area).height >= COMPOSE_MIN_DOCK_HEIGHT;
    // The docked zone grows with the draft (0 = not docking). Its WIDTH is not a
    // parameter here: `preview_compose_split` carves it out of `preview_inner`, and
    // the box's height is read off the editor itself, which knows the width it was
    // last drawn at — so there is no second width to get wrong.
    let compose_h = if dock_compose {
        compose_zone_height(app)
    } else {
        0
    };
    let (banner_area, transcript_area, compose_area) =
        preview_compose_split(area, banner.is_some(), compose_h);
    // The transcript's width is also the table shrink-to-fit budget, so it must
    // be resolved BEFORE rendering the preview text (which fits GFM tables to
    // it). The banner split is vertical only, so this width — and therefore the
    // width-scoped preview cache — is the same with or without a banner.
    let inner_width = transcript_area.width;
    let inner_height = transcript_area.height;

    // A NEW-SESSION draft REPLACES the transcript with a placeholder card. This is
    // the whole separation `App::draft` exists for: the pane asks the draft what to
    // show and never inspects the compose target, so a docked compose box is never
    // drawn over an unrelated conversation (which reads as a reply to it). It also
    // outlives the editor for one in-flight launch, which is why the card — not
    // `is_composing` — is what this branches on.
    let card = app
        .draft
        .as_ref()
        .map(|draft| draft_card(draft, &app.launch_dir, app.tick));
    let showing_card = card.is_some();

    // Optimistic reply turns for an in-flight send, resolved BEFORE the mutable
    // preview borrow so the message you just sent shows immediately at the tail.
    // Suppressed under a card: the echo belongs to the SELECTED session's
    // transcript, which is not what the pane is showing.
    let reply_tail = if card.is_some() {
        None
    } else {
        sending_tail(app, inner_width)
    };
    // Both of the things that are NOT the cached transcript are measured HERE, into
    // the SAME kind of prefix map the cache holds for the transcript, because the
    // cache knows about neither: a draft CARD replaces the transcript outright, and
    // the echo turns of an in-flight reply exist only for the seconds a send is
    // running. Both are a handful of lines, so measuring them per frame is free.
    //
    // Adding their rows to the cached transcript's is exact — `WordWrapper` wraps
    // each logical line on its own and never joins two of them onto one row, so a
    // wrapped row count is additive over lines.
    let card_prefix = card
        .as_ref()
        .map(|lines| wrapped_row_prefix(lines, inner_width));
    let tail_prefix = reply_tail
        .as_ref()
        .map(|tail| wrapped_row_prefix(tail, inner_width));
    let tail_rows = tail_prefix
        .as_ref()
        .map_or(0, |prefix| prefix.last().copied().unwrap_or(0));
    // Whether the pane is still anchored to the newest row — `App::preview_follow_bottom`
    // and nothing else, because that field is the ONE answer to "is this pane still
    // anchored, or did the reader position it?" (see its doc comment for the full set
    // of transitions).
    //
    // An in-flight reply follows the tail THROUGH that anchor rather than around it:
    // a fresh selection arms it and `End` re-arms it, so the ordinary reply still
    // streams into view with no keypress. It must not be ORed in here. `reply_tail`
    // is `Some` for the WHOLE duration of a send and this runs on every frame — the
    // spinner redraws each tick — so an OR re-asserted the anchor on every one of
    // those frames and snapped the pane back to `max_offset` one frame after the
    // reader (or the match jump below) had positioned it.
    //
    // The card anchors to the TOP instead: it is short, and its headline is the
    // first thing to read.
    let follow_bottom = card.is_none() && app.preview_follow_bottom;

    // Take the pending match jump ABOVE the early return below, so EVERY path out
    // of this function consumes it. It is a one-shot describing the pane as it was
    // when a key was pressed; a path that leaves it armed defers it onto an
    // unrelated later frame instead of dropping it (see `App::take_preview_match_jump`).
    let pending_jump = app.take_preview_match_jump();

    // How many LOGICAL lines the cached transcript holds. Asked instead of its
    // wrapped height for the emptiness test alone: at a degenerate zero-width pane
    // every line wraps to zero rows, and a real transcript would read as absent.
    // Also the call that WARMS the cache for this (session, width), which
    // `preview_match_target` below reads without being able to fill.
    let transcript_lines = app.preview_line_count(inner_width);

    // Nothing selected (no text AND no banner, since a banner implies a SELECTED
    // session claude reported). A reported session whose transcript is still
    // empty falls through instead: its banner is the one thing worth drawing, and
    // keeping the banner unconditional is what lets the hit-test below derive the
    // same geometry from `banner.is_some()` alone.
    let nothing_to_draw = !showing_card && reply_tail.is_none() && transcript_lines == 0;
    if nothing_to_draw && banner.is_none() && !dock_compose {
        // Keep the scroll bookkeeping sane and still record the viewport height
        // so a later selection can size a page.
        app.preview_viewport_h = inner_height;
        app.preview_scroll = 0;
        frame.render_widget(
            Paragraph::new("No session selected.")
                .style(Style::default().add_modifier(Modifier::DIM))
                .block(block),
            area,
        );
        return;
    }

    // The block is drawn as its OWN pass instead of via `Paragraph::block` so the
    // pinned banner and the scrolling transcript can occupy separate rects inside
    // one border. For a banner-less session this paints exactly what
    // `Paragraph::new(text).block(block)` painted: `preview_split`'s inner rect is
    // `Block::inner` for `Borders::ALL`, and the paragraph's own style is default.
    frame.render_widget(block, area);
    if let Some(banner) = banner {
        // Deliberately NOT wrapped: a pinned row cannot grow, so an over-long
        // banner truncates at the pane edge rather than silently stealing a
        // transcript row and desyncing the hit-test's geometry.
        frame.render_widget(Paragraph::new(banner), banner_area);
    }

    // The wrapped height of what this pane is ACTUALLY showing — the WHOLE of it,
    // window or no window, because this is what the scroll clamp, the bottom anchor
    // and the scrollbar all describe. The transcript's own count is the last entry
    // of the prefix map CACHED per session at this width; the two things that are
    // NOT that cached transcript were measured above, and both would otherwise be
    // MIS-counted:
    //   - a draft CARD replaces the transcript outright, so the cached count would
    //     describe text that is not on screen — and being far too tall, it would let
    //     a leftover scroll offset survive the clamp and push the short card out of
    //     view entirely;
    //   - an in-flight reply TAIL is appended after the cache was filled, so the
    //     cached count alone under-counts it (`tail_rows`, resolved above).
    let transcript_rows = app.preview_wrapped_rows(inner_width);
    let content_h = match &card_prefix {
        Some(prefix) => prefix.last().copied().unwrap_or(0),
        None => transcript_rows + tail_rows,
    };
    // Resolve the pending match jump HERE, at the one site that knows the pane's
    // width and height — the two things the offset is a function of — and that
    // already writes the resolved scroll state back. The row the matched line starts
    // on is READ OFF the cached prefix map rather than re-measured: that map was
    // built with the SAME wrapper that paints the pane, so `row_prefix[line]` IS the
    // matched line's first screen row — where re-wrapping the transcript's whole
    // prefix on every keypress used to clone every line above the target to ask the
    // same question. An approximate character-packing model is wrong in both
    // directions and would park the match somewhere else entirely.
    //
    // Already taken above, acted on only for a transcript: under a draft CARD the
    // matched line indices address text that is not on screen (the card replaced
    // it), so the request is DROPPED rather than deferred onto whatever frame
    // follows the card.
    let jump = if pending_jump && !showing_card {
        let target = app.preview_match_target();
        target
            .and_then(|line| app.preview_rows_above(inner_width, line))
            .map(|rows_above| match_jump_offset(rows_above, inner_height))
    } else {
        None
    };
    // A jump overrides the bottom anchor — that is the whole request — and says so
    // in `App` too, or the next frame would re-anchor and undo it. The jump is a
    // ONE-SHOT and this function runs many times per second, so the override has to
    // live in state that OUTLASTS the frame; `preview_follow_bottom` is that state
    // and the reader takes it back the ordinary ways (scroll, another row, `End`).
    let follow_bottom = follow_bottom && jump.is_none();
    let offset = clamp_preview_offset(
        follow_bottom,
        jump.unwrap_or(app.preview_scroll),
        content_h,
        inner_height,
    );
    if jump.is_some() {
        app.preview_follow_bottom = false;
    }
    // Persist the resolved geometry so the scroll keys stay in bounds and can
    // size a page on the next keypress — EXCEPT under a draft card, which is not
    // the transcript that offset describes. The card is a handful of lines, so on
    // any ordinary pane it clamps every offset to 0; writing that back would rewind
    // the session behind the draft to the top and hand it back there when the draft
    // is cancelled, losing the position the user was reading. The card still RENDERS
    // from `offset` — measured against the CARD (see `content_h` above), so it is 0
    // unless the card itself overflows a very narrow pane — only the write-back is
    // skipped, so `preview_scroll` keeps describing the transcript throughout.
    if !showing_card {
        app.preview_scroll = offset;
    }
    app.preview_viewport_h = inner_height;

    // --- the windowed draw ---------------------------------------------------
    //
    // The widget is handed ONLY the logical lines this viewport can reach, and is
    // scrolled by the RESIDUAL — the rows to skip inside the first of them — rather
    // than by the pane's absolute offset. `Paragraph` re-wraps every line it is
    // given on every frame, so handing it the whole transcript spent that work on
    // rows nobody could see: measured at 18.2 ms per frame at the bottom of a
    // 16,000-line transcript against a flat ~0.06 ms windowed, independent of length.
    //
    // It also retires the one place `App::preview_scroll`'s `u32` had to narrow.
    // `Paragraph::scroll` takes a `Position { x: u16, y: u16 }` — a ratatui
    // constraint, not a choice this module gets to make — and an absolute offset
    // past `u16::MAX` used to clip there, kept out of reach only by a tail cap on
    // the transcript that no longer exists. The residual is bounded by ONE logical
    // line's wrapped height instead of by the transcript's, so the conversion below
    // survives a transcript of any length; it stays saturating rather than a bare
    // `as` so that even a single line wrapping past 65,535 rows (a pathological
    // paste into a one-column pane) parks on the last row the widget can address
    // instead of wrapping back to the top.
    let offset_rows = usize::try_from(offset).unwrap_or(usize::MAX);
    // `card` and `card_prefix` are built together and travel together, so they are
    // taken together — there is no state where the pane has a card but no map of it.
    let (mut window, mut residual) =
        if let Some((lines, prefix)) = card.as_ref().zip(card_prefix.as_ref()) {
            // A draft CARD replaces the transcript, so it is windowed as itself. It
            // was never in the cache, and its marks were never computed — a
            // placeholder for a session that does not exist yet has no transcript to
            // have matched.
            let (range, residual) = row_window(prefix, offset_rows, inner_height);
            (lines[range].to_vec(), residual)
        } else {
            let window = app.preview_window(inner_width, offset_rows, inner_height);
            let mut lines = window.lines;
            // Mark the query's occurrences INSIDE the transcript — the content-search
            // counterpart of the row-label highlight, derived by re-searching the
            // rendered lines rather than by projecting a position out of
            // `content_index` (see `App::preview_matches`). Only the WINDOW is
            // marked, since only the window is drawn.
            //
            // The map is keyed to the WHOLE transcript, so each line is looked up at
            // its ABSOLUTE index — `window.start` back-added. Dropping that term
            // marks real occurrences onto the wrong words whenever the pane is
            // scrolled, and nothing on screen says so.
            if let Some(matches) = app.preview_matches(inner_width) {
                for (i, line) in lines.iter_mut().enumerate() {
                    if let Some(matched) = matches.get(&(window.start + i)) {
                        *line = highlight_matched_spans(line, matched, PREVIEW_MATCH_MODIFIER);
                    }
                }
            }
            (lines, window.residual)
        };
    // The optimistic reply turns sit BELOW the transcript, so they join the window
    // only once the viewport reaches them — and when the viewport has scrolled
    // PAST the transcript entirely, they carry the residual too, since the window's
    // first line is then one of theirs. They are never marked: they are not in the
    // cache the match map describes.
    if let (Some(tail), Some(prefix)) = (&reply_tail, &tail_prefix) {
        if offset_rows.saturating_add(usize::from(inner_height)) > transcript_rows {
            let (range, tail_residual) = row_window(
                prefix,
                offset_rows.saturating_sub(transcript_rows),
                inner_height,
            );
            if window.is_empty() {
                residual = tail_residual;
            }
            window.extend_from_slice(&tail[range]);
        }
    }
    let widget_offset = u16::try_from(residual).unwrap_or(u16::MAX);
    frame.render_widget(
        Paragraph::new(Text::from(window))
            .wrap(Wrap { trim: false })
            .scroll((widget_offset, 0)),
        transcript_area,
    );

    if content_h > usize::from(inner_height) {
        // Every number below describes the WHOLE transcript, never the window the
        // widget was just handed — which is the point of a scrollbar: it says how
        // much there is and where in it the reader stands. `content_h` is the whole
        // wrapped height and `offset` the absolute row it is scrolled to, both
        // resolved above and both untouched by the windowing, so the thumb's travel
        // spans the transcript rather than collapsing to one viewport's worth.
        //
        // The max offset `clamp_preview_offset` can ever produce for THIS
        // geometry (mirrors that fn's own formula, including its `u32` domain);
        // needed here to know when a boundary arrow should show and to size the
        // thumb-detachment remap below.
        let max_offset =
            u32::try_from(content_h.saturating_sub(usize::from(inner_height))).unwrap_or(u32::MAX);
        // `ScrollbarState`'s `content_length` must span the OFFSET domain (the
        // number of distinct scroll positions), not the raw wrapped row count:
        // ratatui's thumb only touches the bottom of the track when
        // `position == content_length - 1`, and the max offset ever produced by
        // `clamp_preview_offset` is `content_h - inner_height`. Using `content_h`
        // directly would leave the thumb `inner_height - 1` cells short of the
        // track's end. `content_h - inner_height + 1` makes the max offset equal
        // `content_length - 1` exactly, so the thumb pins to the bottom; the
        // guard above guarantees this is >= 2 (never zero/degenerate).
        // `viewport_content_length` is unaffected: it still sizes the thumb via
        // the fraction of transcript visible.
        let content_length = content_h - usize::from(inner_height) + 1;
        // The track's visible length once both boundary-arrow slots are ALWAYS
        // reserved (see `SCROLLBAR_ARROW_HIDDEN`): one row per arrow, matching
        // ratatui's own `track_length_excluding_arrow_heads`.
        let track_length = inner_height.saturating_sub(2);
        let position =
            scrollbar_thumb_position(offset, max_offset, content_length, content_h, track_length);
        let mut scrollbar_state = ScrollbarState::new(content_length)
            .position(position)
            .viewport_content_length(usize::from(inner_height));
        // Boundary-only glyphs: an arrow shows ONLY at the exact edge it points
        // toward (top when `offset == 0`, bottom once fully scrolled down);
        // otherwise the slot renders a blank so the reserved track length never
        // changes with scroll position (see `SCROLLBAR_ARROW_HIDDEN`).
        let begin_symbol = if offset == 0 {
            SCROLLBAR_BEGIN_ARROW
        } else {
            SCROLLBAR_ARROW_HIDDEN
        };
        let end_symbol = if offset >= max_offset {
            SCROLLBAR_END_ARROW
        } else {
            SCROLLBAR_ARROW_HIDDEN
        };
        // DIM-only styling (no fixed color), matching the preview's restrained,
        // dark-terminal-safe palette (see `store::preview`'s `marker_style`).
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .style(Style::default().add_modifier(Modifier::DIM))
            .begin_symbol(Some(begin_symbol))
            .end_symbol(Some(end_symbol));
        // The track spans exactly the TRANSCRIPT's rows — the thing it scrolls —
        // which keeps it off the block's top/bottom border corners and title, and
        // starts it below the pinned banner on a session that has one (the banner
        // does not scroll, so no track cell should address it). Full pane width, so
        // the rightmost column it draws on is the block's own right border.
        frame.render_stateful_widget(
            scrollbar,
            Rect {
                x: area.x,
                y: transcript_area.y,
                width: area.width,
                height: transcript_area.height,
            },
            &mut scrollbar_state,
        );
    }

    // A DOCKED compose zone occupies the bottom rows the transcript was shrunk away
    // from (see `preview_compose_split`); the full-width bottom-bar fallback is
    // drawn by `render` instead, so this only fires when `dock_compose`.
    if dock_compose {
        render_compose_zone(frame, app, compose_area);
    }
}

/// The transcript's REAL wrapped height: how many screen rows `lines` occupy once
/// `Wrap { trim: false }` has wrapped them at `inner_width`.
///
/// ASKED OF THE WIDGET, never modeled here. `Paragraph::line_count` runs the very
/// same `WordWrapper` that `Paragraph::render` runs, so this cannot drift from what
/// is painted; `ratatui_widgets::reflow` is a private module, so that accessor is the
/// only way to reach the wrapper (hence the crate's `unstable-rendered-line-info`
/// feature — see Cargo.toml).
///
/// The alternative — a character-packing `ceil(width / inner)` count — is not an
/// approximation with a safe direction, it is a DIFFERENT function, wrong BOTH ways
/// and user-visibly so, since `max_offset` is derived from whatever this returns:
///
/// - it UNDER-counts wherever a row ends early at a word boundary (ordinary prose:
///   `alpha bravo charlie delta` packs to 3 rows at width 10, wraps to 4), and a
///   short `max_offset` makes the tail of a long transcript unreachable — "follow
///   bottom" stops short of the newest turn and the thumb never reaches the track's
///   end;
/// - it OVER-counts wherever the wrapper swallows the whitespace it broke on (the
///   checked-in `sess-normal-1` fixture packs to 223 rows at inner width 1 against
///   187 painted), and a long `max_offset` scrolls the pane off the end of its own
///   content into blank rows.
///
/// NO BLOCK is set on the measured paragraph, deliberately: `line_count` adds
/// `Block::vertical_space` when one is, and the preview's border is drawn in a
/// SEPARATE pass over its own rect (see [`render_preview`]), so a block here would
/// count those two rows twice.
///
/// Re-running the wrapper over a whole transcript is not free, so a transcript is
/// measured ONCE per (session, width) into the prefix map below
/// ([`wrapped_row_prefix`], built at cache fill); only the short things that are NOT
/// the cached transcript (a draft card, an in-flight reply tail) are measured per
/// frame. The clone is what `Paragraph` needs to own its text.
pub(crate) fn wrapped_text_rows(lines: &[Line<'_>], inner_width: u16) -> usize {
    Paragraph::new(Text::from(lines.to_vec()))
        .wrap(Wrap { trim: false })
        .line_count(inner_width)
}

/// The EXACT per-line wrapped-row prefix map over `lines` at `inner_width`: entry
/// `n` is how many screen rows `lines[..n]` occupy, so the map is ONE LONGER than
/// the lines it describes, starts at `0`, and its LAST entry is the whole run's
/// wrapped height — the very number [`wrapped_text_rows`] answers for the same
/// slice. It replaces that whole-text call rather than joining it.
///
/// Measured line by line through that same widget seam, which is sound for one
/// reason and only that one: `Wrap { trim: false }` runs `WordWrapper`, which
/// breaks each LOGICAL line on its own and never joins two of them onto a shared
/// row, so a wrapped row count is ADDITIVE over lines and a sum of per-line counts
/// IS the whole-text count. That property is a claim about a private module, so it
/// is PINNED by a test rather than assumed (see
/// `per_line_wrapped_row_counts_sum_to_the_whole_text_count`); a ratatui bump that
/// broke it would make every offset below wrong, silently.
///
/// What the map buys is a MAP where there was only a total. With it the pane can
/// answer, in O(log n), which logical line a wrapped-row offset lands in and how
/// far into it ([`row_window`]) — so a draw hands the widget only the lines the
/// viewport can reach, and a search jump reads the row a matched line starts on as
/// a single index rather than re-wrapping every line above it.
pub(crate) fn wrapped_row_prefix(lines: &[Line<'_>], inner_width: u16) -> Vec<usize> {
    let mut prefix = Vec::with_capacity(lines.len() + 1);
    let mut rows = 0usize;
    prefix.push(rows);
    for line in lines {
        rows += wrapped_text_rows(std::slice::from_ref(line), inner_width);
        prefix.push(rows);
    }
    prefix
}

/// Which LOGICAL line of a [`wrapped_row_prefix`] map holds absolute wrapped `row` —
/// a binary search, and the ONE place that question is answered.
///
/// Both consumers must agree or the pane contradicts itself: the windowed draw
/// ([`row_window`]) decides which line to START painting at, and the mouse hit-test
/// ([`visual_to_content`]) decides which line was painted at a clicked row. Two
/// derivations of that — the draw off this exact map, the click off a model of its
/// own — is precisely how a click resolves to a line the pane never painted there.
///
/// A `row` past the map's total answers with the index ONE PAST the last line, since
/// the map holds one more entry than it has lines. The draw clamps that (an offset
/// past the end simply paints nothing); the hit-test rejects it (see
/// [`visual_to_content`]).
fn line_at_row(row_prefix: &[usize], row: usize) -> usize {
    // The LAST entry still `<= row` is the line that occupies it: entries repeat for
    // any zero-height line, and the line that owns the row is the last of them.
    row_prefix
        .partition_point(|&rows| rows <= row)
        .saturating_sub(1)
}

/// The half-open range of LOGICAL lines a `viewport_h`-row viewport can reach when
/// it starts at wrapped row `offset`, plus the rows to skip INSIDE the first of
/// them (the RESIDUAL the widget is then scrolled by).
///
/// `row_prefix` is a [`wrapped_row_prefix`] map, so both bounds are BINARY SEARCHES
/// over it rather than a walk: the window starts at the line holding `offset`
/// ([`line_at_row`]), and ends after the last line that begins before
/// `offset + viewport_h`. `offset - row_prefix[start]` is what is left over once the
/// window has absorbed every whole line above it, so
/// `row_prefix[start] + residual == offset` and the first row painted is the row the
/// pane was scrolled to.
///
/// Saturating and clamped at both ends: an `offset` past the content yields an EMPTY
/// range (the pane draws nothing, which is what scrolling off the end shows anyway)
/// rather than an out-of-bounds slice, and a zero-height viewport never inverts the
/// range. Pure and terminal-free.
pub(crate) fn row_window(
    row_prefix: &[usize],
    offset: usize,
    viewport_h: u16,
) -> (std::ops::Range<usize>, usize) {
    // `row_prefix` describes one MORE position than it has lines (it ends with the
    // total), so the last addressable line index is one short of its length.
    let lines = row_prefix.len().saturating_sub(1);
    let start = line_at_row(row_prefix, offset);
    let residual = offset.saturating_sub(row_prefix.get(start).copied().unwrap_or(0));
    let last_row = offset.saturating_add(usize::from(viewport_h));
    let end = row_prefix
        .partition_point(|&rows| rows < last_row)
        .min(lines)
        .max(start);
    (start..end, residual)
}

/// Map a `visual_row` (rows from the top of the wrapped transcript) to the
/// `(content_row, sub_row)` it lands on — which logical line, and which wrapped
/// sub-row within that line.
///
/// BOTH halves are EXACT, and that is the whole reason `row_prefix` is what this
/// takes: the map was measured by the very wrapper that paints the pane
/// ([`wrapped_row_prefix`]), so the line is one binary search ([`line_at_row`]) and
/// the sub-row is what is left of the visual row once that line's own start row is
/// subtracted. Nothing is accumulated across the lines above the click, so nothing
/// can drift with the transcript's length. A per-line MODEL walked from the top is
/// what this replaced, and its error grew with every wrapping line above the click.
///
/// `None` when the visual row is past the end of the content — [`line_at_row`]
/// answers such a row with the index one past the last line, which is exactly the
/// case a hit-test must refuse rather than clamp. Pure so the mapping is
/// unit-testable from a map alone.
fn visual_to_content(row_prefix: &[usize], visual_row: usize) -> Option<(usize, usize)> {
    // The map describes one MORE position than it has lines (it ends with the
    // total), so the last addressable line index is one short of its length.
    let lines = row_prefix.len().saturating_sub(1);
    let content_row = line_at_row(row_prefix, visual_row);
    if content_row >= lines {
        return None;
    }
    let sub_row = visual_row.saturating_sub(row_prefix[content_row]);
    Some((content_row, sub_row))
}

/// The url of a preview link under a mouse click at screen `(col, row)`, or `None`.
///
/// `inner` is the preview pane's INNER rect (inside the borders), `scroll_offset`
/// the resolved vertical offset in wrapped rows (`App::preview_scroll`), and
/// `row_prefix` the whole transcript's per-line wrapped-row map — the SAME
/// [`wrapped_row_prefix`] the pane windowed its draw by, read off the same
/// width-scoped cache (`App::preview_hit_context`). The click is translated to
/// content coordinates and matched against a [`LinkRegion`]: screen row -> wrapped
/// visual row (via `scroll_offset`) -> `(content_row, sub_row)` -> content column. A
/// region whose `col_start..col_end` on that row contains the content column yields
/// its url.
///
/// Because a region spans the label's full content-column range, a link that
/// SOFT-WRAPS across visual rows is hit on ANY of its wrapped segments for free
/// (each segment's cells map back into the same content-column range) — no special
/// case.
///
/// `scroll_offset` stays ABSOLUTE — rows from the top of the whole transcript —
/// even though the pane hands the widget only a WINDOW of logical lines and scrolls
/// it by a small residual (see [`row_window`]). That is not a coincidence to be
/// preserved by luck: the window starts at the logical line holding absolute row
/// `scroll_offset`, and its residual is what is left of that offset once the whole
/// lines above it are absorbed, so the first row PAINTED is absolute row
/// `scroll_offset` either way. Screen rows therefore still count from the top of the
/// transcript, and `row_prefix` must likewise stay the WHOLE transcript's map —
/// handing this the window's map instead would resolve every click on a scrolled
/// pane to a line near the top of the file.
///
/// WHICH LINE a click lands on is EXACT, however long the transcript and wherever it
/// is scrolled: it is a binary search of the map the wrapper itself measured
/// ([`visual_to_content`]), the same map the draw windows by, so no error is
/// accumulated over the lines above the click. That replaced a per-line
/// character-packing walk whose error GREW with every wrapping line above the click
/// — unbounded once the preview's old 600-line tail cap was removed, since the cap
/// was all that had ever bounded it.
///
/// What stays APPROXIMATE is the COLUMN within that one line: `sub_row *
/// inner.width` packs characters while the wrapper breaks at word boundaries, so a
/// sub-row's true start falls on EITHER side of that product. Break EARLY at a word
/// boundary and the row spent FEWER source characters than the product assumes, so
/// the computed column runs to the RIGHT of the true one; SWALLOW the whitespace
/// broken on — consumed with no cell painted for it — and the row spent MORE, so the
/// column runs to the LEFT
/// (`character_packing_and_word_wrap_disagree_in_both_directions` pins both halves).
/// Its error is bounded either way by ONE logical line's own wrapped extent, and it
/// can never address a DIFFERENT line, because [`line_at_row`] gated the
/// `content_row` exactly and that gate is DIRECTION-INDEPENDENT — which is why the
/// second direction costs the bound nothing. So the worst case is a click on a
/// wrapped line's CONTINUATION row missing the link painted there — or, on a line
/// carrying several links, resolving to a LATER one where the column overshoots and
/// an EARLIER one where it undershoots. The FIRST row of every line — and every row
/// of every line that fits `inner.width`, which is the common case — is exact.
///
/// It stays an approximation because the real wrapper
/// (`ratatui_widgets::reflow::WordWrapper`) is private and the one public accessor
/// over it, `Paragraph::line_count`, answers how TALL a line is and never where
/// inside it each row broke; closing that gap would mean reimplementing the wrapper,
/// which would be a SECOND model to drift. Pure and terminal-free.
pub(crate) fn link_at<'a>(
    col: u16,
    row: u16,
    inner: Rect,
    scroll_offset: u32,
    row_prefix: &[usize],
    regions: &'a [LinkRegion],
) -> Option<&'a str> {
    if !inner.contains(Position { x: col, y: row }) {
        return None;
    }
    let rel_col = usize::from(col - inner.x);
    let rel_row = usize::from(row - inner.y);
    // `scroll_offset` is `App::preview_scroll`, so it carries the pane's `u32`
    // offset domain into this `usize` row lookup. Saturating both steps: the worst a
    // saturated one can produce is a row past the end of the map, which
    // `visual_to_content` already answers with the "no link here" `None`.
    let visual_row = usize::try_from(scroll_offset)
        .unwrap_or(usize::MAX)
        .saturating_add(rel_row);
    let (content_row, sub_row) = visual_to_content(row_prefix, visual_row)?;
    // The one packed step left. A word-wrapped sub-row starts on EITHER side of
    // `sub_row * inner.width`, so this can overshoot OR undershoot the true column —
    // never reaching another line either way, since the content row came from the map
    // above rather than from this arithmetic.
    let content_col = sub_row * usize::from(inner.width) + rel_col;
    regions
        .iter()
        .find(|r| {
            r.content_row == content_row && r.col_start <= content_col && content_col < r.col_end
        })
        .map(|r| r.url.as_str())
}

/// Resolve the final vertical preview offset: pin to the bottom when following,
/// else clamp the requested offset into `[0, max_offset]` where
/// `max_offset = content_h - viewport_h`. Saturating throughout, so a short
/// transcript never underflows past zero and a huge one never overflows `u32`.
///
/// `content_h` is a `usize` wrapped-row count and the offset it produces is a
/// `u32`, which is wide enough to be a real bound rather than a second cap: a
/// transcript that wraps past 65,535 rows in a narrow pane resolves to the row it
/// actually sits on instead of pinning at `u16::MAX` — where "follow the bottom"
/// would stop short of the newest turn and every deeper offset would collapse
/// onto one indistinguishable position.
fn clamp_preview_offset(
    follow_bottom: bool,
    requested: u32,
    content_h: usize,
    viewport_h: u16,
) -> u32 {
    let max_offset =
        u32::try_from(content_h.saturating_sub(usize::from(viewport_h))).unwrap_or(u32::MAX);
    if follow_bottom {
        max_offset
    } else {
        requested.min(max_offset)
    }
}

/// The preview offset that parks a matched line one
/// [`MATCH_JUMP_LEAD_DIVISOR`]th of the way down the viewport, given how many
/// wrapped ROWS sit above that line.
///
/// `rows_above` must be the EXACT wrapped-row count of the lines preceding the
/// match — the first screen row that line occupies — which is why the caller READS
/// it off the cached per-line prefix map (`App::preview_rows_above`, one index into
/// [`wrapped_row_prefix`]) rather than modelling the wrap or re-measuring the
/// transcript's own prefix on the keypress. Saturating at BOTH ends: a match inside
/// the first `viewport_h / MATCH_JUMP_LEAD_DIVISOR` rows resolves to 0 (the
/// transcript's start) instead of underflowing, and a `rows_above` beyond
/// `u32::MAX` pins at the ceiling instead of wrapping.
/// Returns the same `u32` offset domain as [`clamp_preview_offset`], so a match
/// past 65,535 wrapped rows is jumped to rather than clipped short of.
/// Pure and terminal-free.
fn match_jump_offset(rows_above: usize, viewport_h: u16) -> u32 {
    let lead = usize::from(viewport_h / MATCH_JUMP_LEAD_DIVISOR);
    u32::try_from(rows_above.saturating_sub(lead)).unwrap_or(u32::MAX)
}

/// A conservative, provably-sufficient real-offset distance from an edge such
/// that ratatui's own thumb-geometry rounding (`rounding_divide(position *
/// track_length, content_h)`, per `ratatui-widgets`' `Scrollbar`) can never
/// round the thumb back onto that edge: at `min_detach_distance` rows off an
/// edge, `position * track_length >= content_h`, i.e. the position-to-track
/// ratio has already reached a full track cell, so ANY rounding rule lands
/// the thumb at least one cell in. Pure so the margin math is unit-testable
/// without a terminal.
fn min_detach_distance(content_h: usize, track_length: usize) -> usize {
    // Guard a zero-length track (degenerate layout) so we never divide by zero.
    content_h.div_ceil(track_length.max(1))
}

/// Resolve the `ScrollbarState` position fed to the preview scrollbar widget
/// — NOT the exact `Paragraph::scroll` offset upstream, which must stay
/// unclamped so the transcript itself always scrolls by the real amount.
///
/// Pins exactly to the first/last track row at the real edges (`offset == 0`,
/// `offset >= max_offset`). For a GENUINE partial scroll in between, a naive
/// `position = offset` can round back onto an edge track row purely from
/// `rounding_divide`'s rounding when `content_h` is huge relative to
/// `track_length` (a long transcript in a short pane) — reading as "nothing
/// happened" even though a real scroll occurred. This remaps that case into a
/// position clamped at least [`min_detach_distance`] rows off BOTH ends, so
/// the thumb always renders detached from both track ends.
///
/// The bottom margin is doubled versus the top: ratatui clamps a fractional
/// thumb length below one track cell UP to a minimum of one (never down), so
/// the bottom of the track silently loses up to one more `min_detach`-sized
/// slice of headroom than the top does. Doubling absorbs that worst case
/// without modelling the exact thumb length. A track too short to hold any
/// strictly-interior position collapses the range to a single safe value
/// (via `.max`/`.min` rather than a `.clamp` that could panic on an inverted
/// range) instead of a nonsensical clamp.
fn scrollbar_thumb_position(
    offset: u32,
    max_offset: u32,
    content_length: usize,
    content_h: usize,
    track_length: u16,
) -> usize {
    if offset == 0 || max_offset == 0 {
        return 0;
    }
    let last = content_length.saturating_sub(1);
    if offset >= max_offset {
        return last;
    }
    let margin = min_detach_distance(content_h, usize::from(track_length));
    let mid = last / 2;
    let lo = margin.min(mid);
    let hi = last.saturating_sub(margin.saturating_mul(2)).max(lo);
    // The offset shares the pane's `u32` domain (see `clamp_preview_offset`) while
    // the track math is `usize`. Saturating rather than `as`: on any platform where
    // `usize` is narrower than `u32`, the `max`/`min` chained onto it still bounds
    // the result, so a saturated value can only land ON the track, never off it.
    usize::try_from(offset)
        .unwrap_or(usize::MAX)
        .max(lo)
        .min(hi)
}

/// The search input line, echoing the live query.
///
/// The trailing cursor pulses off the SAME [`blink_visible`] phase of
/// [`App::tick`] as the live badge's dot, so the board has exactly ONE blink
/// mechanism and the two pulse together rather than drifting against each other.
/// This cursor carried the ANSI blink attribute (ratatui's slow-blink
/// `Modifier`) originally and therefore never actually blinked — see
/// [`blink_visible`] for why the terminal cannot be asked to animate it.
fn render_search(frame: &mut Frame, app: &App, area: Rect) {
    let cursor = if blink_visible(app.tick) {
        SEARCH_CURSOR
    } else {
        SEARCH_CURSOR_HIDDEN
    };
    let line = Line::from(vec![
        Span::styled("search: ", Style::default().fg(Color::Cyan)),
        Span::raw(app.query.clone()),
        Span::raw(cursor),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// The which-key hint that takes over the help line while a `Ctrl-X` leader chord
/// is pending: the follow-up keys and what each does. The `x` verb tracks the
/// selected row — `hide` for a visible session, `expose` for one already hidden
/// (there `x` un-hides it) — so the hint names what the next keypress actually does.
/// One place for the wording (NO MAGIC VALUES); keep it in step with
/// [`update::chord_key`](crate::tui::update). Rendered with a NAMED color +
/// modifier only, no RGB or ANSI (PATTERNS §7, TERMINAL-SAFE STYLING).
///
/// COLUMN BUDGET: the help row is ONE line and is truncated, never wrapped, so
/// the longest form is what has to fit — `expose` (the wider verb) lands it at
/// exactly 80 columns. Anything added here costs the tail of an 80-column
/// terminal, so weigh a new verb against `Esc cancel` rather than appending.
fn chord_hint(selected_hidden: bool) -> String {
    let x = if selected_hidden { "expose" } else { "hide" };
    format!("^X  x {x} · d delete row/lineage · h show/hide hidden · r reload · Esc cancel")
}

/// The compose zone's key hints, per open draft. Pure so the wording is assertable
/// without a terminal.
///
/// The background draft's `Ctrl-O` hint is worded "run interactively" and NOTHING
/// more, deliberately. The prompt reaches claude as a trailing positional and
/// AUTO-SUBMITS as the first turn — no pre-fill mechanism exists (see
/// [`crate::resume::build_new_argv`]) — so any wording that hinted at reviewing or
/// editing the draft inside claude would promise something the CLI cannot do.
///
/// "paste keeps newlines" names no key on purpose: the terminal's own paste is not
/// a snapback binding (there is no `Ctrl-V`), so caret notation here would advertise
/// one that does not exist. The line states what a paste DOES rather than reassuring
/// that it is allowed.
///
/// It rides the REPLY arm ONLY, and NOT because the reply box is the only one a
/// pasted newline used to break — it is not.
/// [`compose_key_to_action`](crate::tui::compose::compose_key_to_action) is SHARED
/// by both targets and maps a bare `Enter` to Send, so the same paste that sent a
/// truncated reply launched a background agent on the draft's first line. The split
/// is COLUMN BUDGET alone, measured with the `unicode-width` the renderer counts in:
/// the help row is ONE line that never wraps, the reply hint is 55 columns and the
/// clause 23, so it lands at 78 and still fits an 80-column terminal, while
/// [`BG_DRAFT_HINT`] is already 97 there (`Esc cancel` starts at column 88, off
/// screen) and the clause would be painted at columns 98-120 — nowhere. What a paste
/// does is documented in full where there is room for it: `KEYS` in `cli.rs` and the
/// README key map.
fn compose_hint(target: &ComposeTarget) -> &'static str {
    match target {
        ComposeTarget::Reply { .. } => {
            "Enter send · Ctrl-J newline (or Alt+Enter) · paste keeps newlines · Esc cancel"
        }
        // The SAME const the draft card shows, so the two surfaces cannot describe
        // the same keys differently.
        ComposeTarget::NewBackgroundAgent { .. } => BG_DRAFT_HINT,
    }
}

/// The bottom help line: the keybinding cheat sheet, a transient board status
/// (e.g. a resume refusal) when one is set, or the [`chord_hint`] while a `Ctrl-X`
/// leader chord is pending. A status wins over the cheat sheet and is flattened to
/// a single row (newlines -> spaces) since the help area is 1 tall; the chord hint
/// wins over both, since the chord owns the keyboard the moment it is armed.
fn render_help(frame: &mut Frame, app: &App, area: Rect) {
    if app.pending_chord {
        // The leader chord took the keyboard: show its follow-up keys so the chord
        // is discoverable the moment `Ctrl-X` is hit. The `x` verb flips to "expose"
        // when the selected row is already hidden, since there `x` un-hides it.
        let selected_hidden = app
            .selected
            .as_ref()
            .is_some_and(|id| app.hidden_ids.contains(id));
        let hint = Line::from(vec![Span::styled(
            chord_hint(selected_hidden),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )]);
        frame.render_widget(Paragraph::new(hint), area);
        return;
    }
    let line = if let Some(status) = &app.status {
        // A transient status (a refusal, or a send's cost / error) wins the
        // line, flattened to a single row.
        let flat = status.split_whitespace().collect::<Vec<_>>().join(" ");
        Line::from(vec![Span::styled(
            flat,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )])
    } else if let Some(compose) = &app.compose {
        // The compose zone owns the keyboard: show its chords instead of the board
        // keymap. Ctrl-J is the primary newline; Alt+Enter the guaranteed fallback.
        Line::from(vec![Span::styled(
            compose_hint(&compose.target),
            Style::default().add_modifier(Modifier::DIM),
        )])
    } else {
        // The board keymap — one of the four surfaces AGENTS.md's KEEP KEY DOCS IN
        // SYNC names. It does NOT mention the terminal's paste, on COLUMN BUDGET:
        // this line is already 223 columns (measured with the `unicode-width` the
        // renderer counts in) against a help row that is ONE line and never wraps, so
        // on an 80-column terminal it is cut mid-`^K stop` and everything from
        // `^X hide/del` (column 87) rightward is already unpainted. A 23-column
        // "paste keeps newlines" clause would land at columns 224-246 — nowhere, on
        // any realistic width. What a board paste DOES (append to the query with
        // newlines flattened to spaces, and never resume) is documented where there
        // is room to say it: `KEYS` in `cli.rs` and the README key map.
        //
        // `S-↑↓ match` sits with the search cluster rather than with the scroll
        // keys, because it is search navigation that happens to move a pane — and
        // it is spelled `S-` rather than `⇧` so it needs no glyph the terminal may
        // not have. It is off-screen at 80 columns like everything past `^X`, and
        // that is the same budget every clause here is judged against; the key is
        // documented in full in `KEYS` and the README.
        Line::from(vec![Span::styled(
            "↑↓/jk move · ←/→ fold/expand · Enter resume · ^F fork · ^N new · ^R reply · ^K stop · ^X hide/del · type to search · Tab name/content · S-↑↓ match · ^A scope · ^/ preview · PgUp/PgDn·^U/^D·Home/End·wheel scroll · q/Esc quit",
            Style::default().add_modifier(Modifier::DIM),
        )])
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// Shared width (columns) of every modal overlay. ONE constant retiring the
/// running-session choice's old literal `62` and the picker's old
/// `AGENT_PICK_WIDTH` — the latter existed only to match the former's footprint,
/// so the two were always meant to be identical. [`centered_rect`] shrinks it to
/// fit on a tiny terminal.
const MODAL_WIDTH: u16 = 62;

/// Non-message rows a `Row`-layout modal draws, borders excluded: a blank spacer,
/// the button strip, a blank spacer, and the footer help line. The message (one or
/// more wrapped rows) is added on top, so the box grows to fit a long prompt rather
/// than clipping it.
const MODAL_ROW_CHROME_ROWS: u16 = 4;

/// Non-message, non-entry rows a `List`-layout modal draws around its selectable
/// list: a blank spacer above the list, a blank spacer below it, and a footer help
/// line. The box height is message rows + entries + this chrome + two borders, so a
/// picker grows with its choice count (the picker's old `AGENT_PICK_CHROME_ROWS`
/// reasoning, kept) and any modal grows with a wrapped message.
const MODAL_LIST_CHROME_ROWS: u16 = 3;

/// Word-wrap `text` into lines no wider than `width` columns, breaking on
/// whitespace; a single word longer than `width` is kept whole (it clips rather
/// than splitting mid-word — fine for the short, controlled prompts a modal
/// carries). Always returns at least one line so an empty message still reserves a
/// row. Pure, so the wrapped line count that sizes the modal box is unit-testable.
/// Counts by `char` — exact for the ASCII prompts these modals use.
fn wrap_message(text: &str, width: u16) -> Vec<String> {
    let width = usize::from(width.max(1));
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let need = if line.is_empty() {
            word.chars().count()
        } else {
            line.chars().count() + 1 + word.chars().count()
        };
        if !line.is_empty() && need > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    lines.push(line);
    lines
}

/// The "stop the waiting agent?" confirmation overlay, shown when `Ctrl-R` targets
/// a `needs input` background agent. Confirming stops the waiting agent (ending its
/// live job, conversation kept) so the reply can land in place; the two moves are
/// spelled out because stopping is not free.
///
/// Drawn last (on top of the board) with a [`Clear`]. Pure presentation — the
/// target session and the job id live on [`App::pending_stop`]; styled with named
/// colors only (TERMINAL-SAFE STYLING).
fn render_stop_confirm(frame: &mut Frame, app: &App) {
    let Some(pending) = &app.pending_stop else {
        return;
    };
    let label = app
        .session_by_id(&pending.session_id)
        .map(|s| s.label.as_str())
        .filter(|l| !l.is_empty())
        .unwrap_or(pending.session_id.as_str());
    let area = centered_rect(frame.area(), 64, 8);

    let lines = vec![
        Line::from(Span::styled(
            "This session is a waiting agent.",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::raw(format!(
            "Stop it and reply in place?  —  {label}"
        ))),
        Line::from(Span::styled(
            "(ends the live agent; its conversation is kept)",
            Style::default().add_modifier(Modifier::DIM),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Enter  stop & reply    \u{b7}    Esc  cancel",
            Style::default().add_modifier(Modifier::DIM),
        )),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" stop the waiting agent? ");
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .alignment(Alignment::Center),
        area,
    );
}

/// The "stop this agent?" interrupt confirmation overlay, shown when `Ctrl-K`
/// targets a live, not-yet-finished agent. Confirming runs `claude stop <job-id>`,
/// ending the live job (its conversation is kept) — an interrupt, so the wording
/// says nothing about a reply, unlike [`render_stop_confirm`].
///
/// Drawn last (on top of the board) with a [`Clear`]. Pure presentation — the target
/// session and the job id live on [`App::pending_interrupt`]; styled with named
/// colors only (TERMINAL-SAFE STYLING).
fn render_interrupt_confirm(frame: &mut Frame, app: &App) {
    let Some(pending) = &app.pending_interrupt else {
        return;
    };
    let label = app
        .session_by_id(&pending.session_id)
        .map(|s| s.label.as_str())
        .filter(|l| !l.is_empty())
        .unwrap_or(pending.session_id.as_str());
    let area = centered_rect(frame.area(), 64, 8);

    let lines = vec![
        Line::from(Span::styled(
            "This session is running as an agent.",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::raw(format!("Stop it?  —  {label}"))),
        Line::from(Span::styled(
            "(ends the live agent; its conversation is kept)",
            Style::default().add_modifier(Modifier::DIM),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Enter  stop    \u{b7}    Esc  cancel",
            Style::default().add_modifier(Modifier::DIM),
        )),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" stop this agent? ");
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .alignment(Alignment::Center),
        area,
    );
}

/// The generic modal overlay: a centered bordered box with a title, a message, its
/// choices (a `Row` button strip or a vertical `List`), and a footer help line.
///
/// Drawn last (on top of the board) with a [`Clear`] so the board shows through
/// only outside the box. The choices, the highlight, and the routing all live on
/// the [`Modal`] in [`App`], so this is pure presentation. Styled with named
/// colors + modifiers only (terminal-safe). The message accent and footer are
/// derived from the layout, preserving each overlay's original chrome: a `Row`
/// reads as a warning/confirm (`Yellow`, `←/→ … Enter confirm`, centered), a
/// `List` as a picker (`Cyan`, `↑/↓ … Enter draft`, left-aligned).
fn render_modal(frame: &mut Frame, modal: &Modal) {
    let (accent, footer) = match modal.layout {
        ModalLayout::Row => (
            Color::Yellow,
            "\u{2190}/\u{2192} choose \u{b7} Enter confirm \u{b7} Esc cancel",
        ),
        // The picker has TWO verbs, so its footer names both: Enter drafts the
        // session's first message (staying on the board), Ctrl-O starts the agent
        // interactively at once (leaving it). One key each — neither is buried.
        ModalLayout::List => (
            Color::Cyan,
            "↑/↓ choose · Enter draft · ^O interactive · Esc cancel",
        ),
    };

    // Wrap the message to the box's inner width (borders excluded) so a long prompt
    // — e.g. the delete confirmation — shows in full instead of clipping at the
    // border; the box height below counts the wrapped rows so the two agree.
    let message = wrap_message(&modal.message, MODAL_WIDTH.saturating_sub(2));
    let message_rows = message.len() as u16;
    let mut lines: Vec<Line> = message
        .into_iter()
        .map(|l| {
            Line::from(Span::styled(
                l,
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ))
        })
        .collect();
    lines.push(Line::from(""));

    // The choices, plus the box height (message rows + chrome + borders; a list also
    // grows with its entry count).
    let height = match modal.layout {
        ModalLayout::Row => {
            lines.push(Line::from(modal_button_row(&modal.choices, modal.selected)));
            message_rows
                .saturating_add(MODAL_ROW_CHROME_ROWS)
                .saturating_add(2)
        }
        ModalLayout::List => {
            for (i, choice) in modal.choices.iter().enumerate() {
                lines.push(modal_list_row(
                    &choice.label,
                    choice.description.as_deref(),
                    i == modal.selected,
                ));
            }
            (modal.choices.len() as u16)
                .saturating_add(message_rows)
                .saturating_add(MODAL_LIST_CHROME_ROWS)
                .saturating_add(2)
        }
    };

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        footer,
        Style::default().add_modifier(Modifier::DIM),
    )));

    let area = centered_rect(frame.area(), MODAL_WIDTH, height);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", modal.title));
    frame.render_widget(Clear, area);
    let mut paragraph = Paragraph::new(lines).block(block);
    if matches!(modal.layout, ModalLayout::Row) {
        // Center the whole box (message, buttons, footer), as the old
        // running-session overlay did.
        paragraph = paragraph.alignment(Alignment::Center);
    }
    frame.render_widget(paragraph, area);
}

/// A `Row`-layout modal's horizontal button strip: each choice as ` label `
/// (bold, the highlighted one also reversed), separated by four spaces — the old
/// running-session overlay's button styling, verbatim.
fn modal_button_row(choices: &[ModalChoice], selected: usize) -> Vec<Span<'static>> {
    let mut spans: Vec<Span> = Vec::new();
    for (i, choice) in choices.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("    "));
        }
        let mut style = Style::default().add_modifier(Modifier::BOLD);
        if i == selected {
            style = style.add_modifier(Modifier::REVERSED);
        }
        spans.push(Span::styled(format!(" {} ", choice.label), style));
    }
    spans
}

/// One row of a `List`-layout modal: a `› ` marker + reversed, bold label when
/// selected (the same highlight glyph the session list uses), else a padded label,
/// with an optional dim description trailing. Owns its text (`'static`) so it
/// composes into the modal `Paragraph`. (The picker's old `agent_entry_line`,
/// generalized to any list modal.)
fn modal_list_row(label: &str, description: Option<&str>, selected: bool) -> Line<'static> {
    let (marker, label_style) = if selected {
        (
            "› ",
            Style::default()
                .add_modifier(Modifier::REVERSED)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        ("  ", Style::default())
    };
    let mut spans = vec![
        Span::raw(marker),
        Span::styled(label.to_string(), label_style),
    ];
    if let Some(desc) = description {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            desc.to_string(),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    Line::from(spans)
}

/// A centered `width`x`height` (cells) rect within `area`, clamped so it never
/// exceeds the available space (a tiny terminal shrinks the box rather than
/// overflowing). Pure so the centering math is unit-testable.
fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

/// Format a session timestamp as `YYYY-MM-DD HH:MM`, or a placeholder.
fn short_time(ts: Option<OffsetDateTime>) -> String {
    match ts {
        Some(t) => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            t.year(),
            u8::from(t.month()),
            t.day(),
            t.hour(),
            t.minute()
        ),
        None => "--".to_string(),
    }
}

/// The marker a folded lineage head wears: `(+N)`, N being the members it stands
/// in for.
///
/// Only ever built for `hidden > 0` — a row that hides nothing must render
/// nothing, so a `(+0)` is unrepresentable rather than merely unused.
fn lineage_marker(hidden: usize) -> String {
    format!("{LINEAGE_MARKER_GAP}(+{hidden})")
}

/// The first [`CHILD_ID_CHARS`] chars of `session_id`.
fn short_id(session_id: &str) -> String {
    session_id.chars().take(CHILD_ID_CHARS).collect()
}

/// The turn-count segment a lineage CHILD row wears: `  6 msgs`.
///
/// The gap is folded in exactly as [`lineage_marker`] folds [`LINEAGE_MARKER_GAP`]
/// in, so the columns [`fit_child_msgs`] weighs are the columns this draws —
/// one number, impossible to reserve wrongly.
fn child_msgs(msg_count: usize) -> String {
    format!("{CHILD_MSGS_GAP}{msg_count}{CHILD_MSGS_SUFFIX}")
}

/// The turn-count segment a child row can afford, or `None` to draw none.
///
/// `content_width` is the row's drawable columns and `used` what its fields
/// (gutter, timestamp, badge, id) already spend.
///
/// ALL-OR-NOTHING, and that is the RULE rather than an implementation detail: a
/// clipped count is not a degraded count, it is a WRONG one. `171 msgs` cut to
/// fit reads back as `17` — a plausible number, silently off by an order of
/// magnitude — and this field exists precisely to say which member of a lineage
/// is a stalled stub and which holds the work. Getting no answer leaves the user
/// where they were; getting a confidently wrong one sends them to resume the
/// wrong session. So the segment renders WHOLE or not at all, and it never wears
/// [`LABEL_ELLIPSIS`].
///
/// The id is NOT cut down to make room, and that is the same marker-first
/// discipline [`fit_label`] applies rather than an exception to it. There the
/// label gives way because it is redundant — identical across the lineage. Here
/// the id is ALREADY cut to [`CHILD_ID_CHARS`], the documented minimum that
/// still tells one member of a lineage from another, so it has nothing left to
/// give: shortening it further would trade a field that cannot be wrong for one
/// that can, and could collapse two children onto a shared prefix. The count is
/// the field that yields last and, when the columns run out, entirely.
///
/// Pure, so the drop is tested as arithmetic rather than only through a pane.
fn fit_child_msgs(msg_count: usize, content_width: usize, used: usize) -> Option<String> {
    let segment = child_msgs(msg_count);
    (segment.chars().count() <= content_width.saturating_sub(used)).then_some(segment)
}

/// The label text for a row that must ALSO fit `marker` columns of `(+N)`.
///
/// `content_width` is the row's drawable columns and `used` what its prefix
/// (gutter, timestamp, badge) already spends; the label takes what is left after
/// the marker is held back, and is truncated with a [`LABEL_ELLIPSIS`] when it
/// does not fit.
///
/// The marker is reserved FIRST, and that ordering is the whole point: the label
/// is identical across every member of a lineage — which is exactly why the rows
/// looked like duplicates — so a few clipped chars off its tail cost nothing,
/// while the marker is the ONLY thing on the board saying N other sessions are
/// behind this row. Let the list clip right-to-left as it does by default and the
/// marker is the first thing gone on a narrow pane, silently turning a fold back
/// into the vanished-sessions bug it exists to prevent.
///
/// Pure so the reservation is tested as arithmetic rather than only through a
/// rendered pane. Truncation counts CHARS, matching [`highlight_runs`] and
/// `store::label`'s `LABEL_MAX` — the crate's one convention for label width.
fn fit_label(label: &str, content_width: usize, used: usize, marker: usize) -> String {
    let budget = content_width.saturating_sub(used).saturating_sub(marker);
    if label.chars().count() <= budget {
        return label.to_string();
    }
    // A budget of zero has no room even for the ellipsis; the marker still wins.
    if budget == 0 {
        return String::new();
    }
    label
        .chars()
        .take(budget - LABEL_ELLIPSIS.chars().count())
        .chain(LABEL_ELLIPSIS.chars())
        .collect()
}

/// Break `text` into consecutive `(slice, is_match)` runs on EXTENDED
/// GRAPHEME-CLUSTER boundaries — the one splitter behind both highlights.
///
/// `char_offset` is the CHAR position of `text`'s first char within the string
/// `matched` addresses, so a caller walking a line span by span can keep counting
/// across spans (a row label passes 0). A cluster is a match iff `matched` holds
/// ANY of its char positions, which is what snaps a run OUT to the cluster's
/// edges: the run may cover one extra codepoint, and it can never cut a cluster
/// in half. Runs that meet after that snap coalesce, exactly as abutting matches
/// always have, so no two adjacent spans ever carry the same state.
///
/// Cutting a cluster is not cosmetic. `Line::width` sums `unicode-width` PER SPAN
/// and that width is a CONTEXTUAL fold, so an emoji severed from its VS16 or its
/// skin-tone modifier measures differently from the same bytes unsplit — and the
/// cached wrapped-row prefix map, which BOTH the windowed draw and the click
/// hit-test read, is measured on the UNSPLIT lines. Ratatui's word wrapper
/// segments per span too, so a cut cluster also changes where the wrap falls.
/// (Cluster edges are not a total guarantee — unicode-width folds across a few
/// cross-cluster ligature contexts as well — but they cover the emoji sequences a
/// transcript actually carries.)
///
/// Every boundary is a valid char boundary by construction, so multi-byte text is
/// safe (never a raw byte slice), and any index in `matched` past the last char is
/// simply never encountered — an out-of-range index (e.g. from a width-truncated
/// label) is ignored rather than panicking. It walks the text ONCE, the same
/// single pass the char-by-char split it replaced made.
///
/// Pure and terminal-free so the run breakdown is unit-testable on its own.
fn match_runs<'a>(
    text: &'a str,
    char_offset: usize,
    matched: &HashSet<usize>,
) -> Vec<(&'a str, bool)> {
    let mut runs: Vec<(&str, bool)> = Vec::new();
    let mut char_pos = char_offset;
    let mut run_start = 0usize;
    let mut run_match: Option<bool> = None;
    for (byte_pos, cluster) in text.grapheme_indices(true) {
        let chars = cluster.chars().count();
        let is_match = (char_pos..char_pos + chars).any(|p| matched.contains(&p));
        char_pos += chars;
        match run_match {
            Some(open) if open != is_match => {
                runs.push((&text[run_start..byte_pos], open));
                run_start = byte_pos;
                run_match = Some(is_match);
            }
            Some(_) => {}
            None => run_match = Some(is_match),
        }
    }
    if let Some(open) = run_match {
        runs.push((&text[run_start..], open));
    }
    runs
}

/// Break `label` into consecutive owned `(text, is_match)` runs — [`match_runs`]
/// for a string the view owns end to end, so char positions start at 0 and the
/// runs are handed on as `Span` contents.
///
/// Pure and terminal-free; the same helper backs both the flat and the grouped
/// list (they share one session row renderer).
fn highlight_runs(label: &str, matched: &HashSet<usize>) -> Vec<(String, bool)> {
    match_runs(label, 0, matched)
        .into_iter()
        .map(|(text, is_match)| (text.to_string(), is_match))
        .collect()
}

/// Re-style `line` so the chars at `matched` CHAR positions carry `emphasis`,
/// leaving every other char exactly as it was.
///
/// The styled sibling of [`highlight_runs`], and the difference is the whole
/// point: a row label is unstyled text this view owns, whereas a preview line
/// arrives ALREADY styled by `store::preview` (markers, headings, DIM code, the
/// underlined link labels). So this splits the line's own spans at the matched
/// positions and ADDS the modifier to the matched runs, rather than replacing
/// their style — a marked word inside a DIM code span stays DIM.
///
/// Three invariants hold, and the rest of the pane depends on all three:
///
/// - **The text is byte-identical.** Only styles move, so display width and
///   `Line::width` are unchanged — which is what keeps `App::preview_hit_context`'s
///   link columns and the cached wrapped-row count describing what is drawn.
/// - **One line in, one line out.** No span is dropped even when empty, so the
///   line count the scroll clamp was measured against cannot move.
/// - **Cluster boundaries only.** It splits through [`match_runs`], so a span is
///   never sliced mid-codepoint OR mid-grapheme-cluster — the second is what keeps
///   the first invariant true, since a summed-per-span width is not the unsplit
///   width once a cluster is cut. A position past the line's last char is simply
///   never reached (out-of-range indices are ignored, not a panic).
///
/// `matched` addresses the LINE's plain text (`app::line_text`'s concatenation),
/// so the walk counts chars ACROSS spans rather than restarting per span. Cluster
/// boundaries are read per span, which is the same unit ratatui's wrapper reads
/// them in; a cluster the RENDERER already split across two spans stays split,
/// because this fn only promises not to add a cut of its own.
/// Pure and terminal-free.
fn highlight_matched_spans(
    line: &Line<'_>,
    matched: &HashSet<usize>,
    emphasis: Modifier,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len());
    let mut char_pos = 0usize;
    for span in &line.spans {
        // Runs are coalesced per span so a fully unmatched span stays ONE span
        // (the common case: most preview lines carry no match at all).
        let runs = match_runs(&span.content, char_pos, matched);
        char_pos += span.content.chars().count();
        if runs.is_empty() {
            // An empty span carries no chars but is kept anyway: dropping it would
            // be a structural change to a line this fn promises only to re-style.
            spans.push(Span::styled(
                String::new(),
                emphasized(span.style, false, emphasis),
            ));
            continue;
        }
        spans.extend(runs.into_iter().map(|(text, is_match)| {
            Span::styled(text.to_string(), emphasized(span.style, is_match, emphasis))
        }));
    }
    let mut out = Line::from(spans);
    // The LINE's own style and alignment are the span styles' backdrop; carrying
    // them over is part of "only the matched runs changed".
    out.style = line.style;
    out.alignment = line.alignment;
    out
}

/// `base`, plus `emphasis` when this run matched — the one place the match
/// modifier is composed ONTO an existing style rather than replacing it.
fn emphasized(base: Style, is_match: bool, emphasis: Modifier) -> Style {
    if is_match {
        base.add_modifier(emphasis)
    } else {
        base
    }
}

/// Style `label` into spans, giving matched-char runs the `highlight` style and
/// the rest the `base` style (see [`highlight_runs`] for the char-safe,
/// out-of-range-safe run split). The returned spans own their text (`'static`),
/// so they compose into a `Line` alongside the row's other spans; the base
/// style stays `default()` so the List's selection `highlight_style` layers over
/// them at render time.
fn highlight_label_spans(
    label: &str,
    matched: &HashSet<usize>,
    base: Style,
    highlight: Style,
) -> Vec<Span<'static>> {
    highlight_runs(label, matched)
        .into_iter()
        .map(|(text, is_match)| Span::styled(text, if is_match { highlight } else { base }))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use super::super::app::ModalAction;
    use super::*;
    use crate::store::Session;

    #[test]
    fn release_label_is_v_prefixed_crate_version() {
        let label = format_version_label(false, "abc1234", true);
        assert_eq!(label, format!("v{}", env!("CARGO_PKG_VERSION")));
        // Release builds ignore git metadata entirely.
        assert!(!label.contains("abc1234"));
        assert!(!label.contains("dirty"));
    }

    #[test]
    fn dev_label_carries_git_short_hash() {
        assert_eq!(format_version_label(true, "abc1234", false), "dev+abc1234");
    }

    #[test]
    fn dev_label_marks_a_dirty_working_tree() {
        assert_eq!(
            format_version_label(true, "abc1234", true),
            "dev+abc1234-dirty"
        );
    }

    #[test]
    fn version_label_under_cargo_test_is_a_dev_build() {
        // `cargo test` compiles in debug mode, so the live label takes the dev
        // branch; asserts the wiring (cfg + env vars), not a specific commit.
        assert!(version_label().starts_with(DEV_VERSION_PREFIX));
    }

    // --- header scope label -----------------------------------------------

    /// Width the header cases draw at: wide enough that no label is clipped
    /// before the `matched / total` counts, so a missing word is a real miss.
    const HEADER_WIDTH: u16 = 120;

    /// The header row `app` paints, as text.
    fn drawn_header(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(HEADER_WIDTH, 1))
            .expect("build an in-memory test terminal");
        terminal
            .draw(|frame| render_header(frame, app, frame.area()))
            .expect("render_header must not panic");
        full_row_text(terminal.backend().buffer(), 0, HEADER_WIDTH)
    }

    /// The project scope spans SEVERAL folders, so naming the one you launched
    /// in would be a lie about what the list is showing. The header takes the
    /// label git resolved for the whole worktree set instead.
    #[test]
    fn project_scope_header_names_the_resolved_project_not_the_launch_folder() {
        let mut app = App::new(
            vec![sample_session()],
            Scope::Project,
            PathBuf::from("/tmp/launch"),
        );
        app.worktrees = crate::worktrees::WorktreeSet::from_resolved(
            [PathBuf::from("/tmp/launch"), PathBuf::from("/tmp/other-wt")],
            Some("acme/web".to_string()),
        );

        let header = drawn_header(&app);

        assert!(
            header.contains("project:acme/web"),
            "the header must name the project git resolved: {header}"
        );
        assert!(
            !header.contains("project:launch"),
            "naming the launch folder would misdescribe a cross-worktree list: {header}"
        );
    }

    /// Fail-soft, and consistent with what the list actually shows: an
    /// unresolved set still scopes `Project` to the launch dir's repo ROOT, so
    /// the header names that root rather than going blank. `/tmp/launch` is a
    /// plain checkout, i.e. its own root, so the name is the folder's own here;
    /// the case where the two differ is
    /// [`project_scope_header_names_the_repo_root_not_the_worktree_launched_from`].
    #[test]
    fn project_scope_header_falls_back_to_the_repo_root_name() {
        // The test-default worktree probe resolves nothing, which is the
        // "git missing / not a repo" answer.
        let app = App::new(
            vec![sample_session()],
            Scope::Project,
            PathBuf::from("/tmp/launch"),
        );

        assert!(
            drawn_header(&app).contains("project:launch"),
            "an unresolved project is still named after its repo ROOT, which \
             for this plain checkout is the launch folder itself"
        );
    }

    /// The OTHER door into that same fallback: a set whose membership RESOLVED
    /// but that carries no label (`WorktreeSet::from_resolved(roots, None)`,
    /// reachable through the public constructor and the public `worktrees`
    /// field). The list side pins this state — see `App`'s
    /// `a_resolved_set_with_no_label_still_draws_one_head_named_from_the_repo_root`
    /// — but the header side did not, so the two halves of "head and header
    /// agree" rested on reading two implementations of one naming rule rather
    /// than on an assertion here.
    ///
    /// Distinct from the unresolved case above in the premise, not the text:
    /// membership resolved, so the list draws GROUPED under one head, and the
    /// header must name the launch dir for that head to have anything to agree
    /// with.
    #[test]
    fn project_scope_header_names_the_launch_dir_when_a_resolved_set_has_no_label() {
        let mut app = App::new(
            vec![sample_session()],
            Scope::Project,
            PathBuf::from("/tmp/launch"),
        );
        app.worktrees = crate::worktrees::WorktreeSet::from_resolved(
            [PathBuf::from("/tmp/launch"), PathBuf::from("/tmp/other-wt")],
            None,
        );

        assert!(
            !app.worktrees.is_empty(),
            "premise: membership DID resolve — this is not the unresolved case"
        );
        assert_eq!(
            app.worktrees.label(),
            None,
            "premise: and it resolved without a label"
        );

        let header = drawn_header(&app);
        assert!(
            header.contains("project:launch"),
            "a resolved set with no label still names the launch dir: {header}"
        );
        assert_eq!(
            app.project_head().as_deref(),
            Some(project_name(&app).as_str()),
            "and the one group head reads exactly as the header does"
        );
    }

    /// The header names the PROJECT, and a worktree folder is named after its
    /// BRANCH — so when git resolved no label, the fallback must climb to the
    /// repo root rather than print the branch. Otherwise a `-p` board launched
    /// from `.agents/worktrees/feature/quick-send` announces itself as
    /// `project:quick-send` over a list drawn from the whole of `snapback`.
    ///
    /// This is now a REACHABLE state rather than a curiosity: an unresolved set
    /// no longer collapses the project scope to one folder, so the fallback name
    /// heads a genuinely cross-worktree list.
    #[test]
    fn project_scope_header_names_the_repo_root_not_the_worktree_launched_from() {
        let launch = PathBuf::from(
            "/Volumes/Development/ilfroloff/snapback/.agents/worktrees/feature/quick-send",
        );
        // The test-default probe resolves nothing: no `git`, or not a repo.
        let app = App::new(vec![sample_session()], Scope::Project, launch);

        assert!(app.worktrees.is_empty(), "premise: nothing resolved");

        let header = drawn_header(&app);
        assert!(
            header.contains("project:snapback"),
            "the header names the project, not the branch folder: {header}"
        );
        assert!(
            !header.contains("project:quick-send"),
            "naming the branch would misdescribe a whole-project list: {header}"
        );
        assert_eq!(
            app.project_head().as_deref(),
            Some(project_name(&app).as_str()),
            "and the one group head still reads exactly as the header does"
        );
    }

    /// The one launch dir where head and header could drift: a path with no UTF-8
    /// spelling. The header repairs it with `to_string_lossy`; the head must make
    /// the same repair, or a `-p` board names one project two different things at
    /// once — a head reading one way and the header above it reading another.
    /// The head can only follow because `App`'s `project_label` returns an owned
    /// `String`; a borrowed name cannot carry a repair.
    ///
    /// `#[cfg(unix)]` because only there can a `PathBuf` be built from bytes that
    /// are not UTF-8.
    #[cfg(unix)]
    #[test]
    fn head_and_header_name_a_non_utf8_launch_dir_the_same_way() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let mut app = App::new(
            vec![sample_session()],
            Scope::Project,
            PathBuf::from(OsStr::from_bytes(b"/tmp/\xff")),
        );
        app.worktrees =
            crate::worktrees::WorktreeSet::from_resolved([PathBuf::from("/any/root")], None);

        let name = project_name(&app);
        assert_eq!(
            name, "\u{FFFD}",
            "the header repairs an unspellable name rather than dropping it"
        );
        assert_eq!(
            app.project_head().as_deref(),
            Some(name.as_str()),
            "the one group head must read exactly as the header does"
        );
        assert!(
            drawn_header(&app).contains(&format!("project:{name}")),
            "and that is what the board actually paints"
        );
    }

    /// An empty board's ONLY advice is this sentence, so it has to name the
    /// scope `Ctrl-A` actually reaches from here — the next state in the cycle,
    /// not the widest one. Derived from [`Scope::toggled`] rather than restated,
    /// so adding a fourth scope cannot leave the advice one key stale.
    #[test]
    fn an_empty_list_points_at_the_scope_ctrl_a_reaches_next() {
        assert_eq!(
            Scope::CurrentFolder.toggled(true),
            Scope::Project,
            "the cycle this advice describes"
        );
        assert!(
            empty_list_message(Scope::CurrentFolder, true).contains("project"),
            "from the folder scope Ctrl-A widens to the PROJECT, not to all folders"
        );

        assert_eq!(Scope::Project.toggled(true), Scope::All);
        assert!(
            empty_list_message(Scope::Project, true).contains("all folders"),
            "from the project scope Ctrl-A widens to all folders"
        );

        assert!(
            !empty_list_message(Scope::All, true).contains("Ctrl-A"),
            "the widest scope has nothing to widen to, so it must not offer the key"
        );
    }

    /// The same rule under the DEFAULT launch, where `Ctrl-A` cannot reach the
    /// all scope at all: the advice must stop promising a destination the key no
    /// longer has. Derived from [`Scope::toggled`] for the same reason — a
    /// sentence naming a scope the key does not reach is worse than no sentence,
    /// because an empty board has nothing else to go on.
    #[test]
    fn an_empty_project_stops_promising_all_folders_without_the_launch_flag() {
        assert_eq!(
            Scope::Project.toggled(false),
            Scope::CurrentFolder,
            "the cycle this advice describes: no `-a`, so the key NARROWS here"
        );
        let msg = empty_list_message(Scope::Project, false);
        assert!(
            !msg.contains("all folders"),
            "the key cannot show all folders on this launch, so the board must \
             not offer it: {msg}"
        );
        assert!(
            !msg.contains("Ctrl-A"),
            "and there is nothing wider to point at, so it names no key at all: \
             {msg}"
        );

        assert!(
            empty_list_message(Scope::CurrentFolder, false).contains("project"),
            "the folder scope still widens to the project either way — the flag \
             takes away the third stop, not the first"
        );
    }

    // --- header counter: lineages on both sides, and the hidden segment -----

    /// The launch dir every counter case below starts in. A plain checkout, so
    /// `worktrees::project_root` resolves it to itself and the worktree paths
    /// underneath it collapse onto the same root — the git-free arm of
    /// `App::in_scope`, which is the only one available under test (the test
    /// worktree probe resolves nothing).
    const COUNTER_LAUNCH: &str = "/tmp/sbcount-proj";

    /// A `Session` at `cwd` for the counter cases. Only the id and the `cwd`
    /// decide what the counter does, so everything else is inert — except
    /// `root_uuid`, which the fold case needs and which every other case must
    /// leave at `None` so its rows stay unfolded.
    fn counted_session(id: &str, cwd: &str, root_uuid: Option<&str>) -> Session {
        Session {
            file: PathBuf::from(format!("/tmp/{id}.jsonl")),
            session_id: id.to_string(),
            cwd: PathBuf::from(cwd),
            git_branch: Some("main".to_string()),
            timestamp: None,
            repo: "sbcount-proj".to_string(),
            label: id.to_string(),
            root_uuid: root_uuid.map(ToString::to_string),
            msg_count: 0,
            content_index: String::new(),
        }
    }

    /// A store whose four sessions separate the three populations the counter
    /// could plausibly measure: ONE in the launch folder, TWO more in worktrees
    /// of the same project, and one in an unrelated project. So `1` is the
    /// folder's own count, `3` the project's, and `4` the store's — three
    /// distinct numbers, which is what makes a wrong denominator visible.
    ///
    /// The ids carry a `sbcount-` prefix because `App::new` loads the REAL
    /// hidden set from `$SNAPBACK_CONFIG_DIR` unless a case overrides it, and a
    /// collision there would silently move the counts.
    fn counter_store() -> Vec<Session> {
        vec![
            counted_session("sbcount-here", COUNTER_LAUNCH, None),
            counted_session(
                "sbcount-wt1",
                "/tmp/sbcount-proj/.agents/worktrees/feat",
                None,
            ),
            counted_session(
                "sbcount-wt2",
                "/tmp/sbcount-proj/.agents/worktrees/fix",
                None,
            ),
            counted_session("sbcount-away", "/tmp/sbcount-other", None),
        ]
    }

    /// A board over [`counter_store`] in `scope`.
    fn counter_board(scope: Scope) -> App {
        App::new(counter_store(), scope, PathBuf::from(COUNTER_LAUNCH))
    }

    /// `--all` is the one scope that is not about a project, so its denominator
    /// stays the whole store — the counter exactly as it read before the project
    /// population existed.
    #[test]
    fn the_all_scope_counter_still_measures_the_whole_store() {
        let app = counter_board(Scope::All);

        let header = drawn_header(&app);
        assert!(
            header.contains("4 / 4 sessions"),
            "the all scope counts every session in the store: {header}"
        );
        assert!(
            !header.contains("hidden"),
            "nothing is hidden, so no segment is drawn at all: {header}"
        );
    }

    /// The default scope's denominator is deliberately WIDER than its own rows:
    /// it counts the PROJECT, so the header says how much a `Ctrl-A` widen would
    /// reveal instead of advertising every session on the machine (the old
    /// `sessions.len()` denominator) or restating the row count.
    #[test]
    fn the_folder_scope_counter_measures_the_whole_project() {
        let app = counter_board(Scope::CurrentFolder);

        let header = drawn_header(&app);
        assert!(
            header.contains("1 / 3 sessions"),
            "one row drawn, three in the project: {header}"
        );
        assert!(
            !header.contains("1 / 1 sessions"),
            "the folder's own count would make the denominator say nothing: \
             {header}"
        );
        assert!(
            !header.contains("1 / 4 sessions"),
            "and the store total counts a session from another project: {header}"
        );
        assert!(
            !header.contains("hidden"),
            "nothing is hidden here, so the segment is absent entirely — a \
             `· 0 hidden` would sit on every board that never hid a row: {header}"
        );
    }

    /// Widening to the project must not move the denominator — it is the same
    /// population either way, which is the whole point of counting it in the
    /// narrow scope. Only the NUMERATOR catches up.
    #[test]
    fn the_project_scope_counter_measures_the_same_population_as_the_folder_scope() {
        let folder = counter_board(Scope::CurrentFolder);
        let project = counter_board(Scope::Project);

        assert_eq!(
            folder.session_counts().total,
            project.session_counts().total,
            "one project, one denominator, whichever side of Ctrl-A you are on"
        );
        assert_eq!(
            folder.session_counts().hidden,
            project.session_counts().hidden,
            "and one hidden segment with it"
        );
        assert!(
            drawn_header(&project).contains("3 / 3 sessions"),
            "and the widened board draws every session it was counting"
        );
    }

    /// A soft-hidden session leaves the denominator (the board cannot show it)
    /// and is accounted for in its own trailing segment, so the two numbers
    /// still add up to the project's real size — in lineages, which for this
    /// rootless fixture is one per file.
    #[test]
    fn a_hidden_session_leaves_the_denominator_for_its_own_segment() {
        let mut app = counter_board(Scope::CurrentFolder);
        app.hidden_ids.insert("sbcount-wt1".to_string());
        // A reload is the public path that re-runs the whole pipeline; the
        // counts must survive it without a scope toggle (see the caching case
        // below).
        app.apply_sessions(counter_store());

        let header = drawn_header(&app);
        assert!(
            header.contains("1 / 2 sessions"),
            "the hidden project session is out of the denominator: {header}"
        );
        assert!(
            header.contains("1 hidden"),
            "and it is disclosed instead of vanishing: {header}"
        );
        let counts = app.session_counts();
        assert_eq!(
            counts.total + counts.hidden,
            3,
            "the two numbers must reconcile to the project's lineages"
        );
    }

    /// With show-hidden on the rows are back on the board, so they belong INSIDE
    /// the denominator — and the segment must go away, or the header counts the
    /// same visible rows twice.
    #[test]
    fn revealing_hidden_rows_folds_them_back_into_the_denominator() {
        let mut app = counter_board(Scope::CurrentFolder);
        app.hidden_ids.insert("sbcount-wt1".to_string());
        app.apply_sessions(counter_store());
        app.toggle_show_hidden();

        let header = drawn_header(&app);
        assert!(
            header.contains("1 / 3 sessions"),
            "a revealed session is counted like any other: {header}"
        );
        assert!(
            !header.contains("hidden"),
            "so disclosing it separately would double-count it: {header}"
        );
    }

    /// A fork lineage is ONE conversation on BOTH sides of the `/`. Its members
    /// are one row on screen — the head, wearing the `(+N)` that advertises the
    /// rest — so counting the files behind that row into the denominator is what
    /// printed `115 / 146` on a board of 115 rows.
    #[test]
    fn a_folded_fork_lineage_counts_once_on_both_sides() {
        let store = vec![
            counted_session("sbcount-fork-a", COUNTER_LAUNCH, Some("root-1")),
            counted_session("sbcount-fork-b", COUNTER_LAUNCH, Some("root-1")),
        ];
        let app = App::new(store, Scope::Project, PathBuf::from(COUNTER_LAUNCH));

        assert_eq!(
            app.filtered.len(),
            1,
            "premise: the lineage folds to a single head"
        );
        assert!(
            app.query.is_empty(),
            "premise: nothing is filtered by a query"
        );
        let header = drawn_header(&app);
        assert!(
            header.contains("1 / 1 sessions"),
            "one conversation, drawn and counted: {header}"
        );
        assert!(
            !header.contains("1 / 2 sessions"),
            "the two FILES behind that row are not two rows the board could show: \
             {header}"
        );
    }

    /// Opening a `(+N)` family re-emits its members into `filtered`, and the
    /// counter must not notice. Beyond the arithmetic this is a stability
    /// property: `restore_selection` -> `reveal_hidden` auto-expands on
    /// autorefresh, so a fold-sensitive header would move on its own whenever a
    /// background job appended to a transcript.
    #[test]
    fn expanding_a_lineage_leaves_the_header_untouched() {
        let store = vec![
            counted_session("sbcount-fork-a", COUNTER_LAUNCH, Some("root-1")),
            counted_session("sbcount-fork-b", COUNTER_LAUNCH, Some("root-1")),
            counted_session("sbcount-lone", COUNTER_LAUNCH, None),
        ];
        let mut app = App::new(store, Scope::Project, PathBuf::from(COUNTER_LAUNCH));
        let folded = drawn_header(&app);
        assert!(
            folded.contains("2 / 2 sessions"),
            "premise: two conversations, both drawn: {folded}"
        );

        assert_eq!(
            app.selected.as_deref(),
            Some("sbcount-fork-a"),
            "premise: the fold's head is the row the expand acts on"
        );
        app.expand_selected();

        assert_eq!(
            app.filtered.len(),
            3,
            "premise: the expand really did add a row"
        );
        assert_eq!(
            drawn_header(&app),
            folded,
            "an expanded lineage is still one conversation"
        );
    }

    /// The hidden segment counts CONVERSATIONS too, and only fully hidden ones: a
    /// lineage with one member hidden and another still drawing is a visible row,
    /// so it belongs in the denominator and discloses nothing.
    #[test]
    fn a_partially_hidden_lineage_is_counted_as_a_visible_one() {
        let store = || {
            vec![
                counted_session("sbcount-fork-a", COUNTER_LAUNCH, Some("root-1")),
                counted_session("sbcount-fork-b", COUNTER_LAUNCH, Some("root-1")),
            ]
        };
        let mut app = App::new(store(), Scope::Project, PathBuf::from(COUNTER_LAUNCH));
        app.hidden_ids.insert("sbcount-fork-b".to_string());
        app.apply_sessions(store());

        let header = drawn_header(&app);
        assert!(
            header.contains("1 / 1 sessions"),
            "the half-hidden conversation is still a row on the board: {header}"
        );
        assert!(
            !header.contains("hidden"),
            "and nothing about it is hidden from the user: {header}"
        );
    }

    /// The caching guard. The population is resolved ONCE per reload/scope
    /// toggle because deciding it canonicalizes every `cwd`; the hidden split is
    /// not cached with it. So a hide, a reveal, an un-hide and a reload must each
    /// leave the counter truthful with NO scope toggle anywhere in between —
    /// none of these paths runs `recompute_scope` except the reload at the end.
    #[test]
    fn the_counts_survive_hiding_revealing_and_reloading_without_a_scope_toggle() {
        let _guard = crate::config::env_lock();
        let dir = unique_temp_dir("header-counts");
        std::env::set_var("SNAPBACK_CONFIG_DIR", &dir);

        // Two rows in the launch folder, so hiding one leaves a selection to
        // stand on; the third project session keeps the denominator wider than
        // the folder throughout.
        let store = || {
            vec![
                counted_session("sbcount-here-a", COUNTER_LAUNCH, None),
                counted_session("sbcount-here-b", COUNTER_LAUNCH, None),
                counted_session(
                    "sbcount-wt1",
                    "/tmp/sbcount-proj/.agents/worktrees/feat",
                    None,
                ),
            ]
        };
        let mut app = App::new(store(), Scope::CurrentFolder, PathBuf::from(COUNTER_LAUNCH));
        assert!(
            drawn_header(&app).contains("2 / 3 sessions"),
            "premise: two rows drawn out of a three-session project"
        );

        // Hide the second row. `toggle_hidden_selected` never recomputes the
        // scope, so a cached hidden split would go stale right here.
        app.move_selection(1);
        assert_eq!(app.selected.as_deref(), Some("sbcount-here-b"));
        app.toggle_hidden_selected();
        let header = drawn_header(&app);
        assert!(
            header.contains("1 / 2 sessions") && header.contains("1 hidden"),
            "a hide re-splits the cached population on the spot: {header}"
        );

        // Reveal, un-hide the row, and put the reveal back the way it was.
        app.toggle_show_hidden();
        app.move_selection(1);
        assert_eq!(app.selected.as_deref(), Some("sbcount-here-b"));
        app.toggle_hidden_selected();
        app.toggle_show_hidden();
        let header = drawn_header(&app);
        assert!(
            header.contains("2 / 3 sessions"),
            "un-hiding puts the session back in the denominator: {header}"
        );
        assert!(
            !header.contains("hidden"),
            "and nothing is left to disclose: {header}"
        );

        // A reload is the OTHER path that must not need a toggle: it rebuilds
        // the population itself.
        app.apply_sessions(store());
        assert!(
            drawn_header(&app).contains("2 / 3 sessions"),
            "a reload rebuilds the same population without a scope toggle"
        );

        std::env::remove_var("SNAPBACK_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An isolated temp dir for the one counter case that PERSISTS a hide, so it
    /// never touches the real state dir. Mirrors the
    /// `snapback-<tag>-<pid>-<nanos>` convention used across the crate's tests.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "snapback-view-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn short_time_renders_year_month_day_hour_minute() {
        // 1_700_000_000 == 2023-11-14T22:13:20Z (fields rendered as stored).
        let t = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        assert_eq!(short_time(Some(t)), "2023-11-14 22:13");
    }

    #[test]
    fn short_time_none_is_placeholder() {
        assert_eq!(short_time(None), "--");
    }

    // --- preview wrapped-height math --------------------------------------

    /// The width every measurement case below is wrapped at. Narrow enough that
    /// ordinary words have to break across rows, which is where the two candidate
    /// models part company.
    const MEASURE_WIDTH: u16 = 10;
    /// Rows the scratch buffer in [`painted_rows`] offers. Comfortably more than any
    /// case needs, so a model that OVER-counts is caught by the comparison rather
    /// than silently clipped by the buffer.
    const MEASURE_ROWS: u16 = 40;

    /// Rows the RENDERER actually painted for `lines` at [`MEASURE_WIDTH`]: one past
    /// the last row carrying a non-blank cell.
    ///
    /// The ground truth every height claim below is checked against — read off the
    /// drawn buffer, never asked of the same code under test. Every case here ends on
    /// a non-blank line so "last painted row" is well defined (a case that ended on a
    /// blank line would be indistinguishable from one row fewer).
    fn painted_rows(lines: &[Line<'static>]) -> usize {
        let area = Rect {
            x: 0,
            y: 0,
            width: MEASURE_WIDTH,
            height: MEASURE_ROWS,
        };
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::Widget::render(
            Paragraph::new(Text::from(lines.to_vec())).wrap(Wrap { trim: false }),
            area,
            &mut buffer,
        );
        (0..MEASURE_ROWS)
            .rev()
            .find(|&y| {
                (0..MEASURE_WIDTH)
                    .any(|x| buffer.cell((x, y)).is_some_and(|cell| cell.symbol() != " "))
            })
            .map_or(0, |y| usize::from(y) + 1)
    }

    /// Measurement cases, each `(name, lines)`. Between them they cover a line that
    /// fits, an exact multiple of the width, a blank line in the middle (it still
    /// costs a row), a WORD-WRAPPING line (where character packing under-counts), and
    /// an unbreakable word (where the wrapper falls back to breaking mid-word).
    fn measure_cases() -> Vec<(&'static str, Vec<Line<'static>>)> {
        let line = |s: &str| Line::from(s.to_string());
        vec![
            ("fits the width", vec![line("short")]),
            ("exactly the width", vec![line("0123456789")]),
            (
                "a blank line between two full ones",
                vec![line("0123456789"), line(""), line("abcdefghij")],
            ),
            (
                "words that must break across rows",
                vec![line("alpha bravo charlie delta")],
            ),
            (
                "one unbreakable word",
                vec![line("xxxxxxxxxxxxxxxxxxxxxxxxx")],
            ),
            (
                "several turns of prose",
                vec![
                    line("the quick brown fox"),
                    line("jumps over the lazy dog"),
                    line("end"),
                ],
            ),
        ]
    }

    /// The load-bearing verification: the height the transcript is scrolled against
    /// is the height the renderer paints, case for case.
    ///
    /// `Paragraph::line_count` is trusted here only because this pins it against the
    /// pinned `ratatui =0.30.2`'s own output — the two run the same `WordWrapper`, but
    /// "the same" is a claim about a private module, so it is checked rather than
    /// assumed.
    #[test]
    fn the_measured_height_is_the_height_the_renderer_paints() {
        for (name, lines) in measure_cases() {
            assert_eq!(
                wrapped_text_rows(&lines, MEASURE_WIDTH),
                painted_rows(&lines),
                "measured height must equal the rows drawn for {name:?}"
            );
        }
    }

    /// APPROXIMATE wrapped row count of ONE logical line of display `width` at
    /// `inner_width`: `ceil(width / inner_width)`, and at least one row (a blank line
    /// still takes a row).
    ///
    /// A TEST FOIL, and only that — which is why it lives in `mod tests`. No
    /// production path models a wrap any more: the transcript's height AND the
    /// per-line map of where each line starts are both asked of the widget
    /// (`wrapped_text_rows` / `wrapped_row_prefix`), and the click hit-test resolves a
    /// clicked ROW through that same map. What survives of character packing in
    /// production is the COLUMN step inside `link_at`, over the ONE line the map
    /// already resolved exactly.
    ///
    /// It is kept because several fixtures below have to PROVE they are a case the
    /// two models disagree about: on a fixture where they happen to agree, a test
    /// cannot tell a right measurement from a wrong one, and passes for the wrong
    /// reason.
    fn wrapped_line_height(width: usize, inner_width: u16) -> usize {
        // Guard a zero-width viewport (degenerate layout) so we never divide by zero.
        let inner = usize::from(inner_width.max(1));
        width.div_ceil(inner).max(1)
    }

    /// The foil's own shape, pinned so a case built on it cannot be arguing from a
    /// broken model of the thing it claims to differ from.
    #[test]
    fn wrapped_line_height_is_ceil_over_inner_width_min_one() {
        assert_eq!(
            wrapped_line_height(0, 8),
            1,
            "a blank line still takes a row"
        );
        assert_eq!(wrapped_line_height(7, 8), 1, "shorter than width => 1 row");
        assert_eq!(wrapped_line_height(16, 8), 2, "an exact multiple is 2 rows");
        assert_eq!(wrapped_line_height(17, 8), 3, "one over wraps to a 3rd row");
        assert_eq!(
            wrapped_line_height(5, 0),
            5,
            "zero inner width divides by 1"
        );
    }

    /// The character-packing model is not a safe approximation of word wrap in EITHER
    /// direction, which is why the transcript had to stop using it.
    ///
    /// It under-counts ordinary prose (a row that ends early at a word boundary costs
    /// a row it never charged for) — the case that made the newest turn unreachable —
    /// and it over-counts a very narrow pane, where the wrapper drops the whitespace
    /// it broke on and packing still bills for it. Both are asserted against the rows
    /// actually PAINTED, so neither is a claim about a model.
    ///
    /// Without this the agreement test above could pass while both models happened to
    /// agree, proving nothing about which one is in use.
    #[test]
    fn character_packing_and_word_wrap_disagree_in_both_directions() {
        let packed_rows = |lines: &[Line<'static>], width: u16| -> usize {
            lines
                .iter()
                .map(|l| wrapped_line_height(l.width(), width))
                .sum()
        };

        // UNDER-count: 25 columns of words in a 10-column pane packs to 3 rows, but
        // each row ends at a word boundary, so four are painted.
        let prose = vec![Line::from("alpha bravo charlie delta".to_string())];
        assert_eq!(packed_rows(&prose, MEASURE_WIDTH), 3);
        assert_eq!(wrapped_text_rows(&prose, MEASURE_WIDTH), 4);
        assert_eq!(painted_rows(&prose), 4, "and 4 is what reaches the screen");

        // OVER-count: at one column the wrapper swallows the space it breaks on, so
        // the 11-column line needs only its 10 non-blank cells.
        let narrow = [Line::from("alpha bravo".to_string())];
        assert_eq!(packed_rows(&narrow, 1), 11);
        assert_eq!(wrapped_text_rows(&narrow, 1), 10);
    }

    /// A wrapped row count is ADDITIVE over logical lines: the wrapper breaks each
    /// one on its own and never packs two of them onto a shared row.
    ///
    /// `render_preview` leans on this to add the in-flight reply tail's rows to the
    /// CACHED transcript count instead of re-wrapping the whole transcript every
    /// frame while a send is running.
    #[test]
    fn a_wrapped_row_count_is_additive_over_lines() {
        for (name, lines) in measure_cases() {
            let split_at = lines.len() / 2;
            let (head, tail) = lines.split_at(split_at);
            assert_eq!(
                wrapped_text_rows(&lines, MEASURE_WIDTH),
                wrapped_text_rows(head, MEASURE_WIDTH) + wrapped_text_rows(tail, MEASURE_WIDTH),
                "measuring {name:?} in two parts must total the whole"
            );
        }
    }

    /// A degenerate zero-width pane measures zero rows — which is what the renderer
    /// paints there (no column to paint into), so the scroll clamp and the scrollbar
    /// both fall through to "nothing to scroll" rather than to a divide-by-zero or an
    /// invented height.
    #[test]
    fn a_zero_width_pane_measures_no_rows() {
        assert_eq!(
            wrapped_text_rows(&[Line::from("anything at all".to_string())], 0),
            0
        );
    }

    // --- the wrapped-row prefix map ---------------------------------------

    /// Measurement cases that go past ASCII: CJK and emoji occupy TWO terminal
    /// columns each, so where the wrapper breaks them is a function of width in a way
    /// a byte or char count cannot predict — the case a per-line measurement is most
    /// likely to disagree with a whole-text one on.
    fn wide_glyph_cases() -> Vec<(&'static str, Vec<Line<'static>>)> {
        let line = |s: &str| Line::from(s.to_string());
        vec![
            (
                "CJK filling the width exactly",
                vec![line("\u{4f60}\u{597d}\u{4e16}\u{754c}\u{518d}")],
            ),
            (
                "CJK straddling the width",
                vec![line(
                    "\u{4f60}\u{597d}\u{4e16}\u{754c}\u{518d}\u{89c1}\u{4e86}",
                )],
            ),
            (
                "emoji between ASCII words",
                vec![
                    line("ship \u{1f680} it"),
                    line("\u{1f600}\u{1f601}\u{1f602}\u{1f603}\u{1f604}\u{1f605}"),
                    line("done"),
                ],
            ),
            (
                "mixed scripts on one line",
                vec![line("build \u{4f60}\u{597d} now \u{1f680} ok")],
            ),
        ]
    }

    /// THE TRIPWIRE the whole prefix map rests on: measuring each logical line ON ITS
    /// OWN and summing gives exactly the whole-text count.
    ///
    /// `Wrap { trim: false }` runs `WordWrapper`, which breaks each logical line
    /// independently and never joins two of them onto a shared row — so a wrapped row
    /// count is additive, and `wrapped_row_prefix`'s per-line walk can stand in for
    /// the whole-text measurement it replaced. That is a claim about a PRIVATE ratatui
    /// module (`ratatui_widgets::reflow`), held by a `=0.30.2` pin. If a bump ever
    /// changes it, every offset the pane computes — the scroll clamp, the search jump,
    /// the window's own start row — goes quietly wrong at once. This is the test that
    /// has to go red first, which is why it is driven over wrapping ASCII AND
    /// double-width CJK/emoji rather than the easy cases.
    #[test]
    fn per_line_wrapped_row_counts_sum_to_the_whole_text_count() {
        for (name, lines) in measure_cases().into_iter().chain(wide_glyph_cases()) {
            let prefix = wrapped_row_prefix(&lines, MEASURE_WIDTH);
            assert_eq!(
                prefix.len(),
                lines.len() + 1,
                "the map must describe one more position than there are lines ({name:?})"
            );
            assert_eq!(prefix.first().copied(), Some(0), "the map starts at row 0");
            assert_eq!(
                prefix.last().copied(),
                Some(wrapped_text_rows(&lines, MEASURE_WIDTH)),
                "per-line counts must sum to the whole-text count for {name:?}"
            );
            // And every intermediate entry, not just the total: a map that only got
            // the sum right could still start a window on the wrong row.
            for n in 0..=lines.len() {
                assert_eq!(
                    prefix[n],
                    wrapped_text_rows(&lines[..n], MEASURE_WIDTH),
                    "entry {n} of {name:?} must be the wrapped height of the lines above it"
                );
            }
        }
    }

    /// Marking a line SPLITS its spans, and that must not move a single row.
    ///
    /// The prefix map is built at cache fill, over the UNMARKED transcript, and then
    /// used to window and scroll a MARKED one. That only holds because
    /// `highlight_matched_spans` re-styles without touching text: were a span split a
    /// break opportunity for the wrapper, every offset below a mark would drift by the
    /// rows the split added, and only on a searched pane.
    #[test]
    fn splitting_a_line_at_its_marks_does_not_change_its_wrapped_height() {
        for (name, lines) in measure_cases().into_iter().chain(wide_glyph_cases()) {
            // Mark every other char, which is the worst case: it splits each span at
            // as many cluster boundaries as the line has.
            let marked: HashSet<usize> = (0..200).step_by(2).collect();
            let highlighted: Vec<Line<'static>> = lines
                .iter()
                .map(|line| highlight_matched_spans(line, &marked, PREVIEW_MATCH_MODIFIER))
                .collect();
            assert!(
                highlighted.iter().any(|line| line.spans.len() > 1),
                "the fixture must really have been split, or this proves nothing ({name:?})"
            );
            assert_eq!(
                wrapped_row_prefix(&highlighted, MEASURE_WIDTH),
                wrapped_row_prefix(&lines, MEASURE_WIDTH),
                "marking {name:?} must not move a row"
            );
        }
    }

    /// The window resolution, stated as arithmetic over a prefix map: two lines of one
    /// row, then one of three rows, then one of one row — seven rows over four lines.
    #[test]
    fn row_window_finds_the_line_holding_an_offset_and_the_rows_left_inside_it() {
        // rows: line 0 -> [0], line 1 -> [1], line 2 -> [2,3,4], line 3 -> [5]
        let prefix = [0usize, 1, 2, 5, 6];

        // TOP: the window starts at line 0 with nothing to skip, and reaches every
        // line that begins inside the viewport.
        assert_eq!(row_window(&prefix, 0, 2), (0..2, 0));
        assert_eq!(row_window(&prefix, 0, 6), (0..4, 0));

        // MIDDLE, on a line boundary: no residual.
        assert_eq!(row_window(&prefix, 2, 3), (2..3, 0));

        // MIDDLE, INSIDE a wrapped line: the window still starts at that line, and
        // the leftover rows become the residual the widget is scrolled by.
        assert_eq!(row_window(&prefix, 3, 2), (2..3, 1));
        assert_eq!(row_window(&prefix, 4, 2), (2..4, 2));

        // BOTTOM: the last line alone.
        assert_eq!(row_window(&prefix, 5, 4), (3..4, 0));

        // PAST THE END: an empty window rather than an out-of-bounds slice.
        assert_eq!(row_window(&prefix, 6, 4), (4..4, 0));
        assert_eq!(row_window(&prefix, 99, 4), (4..4, 93));

        // A zero-height viewport keeps the line the offset landed in and no more, so
        // the range can never invert (a degenerate layout must not panic on a slice).
        assert_eq!(row_window(&prefix, 3, 0), (2..3, 1));
    }

    /// The window's start row is EXACTLY the offset asked for: `row_prefix[start]`
    /// plus the residual. This is the identity the whole draw rests on — the pane
    /// paints from `offset` whether the widget was handed the transcript or a slice of
    /// it — so it is asserted over every measurement fixture at every reachable offset
    /// rather than at a few sampled ones.
    #[test]
    fn a_window_start_plus_its_residual_is_the_offset_it_was_asked_for() {
        for (name, lines) in measure_cases().into_iter().chain(wide_glyph_cases()) {
            let prefix = wrapped_row_prefix(&lines, MEASURE_WIDTH);
            let total = prefix.last().copied().expect("a non-empty prefix map");
            for offset in 0..total {
                let (range, residual) = row_window(&prefix, offset, 3);
                assert_eq!(
                    prefix[range.start] + residual,
                    offset,
                    "{name:?} at offset {offset} must start on the row it was asked for"
                );
                assert!(
                    range.start < lines.len(),
                    "{name:?} at offset {offset} must land on a real line"
                );
                assert!(
                    residual < prefix[range.start + 1] - prefix[range.start],
                    "{name:?} at offset {offset} must leave a residual INSIDE its line, \
                     not past it"
                );
            }
        }
    }

    // --- preview link hit-testing (content<->screen mapping) --------------

    #[test]
    fn visual_to_content_maps_rows_across_wrapped_lines() {
        // Line 0 occupies 3 rows, line 1 one row, line 2 one row.
        let prefix = [0usize, 3, 4, 5];
        assert_eq!(visual_to_content(&prefix, 0), Some((0, 0)));
        assert_eq!(
            visual_to_content(&prefix, 2),
            Some((0, 2)),
            "3rd wrap row of line 0"
        );
        assert_eq!(
            visual_to_content(&prefix, 3),
            Some((1, 0)),
            "line 1 starts after 3 rows"
        );
        assert_eq!(visual_to_content(&prefix, 4), Some((2, 0)));
        assert_eq!(
            visual_to_content(&prefix, 5),
            None,
            "past the end of content"
        );
    }

    /// The map, not a model, decides which line a row belongs to — so a line the
    /// WRAPPER broke early is followed exactly instead of drifting.
    ///
    /// The prefix below is one no `ceil(width / inner)` walk can produce for these
    /// widths: at inner width 20, packing calls each of these 21-cell lines 2 rows,
    /// while the wrapper paints 3. Row 6 is line 2's first row by the map and line 3's
    /// by the model, and the gap grows by one with every wrapping line above it —
    /// which is the drift a hit-test used to inherit over a whole transcript.
    #[test]
    fn visual_to_content_follows_the_map_where_a_packing_model_would_have_drifted() {
        let widths = [21usize; 4];
        let packed: Vec<usize> = widths.iter().map(|&w| wrapped_line_height(w, 20)).collect();
        assert_eq!(
            packed,
            vec![2, 2, 2, 2],
            "the model would say two rows each"
        );
        // What the wrapper actually does with them: three rows each.
        let prefix = [0usize, 3, 6, 9, 12];
        assert_eq!(
            visual_to_content(&prefix, 6),
            Some((2, 0)),
            "row 6 opens line 2; a packed walk reaches line 3 by then"
        );
        assert_eq!(
            visual_to_content(&prefix, 11),
            Some((3, 2)),
            "and the last row belongs to the last line, not past the end"
        );
        assert_eq!(visual_to_content(&prefix, 12), None, "past the end");
    }

    /// A degenerate map answers `None` rather than indexing into nothing: an empty
    /// pane (no selection) and a transcript with no lines both arrive here.
    #[test]
    fn visual_to_content_is_none_for_a_map_that_describes_no_lines() {
        assert_eq!(visual_to_content(&[], 0), None, "no map at all");
        assert_eq!(visual_to_content(&[0], 0), None, "a map of zero lines");
    }

    /// A 20-wide inner pane at origin (1,1); most link tests share it.
    fn inner_rect() -> Rect {
        Rect {
            x: 1,
            y: 1,
            width: 20,
            height: 10,
        }
    }

    fn region(content_row: usize, col_start: usize, col_end: usize, url: &str) -> LinkRegion {
        LinkRegion {
            content_row,
            col_start,
            col_end,
            url: url.to_string(),
        }
    }

    #[test]
    fn link_at_returns_url_inside_a_link_and_none_just_outside() {
        let inner = inner_rect();
        // Three unwrapped content lines; a link on line 2 at columns 4..8.
        let prefix = [0usize, 1, 2, 3];
        let regions = [region(2, 4, 8, "u")];
        // Inside the label (content col 4..7) -> the url.
        assert_eq!(
            link_at(inner.x + 4, inner.y + 2, inner, 0, &prefix, &regions),
            Some("u")
        );
        assert_eq!(
            link_at(inner.x + 7, inner.y + 2, inner, 0, &prefix, &regions),
            Some("u")
        );
        // One cell past the end (col_end is exclusive) -> None.
        assert_eq!(
            link_at(inner.x + 8, inner.y + 2, inner, 0, &prefix, &regions),
            None
        );
        // One cell before the start -> None.
        assert_eq!(
            link_at(inner.x + 3, inner.y + 2, inner, 0, &prefix, &regions),
            None
        );
    }

    #[test]
    fn link_at_is_none_on_blank_rows_and_outside_the_pane() {
        let inner = inner_rect();
        let prefix = [0usize, 1, 2, 3];
        let regions = [region(2, 4, 8, "u")];
        // A click on the blank content line 1 hits no region.
        assert_eq!(
            link_at(inner.x + 4, inner.y + 1, inner, 0, &prefix, &regions),
            None
        );
        // A click left of the inner rect is rejected outright.
        assert_eq!(link_at(0, inner.y + 2, inner, 0, &prefix, &regions), None);
        // A click below the content (inside the pane, past the last line) -> None.
        assert_eq!(
            link_at(inner.x + 4, inner.y + 5, inner, 0, &prefix, &regions),
            None
        );
    }

    #[test]
    fn link_at_hits_a_soft_wrapped_link_on_its_second_visual_row() {
        let inner = inner_rect();
        // One content line occupying 3 visual rows (inner width 20). A link at
        // content columns 25..30 lives on the SECOND wrapped row.
        let prefix = [0usize, 3];
        let regions = [region(0, 25, 30, "w")];
        // Second visual row, column 7 => content col 20 + 7 = 27, inside 25..30.
        assert_eq!(
            link_at(inner.x + 7, inner.y + 1, inner, 0, &prefix, &regions),
            Some("w"),
            "a wrapped link is clickable on its second visual segment"
        );
        // The SAME column on the first visual row is content col 7 -> no link.
        assert_eq!(
            link_at(inner.x + 7, inner.y, inner, 0, &prefix, &regions),
            None
        );
    }

    #[test]
    fn link_at_respects_the_scroll_offset() {
        let inner = inner_rect();
        // Five unwrapped lines; a link on line 3 spanning columns 0..3.
        let prefix = [0usize, 1, 2, 3, 4, 5];
        let regions = [region(3, 0, 3, "s")];
        // Scrolled down 2 rows, screen row rel 1 => visual row 3 => content line 3.
        assert_eq!(
            link_at(inner.x + 1, inner.y + 1, inner, 2, &prefix, &regions),
            Some("s")
        );
        // Without the scroll, the same screen cell is content line 1 -> no link.
        assert_eq!(
            link_at(inner.x + 1, inner.y + 1, inner, 0, &prefix, &regions),
            None
        );
    }

    // --- preview offset clamping (no overflow / underflow) ----------------

    #[test]
    fn follow_bottom_pins_to_max_offset() {
        // content 20 rows in a 5-row viewport => bottom offset 15.
        assert_eq!(clamp_preview_offset(true, 0, 20, 5), 15);
        // The requested value is ignored while following the bottom.
        assert_eq!(clamp_preview_offset(true, 3, 20, 5), 15);
    }

    #[test]
    fn requested_offset_clamps_to_content_bounds() {
        // A request past the end clamps to max_offset (no runaway overflow).
        assert_eq!(clamp_preview_offset(false, 100, 20, 5), 15);
        // A request within range passes through untouched.
        assert_eq!(clamp_preview_offset(false, 4, 20, 5), 4);
    }

    #[test]
    fn short_content_never_underflows_below_zero() {
        // Content shorter than the viewport => max_offset 0, any request pinned 0.
        assert_eq!(clamp_preview_offset(false, 9, 3, 10), 0);
        assert_eq!(clamp_preview_offset(true, 0, 3, 10), 0);
    }

    /// A wrapped-row count is a function of the transcript's length AND the pane's
    /// width, so it has no `u16`-shaped bound: a long session in a narrow pane
    /// passes 65,535 rows. This is the case the offset domain was widened for, and
    /// the values are deliberately ABOVE `u16::MAX` — a test that merely used the
    /// wider TYPE would pass against the narrow implementation too.
    ///
    /// Every clause below is a distinct way the old width failed. `max_offset`
    /// saturated at 65,535, so "follow the bottom" stopped ~10,000 rows short of
    /// the newest turn and never reached it again. Every offset past 65,535
    /// collapsed onto that one value, so the whole tail of the transcript was a
    /// single indistinguishable position. And an over-request clamped to the
    /// ceiling rather than to the content's real last row.
    #[test]
    fn a_transcript_taller_than_u16_max_clamps_at_its_real_last_row() {
        // 75,535 wrapped rows in a 50-row viewport => a bottom offset of 75,485,
        // which is 9,950 rows BEYOND anything a `u16` offset could address.
        let content_h = usize::from(u16::MAX) + 10_000;
        let viewport_h = 50u16;
        let max_offset = 75_485u32;
        assert!(
            max_offset > u32::from(u16::MAX),
            "the fixture must exceed u16"
        );

        assert_eq!(
            clamp_preview_offset(true, 0, content_h, viewport_h),
            max_offset,
            "following the bottom must reach the transcript's real last row"
        );
        assert_eq!(
            clamp_preview_offset(false, 70_000, content_h, viewport_h),
            70_000,
            "an in-range offset past u16::MAX is a position, not a saturation"
        );
        assert_eq!(
            clamp_preview_offset(false, 200_000, content_h, viewport_h),
            max_offset,
            "an over-request clamps to the content's last row, not to a type ceiling"
        );
    }

    // --- scrollbar thumb detachment (no rounding "stuck at the edge") -----

    #[test]
    fn min_detach_distance_is_content_h_divided_by_track_length_rounded_up() {
        assert_eq!(min_detach_distance(500, 8), 63, "500/8 = 62.5, rounds up");
        assert_eq!(
            min_detach_distance(16, 4),
            4,
            "an exact multiple needs no rounding"
        );
    }

    #[test]
    fn min_detach_distance_guards_zero_track_length() {
        // A degenerate zero-length track divides by 1, not 0.
        assert_eq!(min_detach_distance(10, 0), 10);
    }

    /// Mirrors ratatui's own thumb-start formula (`Scrollbar`'s private
    /// `rounding_divide` + `thumb_length` + `thumb_start`, confirmed against
    /// `ratatui-widgets-0.3.2/src/scrollbar.rs`) closely enough to predict
    /// exactly which track row a given `ScrollbarState` position renders the
    /// thumb's TOP at, without depending on ratatui's private internals. Used
    /// only to make the "naive vs. remapped" contrast in the tests below
    /// self-checking.
    fn ratatui_thumb_start(
        position: usize,
        viewport_length: usize,
        content_h: usize,
        track_length: usize,
    ) -> usize {
        fn rounding_divide(numerator: usize, denominator: usize) -> usize {
            (numerator + denominator / 2) / denominator
        }
        let thumb_length =
            rounding_divide(viewport_length * track_length, content_h).clamp(1, track_length);
        rounding_divide(position * track_length, content_h).min(track_length - thumb_length)
    }

    #[test]
    fn scrollbar_thumb_position_pins_to_first_track_row_at_offset_zero() {
        assert_eq!(scrollbar_thumb_position(0, 490, 491, 500, 8), 0);
    }

    #[test]
    fn scrollbar_thumb_position_pins_to_last_track_row_at_max_offset() {
        // `content_length - 1` == `max_offset`, matching the exact-bottom-pin
        // contract already covered by `render_preview_pins_scrollbar_thumb_...`.
        assert_eq!(scrollbar_thumb_position(490, 490, 491, 500, 8), 490);
    }

    #[test]
    fn scrollbar_thumb_position_detaches_from_top_on_a_barely_scrolled_offset() {
        // A 500-row transcript in an 8-row track (viewport 10, max_offset 490):
        // naively feeding the real offset (1) straight through rounds onto the
        // very first track row.
        let (content_h, viewport_length, track_length) = (500usize, 10usize, 8usize);
        let naive = ratatui_thumb_start(1, viewport_length, content_h, track_length);
        assert_eq!(
            naive, 0,
            "sanity: the naive position (== offset) rounds onto the top row"
        );

        let remapped = scrollbar_thumb_position(1, 490, 491, content_h, track_length as u16);
        let remapped_start =
            ratatui_thumb_start(remapped, viewport_length, content_h, track_length);
        assert!(
            remapped_start >= 1,
            "remapped position {remapped} must not render at the top row (got {remapped_start})"
        );
    }

    #[test]
    fn scrollbar_thumb_position_detaches_from_bottom_on_a_barely_scrolled_offset() {
        // Same geometry, one row short of the bottom (offset 489 of 490).
        let (content_h, viewport_length, track_length) = (500usize, 10usize, 8usize);
        let naive = ratatui_thumb_start(489, viewport_length, content_h, track_length);
        assert_eq!(
            naive,
            track_length - 1,
            "sanity: the naive position still touches the last track row"
        );

        let remapped = scrollbar_thumb_position(489, 490, 491, content_h, track_length as u16);
        let remapped_start =
            ratatui_thumb_start(remapped, viewport_length, content_h, track_length);
        assert!(
            remapped_start < track_length - 1,
            "remapped position {remapped} must not render at the last track row \
             (got {remapped_start}, last is {})",
            track_length - 1
        );
    }

    // --- overlay centering ------------------------------------------------

    #[test]
    fn centered_rect_centers_and_clamps_to_the_area() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        };
        // A 62x7 box centers with symmetric margins.
        let r = centered_rect(area, 62, 7);
        assert_eq!((r.width, r.height), (62, 7));
        assert_eq!(r.x, (100 - 62) / 2);
        assert_eq!(r.y, (40 - 7) / 2);

        // A box larger than the area shrinks to fit rather than overflowing.
        let tiny = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 3,
        };
        let clamped = centered_rect(tiny, 62, 7);
        assert_eq!((clamped.width, clamped.height), (20, 3));
        assert_eq!((clamped.x, clamped.y), (0, 0));
    }

    // --- search-match highlight run splitting -----------------------------

    #[test]
    fn highlight_runs_splits_into_matched_and_unmatched_runs() {
        // "abcd" with chars 1 and 2 matched => a | bc | d.
        let matched: HashSet<usize> = [1, 2].into_iter().collect();
        assert_eq!(
            highlight_runs("abcd", &matched),
            vec![
                ("a".to_string(), false),
                ("bc".to_string(), true),
                ("d".to_string(), false),
            ]
        );
    }

    #[test]
    fn highlight_runs_all_unmatched_when_set_is_empty() {
        // No matches => one unmatched run spanning the whole label.
        let matched: HashSet<usize> = HashSet::new();
        assert_eq!(
            highlight_runs("abc", &matched),
            vec![("abc".to_string(), false)]
        );
    }

    #[test]
    fn highlight_runs_ignores_out_of_range_indices_without_panicking() {
        // A truncated label can leave match indices pointing past its end; those
        // must be ignored (never a slice/panic). Only index 0 is in range here.
        let matched: HashSet<usize> = [0, 9, 42].into_iter().collect();
        assert_eq!(
            highlight_runs("ab", &matched),
            vec![("a".to_string(), true), ("b".to_string(), false)],
            "index 0 highlights 'a'; 9 and 42 are out of range and ignored"
        );
    }

    #[test]
    fn highlight_runs_is_char_safe_for_multibyte_labels() {
        // "🚀 d": char 0 is the 4-byte emoji, char 2 is 'd'. Splitting on CHAR
        // boundaries (never byte offsets) must land exactly on those runs.
        let matched: HashSet<usize> = [0, 2].into_iter().collect();
        assert_eq!(
            highlight_runs("🚀 d", &matched),
            vec![
                ("🚀".to_string(), true),
                (" ".to_string(), false),
                ("d".to_string(), true),
            ]
        );
    }

    /// A mark that covers only PART of a grapheme cluster still takes the whole
    /// cluster: the run's end snaps UP past the emoji's skin-tone modifier, and
    /// (the same rule read the other way) its start snaps DOWN onto the base
    /// codepoint. Marking one extra codepoint is harmless; a span boundary inside
    /// a cluster is not — see [`match_runs`].
    #[test]
    fn match_runs_snaps_a_partial_mark_out_to_the_whole_cluster() {
        // "e👍🏽f": char 1 is the emoji base, char 2 its Fitzpatrick modifier.
        let text = "e\u{1F44D}\u{1F3FD}f";
        for marked in [1usize, 2] {
            let matched: HashSet<usize> = [marked].into_iter().collect();
            assert_eq!(
                match_runs(text, 0, &matched),
                vec![("e", false), ("\u{1F44D}\u{1F3FD}", true), ("f", false)],
                "a mark on char {marked} alone still takes the cluster whole"
            );
        }
    }

    /// Two clusters that each snap outward until they touch become ONE run, the
    /// same coalescing abutting marks have always had — never two adjacent spans
    /// carrying the same state.
    #[test]
    fn match_runs_merges_clusters_that_meet_after_snapping() {
        // "a👍🏽👍🏽b": chars 1,2 are the first cluster and 3,4 the second, so a mark
        // on 2 and 3 lands inside a different cluster at each end.
        let text = "a\u{1F44D}\u{1F3FD}\u{1F44D}\u{1F3FD}b";
        let matched: HashSet<usize> = [2, 3].into_iter().collect();
        assert_eq!(
            match_runs(text, 0, &matched),
            vec![
                ("a", false),
                ("\u{1F44D}\u{1F3FD}\u{1F44D}\u{1F3FD}", true),
                ("b", false),
            ],
            "the two snapped runs merge instead of emitting two abutting spans"
        );
    }

    /// `char_offset` is what lets a caller walk a multi-span line: the positions
    /// address the whole line's text, so each span resumes counting where the
    /// previous one stopped rather than restarting at 0.
    #[test]
    fn match_runs_counts_char_positions_from_the_offset() {
        let matched: HashSet<usize> = [6].into_iter().collect();
        assert_eq!(
            match_runs("ab", 5, &matched),
            vec![("a", false), ("b", true)],
            "with the span starting at char 5, position 6 is its SECOND char"
        );
    }

    #[test]
    fn highlight_label_spans_applies_highlight_only_to_matched_runs() {
        let matched: HashSet<usize> = [0].into_iter().collect();
        let base = Style::default();
        let hl = Style::default()
            .fg(Color::LightBlue)
            .add_modifier(Modifier::BOLD);
        let spans = highlight_label_spans("ab", &matched, base, hl);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content.as_ref(), "a");
        assert_eq!(
            spans[0].style, hl,
            "the matched char gets the highlight style"
        );
        assert_eq!(spans[1].content.as_ref(), "b");
        assert_eq!(
            spans[1].style, base,
            "an unmatched char keeps the base style"
        );
    }

    // --- preview match highlight (the styled sibling) ----------------------

    /// A preview line's plain text: the spans' contents, concatenated — the same
    /// string `app::line_text` hands the matcher, so a test's expectations about
    /// char positions are the ones the production path uses.
    fn spans_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// A styled two-span line: DIM `code ` then BOLD `word`, under a line-level
    /// style and alignment of its own — both non-default on purpose, so an
    /// assertion that they survived is capable of failing.
    fn styled_line() -> Line<'static> {
        Line::from(vec![
            Span::styled("code ", Style::default().add_modifier(Modifier::DIM)),
            Span::styled("word", Style::default().add_modifier(Modifier::BOLD)),
        ])
        .style(Style::default().fg(Color::Cyan))
        .alignment(Alignment::Center)
    }

    /// The mark COMPOSES onto each span's own style instead of replacing it: a
    /// matched run inside DIM code stays DIM and gains the modifier, and the
    /// unmatched remainder of that same span is untouched.
    #[test]
    fn preview_match_highlight_preserves_each_spans_own_style() {
        // "code word": chars 0..=3 are "code" (inside the DIM span), chars 5..=8
        // are "word" (inside the BOLD span).
        let matched: HashSet<usize> = [0, 1, 2, 3, 5, 6, 7, 8].into_iter().collect();
        let out = highlight_matched_spans(&styled_line(), &matched, PREVIEW_MATCH_MODIFIER);
        let styled: Vec<(String, Style)> = out
            .spans
            .iter()
            .map(|s| (s.content.to_string(), s.style))
            .collect();
        assert_eq!(
            styled,
            vec![
                (
                    "code".to_string(),
                    Style::default()
                        .add_modifier(Modifier::DIM)
                        .add_modifier(PREVIEW_MATCH_MODIFIER)
                ),
                (
                    " ".to_string(),
                    Style::default().add_modifier(Modifier::DIM)
                ),
                (
                    "word".to_string(),
                    Style::default()
                        .add_modifier(Modifier::BOLD)
                        .add_modifier(PREVIEW_MATCH_MODIFIER)
                ),
            ],
            "each run keeps the style of the span it came from, plus the mark"
        );
    }

    /// The width the overflow fixture below is measured at. [`overflowing_line`]
    /// is 40 columns wide, so it wraps to exactly TWO full rows here and a single
    /// invented column tips it to three — which is the only shape in which a
    /// width-changing split can be caught moving a row.
    const OVERFLOW_WRAP_WIDTH: u16 = 20;
    /// Rows [`overflowing_line`] occupies at [`OVERFLOW_WRAP_WIDTH`] when nothing
    /// has changed its width. Stated so the fixture's overflow is asserted, not
    /// assumed.
    const OVERFLOW_WRAP_ROWS: usize = 2;

    /// A styled line that OVERFLOWS [`OVERFLOW_WRAP_WIDTH`], with the emoji +
    /// skin-tone cluster parked right where the first row fills up.
    ///
    /// Both halves are load-bearing. It is 40 columns at 20, so row one ends
    /// exactly full and one extra column pushes the `ddd` word — and then the long
    /// `e` word behind it — onto rows of their own. And the mark's run boundary
    /// falls INSIDE the cluster, the split that silently invents those columns.
    fn overflowing_line() -> Line<'static> {
        Line::from(vec![
            Span::styled(
                "aaaa bbbb cccc ",
                Style::default().add_modifier(Modifier::DIM),
            ),
            Span::styled(
                "\u{1F44D}\u{1F3FD}ddd eeeeeeeeeeeeeeeeeee",
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ])
        .style(Style::default().fg(Color::Cyan))
        .alignment(Alignment::Center)
    }

    /// The pane's geometry rides on this: a line's DISPLAY WIDTH is what the wrapper
    /// breaks on, and the prefix map it produces is cached per (session, width) and
    /// then read by BOTH the windowed draw and the click hit-test
    /// (`App::preview_hit_context`) — so a re-styled line that changed either would
    /// silently move every link and every scroll bound.
    ///
    /// Measured against a fixture that WRAPS. A line that fits the width leaves
    /// both sides of the row assertion at 1 for any implementation at all, broken
    /// ones included — the assertion has to be able to see a row move.
    #[test]
    fn preview_match_highlight_changes_no_width_and_no_line_count() {
        let line = overflowing_line();
        // Char 15 is the emoji's base codepoint; char 16 is its skin-tone
        // modifier. Marking only the base puts the run boundary mid-cluster.
        let matched: HashSet<usize> = [15].into_iter().collect();
        let out = highlight_matched_spans(&line, &matched, PREVIEW_MATCH_MODIFIER);
        assert_eq!(spans_text(&out), spans_text(&line), "the text is identical");
        assert_eq!(out.width(), line.width(), "the display width is identical");
        assert_eq!(
            wrapped_text_rows(std::slice::from_ref(&line), OVERFLOW_WRAP_WIDTH),
            OVERFLOW_WRAP_ROWS,
            "the fixture must overflow the measuring width, or the next assertion \
             compares 1 to 1 and cannot fail"
        );
        assert_eq!(
            wrapped_text_rows(std::slice::from_ref(&out), OVERFLOW_WRAP_WIDTH),
            wrapped_text_rows(std::slice::from_ref(&line), OVERFLOW_WRAP_WIDTH),
            "the wrapped row count is identical"
        );
        assert_eq!(out.style, line.style, "the line's own style survives");
        assert_eq!(out.alignment, line.alignment, "the alignment survives");
    }

    /// A span boundary must never land inside a grapheme cluster.
    ///
    /// `Line::width` sums `unicode-width` PER SPAN and that width is a CONTEXTUAL
    /// fold, so cutting a cluster changes the measured width of text that did not
    /// change: +2 columns for an emoji severed from its skin-tone modifier, -1 for
    /// one severed from its VS16. The cached wrapped-row prefix map — which the
    /// windowed draw and the click hit-test BOTH read — is measured on the UNSPLIT
    /// lines, so a cut cluster desyncs it from the line actually painted, and it is
    /// the wrapper's own break points that move with it. The run therefore
    /// snaps OUT to the cluster's edges — marking one extra codepoint, never
    /// splitting one.
    #[test]
    fn preview_match_highlight_never_splits_a_grapheme_cluster() {
        // (the cluster, which of ITS chars the mark lands on)
        for (cluster, marked_char) in [
            // Thumbs-up + Fitzpatrick modifier: severing it costs +2 columns.
            ("\u{1F44D}\u{1F3FD}", 0),
            // Heart + VS16 (emoji presentation): severing it costs -1 column.
            ("\u{2764}\u{FE0F}", 0),
            // A flag — two regional indicators. The mark lands on the SECOND, so
            // the run's START has to snap DOWN, not just its end up.
            ("\u{1F1FA}\u{1F1F8}", 1),
        ] {
            let line = Line::from(Span::raw(format!("x{cluster}y")));
            let matched: HashSet<usize> = [1 + marked_char].into_iter().collect();
            let out = highlight_matched_spans(&line, &matched, PREVIEW_MATCH_MODIFIER);
            let runs: Vec<&str> = out.spans.iter().map(|s| s.content.as_ref()).collect();
            assert_eq!(
                runs,
                vec!["x", cluster, "y"],
                "{:?}: the cluster is marked whole",
                cluster.escape_unicode().to_string()
            );
            assert_eq!(
                out.width(),
                line.width(),
                "{:?}: the summed span width still describes the painted text",
                cluster.escape_unicode().to_string()
            );
        }
    }

    /// Multi-byte chars: positions are CHAR positions, so a 4-byte emoji is
    /// marked whole and a CJK span is never sliced mid-codepoint. Out-of-range
    /// positions (a match past this line's end) are ignored, never a panic.
    #[test]
    fn preview_match_highlight_is_char_safe_for_multibyte_lines() {
        let line = Line::from(vec![
            Span::raw("🚀 "),
            Span::styled("日本語", Style::default().add_modifier(Modifier::ITALIC)),
        ]);
        // char 0 = 🚀, char 3 = 本, plus two positions past the end.
        let matched: HashSet<usize> = [0, 3, 99, 400].into_iter().collect();
        let out = highlight_matched_spans(&line, &matched, PREVIEW_MATCH_MODIFIER);
        let runs: Vec<(String, bool)> = out
            .spans
            .iter()
            .map(|s| {
                (
                    s.content.to_string(),
                    s.style.add_modifier.contains(PREVIEW_MATCH_MODIFIER),
                )
            })
            .collect();
        assert_eq!(
            runs,
            vec![
                ("🚀".to_string(), true),
                (" ".to_string(), false),
                ("日".to_string(), false),
                ("本".to_string(), true),
                ("語".to_string(), false),
            ],
            "runs split on char boundaries, out-of-range positions ignored"
        );
        assert_eq!(spans_text(&out), spans_text(&line));
        assert_eq!(
            out.width(),
            line.width(),
            "CJK/emoji columns survive the split"
        );
    }

    /// An unmatched line is handed back structurally unchanged — same span count
    /// (an EMPTY span included), same styles — so the common case (most lines
    /// match nothing) adds nothing and no line is quietly restructured.
    #[test]
    fn preview_match_highlight_leaves_an_unmatched_line_alone() {
        let mut line = styled_line();
        line.spans
            .push(Span::styled("", Style::default().fg(Color::Red)));
        let out = highlight_matched_spans(&line, &HashSet::new(), PREVIEW_MATCH_MODIFIER);
        assert_eq!(out.spans.len(), line.spans.len(), "no span was split");
        for (got, want) in out.spans.iter().zip(line.spans.iter()) {
            assert_eq!(got.content, want.content);
            assert_eq!(got.style, want.style, "no mark on an unmatched line");
        }
    }

    // --- preview scrollbar geometry -----------------------------------------

    /// Path to a checked-in transcript fixture (shared with `store::preview`'s
    /// own tests), so this test exercises real markdown-rendered turns rather
    /// than a hand-rolled `Text`.
    fn fixture(folder: &str, file: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("store")
            .join(folder)
            .join(file)
    }

    /// A one-off `Session` over the checked-in `sess-normal-1` fixture, shared
    /// by the scrollbar-geometry tests below so each only states the geometry
    /// (viewport size, scroll position) it actually cares about.
    fn sample_session() -> Session {
        Session {
            file: fixture("-Users-me-project-alpha", "sess-normal-1.jsonl"),
            session_id: "sess-normal-1".to_string(),
            cwd: PathBuf::from("/Users/me/project-alpha"),
            git_branch: Some("main".to_string()),
            timestamp: None,
            repo: "project-alpha".to_string(),
            label: "sess-normal-1".to_string(),
            root_uuid: None,
            msg_count: 0,
            content_index: String::new(),
        }
    }

    // --- quick-reply compose zone: placement + split geometry + render -----

    /// The compose zone docks in the preview on a tall board and falls back to the
    /// full-width bottom bar on a short one; a non-composing board never bottom-bars.
    #[test]
    fn compose_docks_on_a_tall_board_and_bottom_bars_on_a_short_one() {
        // Not composing -> never a bottom bar, whatever the height.
        assert!(!compose_uses_bottom_bar(false, 8));
        assert!(!compose_uses_bottom_bar(false, 40));
        // preview_pane_inner_height(h) = h - BOARD_CHROME_ROWS - 2, so this height
        // is exactly the dock threshold.
        let just_docks = COMPOSE_MIN_DOCK_HEIGHT + BOARD_CHROME_ROWS + 2;
        assert!(
            !compose_uses_bottom_bar(true, just_docks),
            "just tall enough must dock, not bottom-bar"
        );
        assert!(
            !compose_uses_bottom_bar(true, just_docks + 6),
            "taller still docks"
        );
        // One row short of the threshold -> bottom bar; a tiny board too.
        assert!(
            compose_uses_bottom_bar(true, just_docks - 1),
            "one row short of docking must bottom-bar"
        );
        assert!(compose_uses_bottom_bar(true, 8), "a tiny board bottom-bars");
    }

    /// `preview_compose_split` carves the compose zone off the bottom of the
    /// transcript when docking, and is a no-op (empty compose, full transcript) when
    /// not — so a non-composing pane keeps its exact prior geometry.
    #[test]
    fn preview_compose_split_reserves_the_zone_only_when_docking() {
        let pane = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 20,
        };
        // Not docking (height 0): identical to the 2-way split, empty compose rect.
        let (b0, t0) = preview_split(pane, false);
        let (b1, t1, c1) = preview_compose_split(pane, false, 0);
        assert_eq!((b1, t1), (b0, t0), "no dock must not disturb the split");
        assert_eq!(c1.height, 0, "no compose rect when not docking");

        // Docking with a given height: the zone comes out of the transcript bottom,
        // the two do not overlap, and together they tile the original transcript.
        let zone = 5u16;
        let (_b, transcript, compose) = preview_compose_split(pane, false, zone);
        assert_eq!(compose.height, zone);
        assert_eq!(
            t0.height,
            transcript.height + compose.height,
            "the compose zone is taken FROM the transcript, not added beside it"
        );
        assert_eq!(
            compose.y,
            transcript.y + transcript.height,
            "compose sits directly below the (shrunk) transcript"
        );
        assert_eq!(
            transcript.y, t0.y,
            "the transcript still starts where it did"
        );
        assert_eq!(compose.width, transcript.width);
    }

    /// The compose box starts at one text row and GROWS with the draft — one row per
    /// logical line, and extra rows for a long soft-wrapped line — capped at
    /// [`COMPOSE_MAX_TEXT_ROWS`].
    #[test]
    fn compose_box_grows_from_one_line_up_to_the_cap() {
        use crate::tui::compose::ComposeState;

        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        app.compose = Some(ComposeState::new_reply("sess-normal-1".to_string(), None));

        // Empty draft -> one text row (zone height = 1 + 2 borders).
        assert_eq!(compose_text_rows(&app), COMPOSE_MIN_TEXT_ROWS);
        assert_eq!(compose_zone_height(&app), COMPOSE_MIN_TEXT_ROWS + 2);

        // Three logical lines -> three text rows.
        let ta = &mut app.compose.as_mut().unwrap().textarea;
        ta.insert_newline();
        ta.insert_newline();
        assert_eq!(compose_text_rows(&app), 3);

        // Well past the cap -> clamped to the max.
        for _ in 0..20 {
            app.compose.as_mut().unwrap().textarea.insert_newline();
        }
        assert_eq!(
            compose_text_rows(&app),
            COMPOSE_MAX_TEXT_ROWS,
            "grows only to the cap"
        );

        // A single long line soft-wraps to more than one row. The board is DRAWN
        // first because the editor measures itself at the width it was last drawn
        // at: without that frame its area is still zero wide, it wraps nothing, and
        // this would pass vacuously against the one logical line.
        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        app.show_preview = true;
        app.compose = Some(ComposeState::new_reply("s".to_string(), None));
        app.compose
            .as_mut()
            .unwrap()
            .textarea
            .insert_str("word ".repeat(40)); // ~200 columns on one logical line
        let _ = drawn_board(&mut app, DOCK_BOARD.0, DOCK_BOARD.1);
        assert!(
            compose_text_rows(&app) > 1,
            "a long line wraps to more than one row in the drawn box"
        );
    }

    /// A DOCKED compose zone is laid out at the preview pane's INNER width — the one
    /// rect `render_compose_zone` is handed — and never at the pane's outer width.
    ///
    /// This is the invariant the docked box lost: it sits inside the pane's border
    /// and then draws a border of its OWN, so anything that measured it against
    /// `area.width` believed it was two columns wider than it is. Pinning the split's
    /// compose rect against [`preview_inner`] keeps the two derivations from parting
    /// again — and the `- 2` below is asserted explicitly so a change that quietly
    /// dropped the pane's inset would fail here rather than only in a render.
    #[test]
    fn the_docked_compose_zone_is_laid_out_at_the_pane_inner_width() {
        let pane = Rect {
            x: 3,
            y: 1,
            width: 40,
            height: 20,
        };
        for has_banner in [false, true] {
            let (_, _, compose) = preview_compose_split(pane, has_banner, COMPOSE_MAX_ZONE_HEIGHT);
            assert_eq!(
                compose.width,
                preview_inner(pane).width,
                "the compose rect must be the pane's INNER width (banner: {has_banner})"
            );
            assert_eq!(
                compose.width,
                pane.width - 2,
                "which is the pane's own border inset, not its outer width"
            );
            assert_eq!(
                compose.x,
                preview_inner(pane).x,
                "and it must start inside the pane's left border"
            );
        }
    }

    /// A DOCKED compose-zone board: 60 columns with the splitter PINNED, so the
    /// preview pane is exactly 30 columns, and tall enough to clear the dock
    /// threshold. Both halves are fixed rather than defaulted because the tests below
    /// need to know where the editor actually wraps.
    const DOCK_BOARD: (u16, u16) = (60, 30);
    /// The splitter position [`DOCK_BOARD`] is drawn with, leaving a 30-column
    /// preview pane beside it.
    const DOCK_LIST_WIDTH: u16 = 30;
    /// The editor's REAL inner width on [`DOCK_BOARD`]: the 30-column preview pane,
    /// less the pane's own border (2), less the compose block's border (2). Spelled
    /// out because those four columns are exactly what the docked path used to lose
    /// track of — and PINNED to the drawn box by [`drawn_editor_width`] rather than
    /// trusted, so a layout change that moved the editor fails the cases below instead
    /// of leaving their premise reasoning about a width the editor no longer has.
    const DOCK_EDITOR_WIDTH: u16 = 26;

    /// A BOTTOM-BAR compose board: one row short of the dock threshold, so the zone
    /// claims a full-width bar between the body and the search line.
    const BAR_BOARD: (u16, u16) = (30, COMPOSE_MIN_DOCK_HEIGHT + BOARD_CHROME_ROWS + 1);
    /// The editor's inner width on [`BAR_BOARD`]: the whole 30-column board, less the
    /// bar's own border (2). The bar has no pane around it, which is why this path was
    /// always right about its width — and why it still needs its own test, since the
    /// WRAP MODEL was wrong on both paths. Pinned to the drawn box like its docked
    /// counterpart.
    const BAR_EDITOR_WIDTH: u16 = 28;

    /// Three words too long to share a row in either box above: word wrap puts each on
    /// its own row ([`WRAPPING_DRAFT_ROWS`]), while the character-packing
    /// `ceil(width / inner)` model the compose path used to apply reports one row
    /// FEWER. That gap is what makes the tests below able to tell the two apart.
    const WRAPPING_DRAFT: &str = "aaaaaaaaaaaaaa bbbbbbbbbbbbbb cccccccccccccc";
    /// Rows [`WRAPPING_DRAFT`] occupies once word-wrapped at either editor width.
    const WRAPPING_DRAFT_ROWS: usize = 3;

    /// What a reply box's title starts with ([`compose_title`]), used to FIND the box
    /// in a drawn board.
    const COMPOSE_TITLE_MARKER: &str = "reply to";
    /// The bottom-left corner a `Borders::ALL` block closes with. Used to find the
    /// compose box's last drawn row without trusting the height the code under test
    /// computed.
    const BOX_BOTTOM_LEFT: char = '└';
    /// The bottom-RIGHT corner of that same row. Paired with [`BOX_BOTTOM_LEFT`] it
    /// spans the box exactly as drawn, which is how [`drawn_editor_width`] recovers the
    /// width without trusting the geometry the code under test computed either.
    const BOX_BOTTOM_RIGHT: char = '┘';

    /// The compose box's TEXT rows exactly as DRAWN: everything between its titled top
    /// border and the border row that closes it.
    ///
    /// Both bounds are read off the BUFFER rather than from `compose_zone_height`, so
    /// a box that grew wrongly is measured by what reached the screen instead of by
    /// its own mistake.
    fn drawn_compose_text_rows(
        buffer: &ratatui::buffer::Buffer,
        width: u16,
        height: u16,
    ) -> Vec<String> {
        let rows: Vec<String> = (0..height)
            .map(|y| full_row_text(buffer, y, width))
            .collect();
        let top = rows
            .iter()
            .position(|row| row.contains(COMPOSE_TITLE_MARKER))
            .expect("the compose box must be drawn, titled");
        let bottom = rows
            .iter()
            .enumerate()
            .skip(top + 1)
            .find(|(_, row)| row.contains(BOX_BOTTOM_LEFT))
            .map(|(y, _)| y)
            .expect("the compose box must be closed by a bottom border");
        rows[top + 1..bottom].to_vec()
    }

    /// The width the compose EDITOR was really drawn at: the columns its closing
    /// border row spans, less that border's own two.
    ///
    /// Read off the BUFFER for the same reason the row count is — the drawn cells are
    /// the only place the box's real geometry exists — so the width constants above can
    /// be pinned to the layout instead of merely describing it.
    fn drawn_editor_width(buffer: &ratatui::buffer::Buffer, width: u16, height: u16) -> u16 {
        let rows: Vec<String> = (0..height)
            .map(|y| full_row_text(buffer, y, width))
            .collect();
        let top = rows
            .iter()
            .position(|row| row.contains(COMPOSE_TITLE_MARKER))
            .expect("the compose box must be drawn, titled");
        let closing: Vec<char> = rows
            .iter()
            .skip(top + 1)
            .find(|row| row.contains(BOX_BOTTOM_LEFT))
            .expect("the compose box must be closed by a bottom border")
            .chars()
            .collect();
        let left = closing
            .iter()
            .position(|c| *c == BOX_BOTTOM_LEFT)
            .expect("the closing row carries the box's bottom-left corner");
        let right = closing
            .iter()
            .position(|c| *c == BOX_BOTTOM_RIGHT)
            .expect("the closing row carries the box's bottom-right corner");
        u16::try_from(right.saturating_sub(left) + 1)
            .expect("a drawn box is never wider than a terminal")
            .saturating_sub(2) // the box's own left + right border
    }

    /// Assert the drawn compose box holds the WHOLE wrapping draft, first row first.
    ///
    /// The symptom being pinned is a scroll, not a crop: an under-grown box keeps the
    /// CARET visible, so the tail rows are all still there and only the head is gone.
    /// Checking `rows[0]` is therefore the assertion that fails, and the row count is
    /// what says why.
    fn assert_whole_draft_is_visible(
        buffer: &ratatui::buffer::Buffer,
        width: u16,
        height: u16,
        editor_width: u16,
    ) {
        // `editor_width` is what the ceil-model guard at the bottom reasons about, so
        // tie it to the box that was actually drawn FIRST: a layout change that moved
        // the editor then fails here, rather than leaving that guard — and with it the
        // whole case — checking a width nothing on screen has any more.
        assert_eq!(
            drawn_editor_width(buffer, width, height),
            editor_width,
            "the compose editor must really be drawn {editor_width} columns wide"
        );
        let rows = drawn_compose_text_rows(buffer, width, height);
        let words: Vec<&str> = WRAPPING_DRAFT.split(' ').collect();
        assert_eq!(
            rows.len(),
            WRAPPING_DRAFT_ROWS,
            "the box must grow to the draft's wrapped height; drawn rows: {rows:?}"
        );
        assert!(
            rows[0].contains(words[0]),
            "the draft's FIRST row must still be on screen, not scrolled away; \
             drawn rows: {rows:?}"
        );
        for word in &words {
            assert!(
                rows.iter().any(|row| row.contains(word)),
                "{word:?} must be visible somewhere in the box; drawn rows: {rows:?}"
            );
        }
        // The character-packing model, run over the same draft at the same editor
        // width: one row short. Without this the case could pass while both models
        // agreed, proving nothing about which one is in use.
        assert!(
            wrapped_line_height(WRAPPING_DRAFT.len(), editor_width) < WRAPPING_DRAFT_ROWS,
            "this draft must be one the ceil model gets WRONG at width {editor_width}"
        );
    }

    /// A soft-wrapping draft grows the DOCKED compose box instead of the editor
    /// scrolling the draft's first row out of view.
    ///
    /// The docked box is where both defects landed: it is measured for a zone that
    /// lives INSIDE the preview pane's border and draws a border of its own, and it
    /// was measured with the transcript's character-packing wrap model rather than the
    /// editor's word wrap. Either alone under-grew the box, and an under-grown box
    /// scrolls to keep the caret visible — so the row the user just started typing on
    /// disappears upward. Asserted on DRAWN CELLS, because that is the only place the
    /// symptom was ever visible.
    #[test]
    fn a_wrapping_draft_grows_the_docked_compose_box_rather_than_scrolling_its_first_row_away() {
        use crate::tui::compose::ComposeState;

        let (width, height) = DOCK_BOARD;
        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        app.show_preview = true;
        app.list_width = Some(DOCK_LIST_WIDTH);
        app.compose = Some(ComposeState::new_reply("sess-normal-1".to_string(), None));
        assert!(
            !compose_uses_bottom_bar(app.is_composing(), height),
            "this board must DOCK, or it tests the other path"
        );

        // Draw ONCE before typing: the editor measures itself at the width it was last
        // drawn at, and before its first frame that width is zero — so without this
        // frame the box would be sized from logical lines and the test would chase a
        // ghost.
        let _ = drawn_board(&mut app, width, height);
        app.compose
            .as_mut()
            .expect("compose is open")
            .textarea
            .insert_str(WRAPPING_DRAFT);
        let buffer = drawn_board(&mut app, width, height);

        assert_whole_draft_is_visible(&buffer, width, height, DOCK_EDITOR_WIDTH);
    }

    /// The same draft in the FULL-WIDTH BOTTOM BAR: it too grows to the wrapped
    /// height rather than scrolling its first row away.
    ///
    /// This path always knew its own width, so it isolates the WRAP MODEL half of the
    /// bug — and covering both placements is what keeps them from drifting apart
    /// again, which is how the docked one broke alone.
    #[test]
    fn a_wrapping_draft_grows_the_bottom_bar_compose_box_rather_than_scrolling_its_first_row_away()
    {
        use crate::tui::compose::ComposeState;

        let (width, height) = BAR_BOARD;
        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        app.show_preview = true;
        app.compose = Some(ComposeState::new_reply("sess-normal-1".to_string(), None));
        assert!(
            compose_uses_bottom_bar(app.is_composing(), height),
            "this board must BOTTOM-BAR, or it tests the other path"
        );

        let _ = drawn_board(&mut app, width, height);
        app.compose
            .as_mut()
            .expect("compose is open")
            .textarea
            .insert_str(WRAPPING_DRAFT);
        let buffer = drawn_board(&mut app, width, height);

        assert_whole_draft_is_visible(&buffer, width, height, BAR_EDITOR_WIDTH);
    }

    /// Opening compose draws a bordered "reply to <label>" box — docked in the
    /// preview on a tall board, and as a full-width bottom bar on a short one — and
    /// never panics through a real backend.
    #[test]
    fn composing_renders_the_reply_box_docked_and_as_a_bottom_bar() {
        use crate::tui::compose::ComposeState;

        let open = |app: &mut App| {
            app.show_preview = true;
            app.compose = Some(ComposeState::new_reply("sess-normal-1".to_string(), None));
        };
        let full = |buffer: &ratatui::buffer::Buffer, w: u16, h: u16| -> String {
            (0..h)
                .map(|y| full_row_text(buffer, y, w))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let width = 80u16;

        // Tall board: the reply box docks inside the preview pane.
        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        open(&mut app);
        let tall = 30u16;
        assert!(
            !compose_uses_bottom_bar(app.is_composing(), tall),
            "this board must dock"
        );
        let buffer = drawn_board(&mut app, width, tall);
        assert!(
            full(&buffer, width, tall).contains("reply to"),
            "the docked reply box must be titled"
        );

        // Short board: the reply box falls back to a full-width bottom bar.
        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        open(&mut app);
        let short = COMPOSE_MIN_DOCK_HEIGHT + BOARD_CHROME_ROWS + 1; // one short of docking
        assert!(
            compose_uses_bottom_bar(app.is_composing(), short),
            "this board must bottom-bar"
        );
        let buffer = drawn_board(&mut app, width, short);
        assert!(
            full(&buffer, width, short).contains("reply to"),
            "the bottom-bar reply box must be titled"
        );
    }

    /// A draft SUPPRESSES the selected session's pinned status banner.
    ///
    /// Two reasons, and the second is a correctness one: the banner describes a
    /// session the pane is no longer showing, and `preview_split` keys the
    /// transcript rect off `preview_banner(..).is_some()` — the same fn
    /// `update`'s click hit-test asks — so a banner drawn above the card would
    /// leave render and hit-test disagreeing by a row.
    #[test]
    fn a_draft_suppresses_the_selected_sessions_banner() {
        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        let mut reported = HashMap::new();
        reported.insert(
            "sess-normal-1".to_string(),
            ReportedAgent {
                kind: "background".to_string(),
                id: Some("job-1".to_string()),
                state: Some("running".to_string()),
                status: None,
                name: None,
            },
        );
        app.set_reported_agents(reported);
        assert!(
            preview_banner(&app).is_some(),
            "a reported session banners while browsing, or this proves nothing"
        );

        crate::tui::compose::open_background(&mut app, Some("planner".to_string()));
        assert!(
            preview_banner(&app).is_none(),
            "a draft owns the pane, so no banner row may be reserved"
        );
    }

    /// The draft card carries THREE facts and nothing else — what is starting,
    /// where it will run, and the keys that act on it.
    ///
    /// The line COUNT is asserted on purpose: the card stands for a session that
    /// does not exist yet, so its emptiness is the feature. Anything that later
    /// tries to fill it with invented content fails here rather than shipping a
    /// pane that looks like a conversation.
    #[test]
    fn the_draft_card_names_the_agent_and_the_dir_and_stays_empty() {
        let dir = PathBuf::from("/tmp/launch");
        let flat = |lines: &[Line<'static>]| -> Vec<String> {
            lines.iter().map(|l| l.to_string()).collect()
        };

        let card = draft_card(
            &NewSessionDraft {
                agent: Some("planner".to_string()),
                launch_id: None,
            },
            &dir,
            0,
        );
        let rows = flat(&card);
        assert_eq!(
            rows.len(),
            4,
            "the card is a placeholder, not a page: {rows:?}"
        );
        assert_eq!(
            rows[0],
            format!("new session{DRAFT_CARD_SEPARATOR}@planner")
        );
        assert_eq!(rows[1], "/tmp/launch", "the card states where it will run");
        assert_eq!(
            rows[2], "",
            "one blank row separates the facts from the keys"
        );
        assert_eq!(
            rows[3], BG_DRAFT_HINT,
            "the hint is shared with the help line"
        );

        // The picker's default row is NAMED, never a bare `@`; a blank agent name
        // degrades to the same wording rather than rendering an empty handle.
        for agent in [None, Some(""), Some("   ")] {
            let rows = flat(&draft_card(
                &NewSessionDraft {
                    agent: agent.map(str::to_owned),
                    launch_id: None,
                },
                &dir,
                0,
            ));
            assert_eq!(
                rows[0],
                format!("new session{DRAFT_CARD_SEPARATOR}{BG_DRAFT_DEFAULT_AGENT}"),
                "a nameless pick must read as the default row: {agent:?}"
            );
        }

        // Once dispatched, the keys no longer apply, so the hint gives way to the
        // in-flight line — animated off the board's OWN tick, not a second cadence.
        let launching = |tick: u64| {
            flat(&draft_card(
                &NewSessionDraft {
                    agent: Some("planner".to_string()),
                    // Any stamped id means "in flight"; the card renders the same
                    // line whichever dispatch it names.
                    launch_id: Some(1),
                },
                &dir,
                tick,
            ))
        };
        let at_zero = launching(0);
        assert_eq!(at_zero.len(), 4, "the in-flight card grows no rows");
        assert!(
            at_zero[3].contains(DRAFT_CARD_LAUNCHING),
            "a dispatched card reports the launch: {at_zero:?}"
        );
        assert!(
            !at_zero[3].contains("Esc cancel"),
            "the key hints must not survive a dispatch: {at_zero:?}"
        );
        assert_ne!(
            at_zero[3],
            launching(1)[3],
            "the in-flight line must animate off App::tick"
        );
    }

    /// The BACKGROUND draft REPLACES the previewed transcript with a placeholder
    /// card — docked compose on a tall board, a full-width bottom bar on a short
    /// one, the card in the pane either way.
    ///
    /// The load-bearing assertion is the NEGATIVE one: the selected session's
    /// transcript must NOT be behind the draft. A compose box docked over an
    /// unrelated conversation reads as a reply to that conversation, which is
    /// exactly the bug this card exists to fix — and it is the DEFAULT `Ctrl-N`
    /// path, so it is the first thing a user sees.
    #[test]
    fn the_background_draft_pane_renders_a_placeholder_card_not_a_transcript() {
        let open = |app: &mut App, agent: Option<&str>| {
            // Through the REAL open path, not a hand-built state, so the test
            // exercises whatever that path installs.
            crate::tui::compose::open_background(app, agent.map(str::to_owned));
        };
        let full = |buffer: &ratatui::buffer::Buffer, w: u16, h: u16| -> String {
            (0..h)
                .map(|y| full_row_text(buffer, y, w))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let width = 80u16;

        // Control: with NO draft open, the selected session's transcript IS drawn —
        // so the negative assertion below can actually fail.
        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        let tall = 30u16;
        let browsing = full(&drawn_board(&mut app, width, tall), width, tall);
        assert!(
            browsing.contains("webhook"),
            "the fixture's transcript must be visible while browsing, or the \
             negative assertion below proves nothing:\n{browsing}"
        );

        // Tall board: the draft card fills the pane and compose docks beneath it.
        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        open(&mut app, Some("planner"));
        assert!(
            !compose_uses_bottom_bar(app.is_composing(), tall),
            "this board must dock"
        );
        let drawn = full(&drawn_board(&mut app, width, tall), width, tall);
        assert!(
            !drawn.contains("webhook"),
            "the draft must not dock over the selected session's transcript:\n{drawn}"
        );
        assert!(
            drawn.contains("new session"),
            "the pane must show a new-session placeholder card:\n{drawn}"
        );
        assert!(
            drawn.contains("@planner"),
            "the card must name the picked agent:\n{drawn}"
        );
        assert!(
            drawn.contains("/tmp/launch"),
            "the card must show the launch directory:\n{drawn}"
        );
        assert!(
            drawn.contains("background agent: planner"),
            "the docked compose box must still be titled for the picked agent:\n{drawn}"
        );
        assert!(
            drawn.contains("Ctrl-O run interactively"),
            "the draft's key hints must offer the interactive escape hatch:\n{drawn}"
        );

        // Short board: compose falls back to a full-width bottom bar, the card still
        // owns the pane, and the default (no agent) row is named rather than blank.
        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        open(&mut app, None);
        let short = COMPOSE_MIN_DOCK_HEIGHT + BOARD_CHROME_ROWS + 1; // one short of docking
        assert!(
            compose_uses_bottom_bar(app.is_composing(), short),
            "this board must bottom-bar"
        );
        let drawn = full(&drawn_board(&mut app, width, short), width, short);
        assert!(
            !drawn.contains("webhook"),
            "the bottom-bar fallback must not leave the transcript in the pane:\n{drawn}"
        );
        assert!(
            // Matched against the CARD's own row, not a bare `BG_DRAFT_DEFAULT_AGENT`:
            // the compose box's title carries that phrase too, so the loose form
            // passes even with the card's label blanked out.
            drawn.contains(&format!(
                "{DRAFT_CARD_HEADLINE}{DRAFT_CARD_SEPARATOR}{BG_DRAFT_DEFAULT_AGENT}"
            )),
            "the card must name the default row rather than leave it blank:\n{drawn}"
        );
        assert!(
            drawn.contains(&format!("background agent: {BG_DRAFT_DEFAULT_AGENT}")),
            "the bottom-bar compose box must name the default row:\n{drawn}"
        );
    }

    /// Opening — and cancelling — a draft hands the transcript back at the scroll
    /// position it had.
    ///
    /// The card is four lines, so it never overflows: every offset clamps to 0.
    /// `render_preview` writes its resolved offset back to `App::preview_scroll`,
    /// which is right for a transcript and wrong for a card — the card is not what
    /// that offset describes. Persisting it rewinds the session BEHIND the draft to
    /// the top, so `Esc` hands back a pane scrolled somewhere the user never put it,
    /// and the position they were reading is gone. Asserted as the drawn pane rather
    /// than as the field alone: what the user loses is the view, not the number.
    #[test]
    fn a_cancelled_draft_hands_the_transcript_back_at_the_scroll_it_had() {
        let width = 40u16;
        let height = 12u16;
        let draw = |app: &mut App| -> String {
            let mut terminal = Terminal::new(TestBackend::new(width, height))
                .expect("build an in-memory test terminal");
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    render_preview(frame, app, area);
                })
                .expect("render_preview must not panic");
            let buffer = terminal.backend().buffer().clone();
            (0..height)
                .map(|y| full_row_text(&buffer, y, width))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        // A genuine INTERIOR scroll — the user read back up the transcript and
        // stopped there — so only preserving it can reproduce this pane.
        app.preview_follow_bottom = false;
        app.preview_scroll = 3;
        let browsing = draw(&mut app);
        assert_eq!(
            app.preview_scroll, 3,
            "the fixture must overflow this pane far enough for 3 to be a real \
             offset, or every assertion below passes vacuously"
        );

        crate::tui::compose::open_background(&mut app, Some("planner".to_string()));
        let carded = draw(&mut app);
        assert!(
            carded.contains(DRAFT_CARD_HEADLINE),
            "the card must own the pane, or the card path was never taken:\n{carded}"
        );
        assert_eq!(
            app.preview_scroll, 3,
            "the card's own geometry must not overwrite the transcript's scroll"
        );

        app.close_compose();
        assert_eq!(
            draw(&mut app),
            browsing,
            "cancelling a draft must hand the pane back exactly as it was"
        );
    }

    /// The compose title and key hints branch on the TARGET, and the background
    /// draft's `Ctrl-O` hint must stay honest: the prompt auto-submits as the first
    /// turn (no pre-fill exists), so the wording may not promise a review or an edit.
    #[test]
    fn compose_wording_branches_by_target_and_never_promises_a_review() {
        use crate::tui::compose::ComposeState;

        let app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );

        // Reply: named for the target session; stop-then-reply says so.
        let plain = ComposeState::new_reply("sess-normal-1".to_string(), None);
        assert!(compose_title(&app, &plain).contains("reply to"));
        let held = ComposeState::new_reply("sess-normal-1".to_string(), Some("job".to_string()));
        assert!(compose_title(&app, &held).contains("stop & reply to"));

        // Background: named for the agent, and a blank name falls back to the
        // default label rather than rendering an empty title.
        assert_eq!(
            compose_title(&app, &ComposeState::new_background(Some("planner".into()))),
            " new background agent: planner "
        );
        for blank in [None, Some(String::new()), Some("   ".to_string())] {
            assert_eq!(
                compose_title(&app, &ComposeState::new_background(blank.clone())),
                format!(" new background agent: {BG_DRAFT_DEFAULT_AGENT} "),
                "a blank agent name must not render an empty title: {blank:?}"
            );
        }

        // The reply hint offers NO interactive escape hatch, and says what a pasted
        // newline does (it used to submit the draft's first line).
        let reply_hint = compose_hint(&plain.target);
        assert_eq!(
            reply_hint,
            "Enter send · Ctrl-J newline (or Alt+Enter) · paste keeps newlines · Esc cancel",
        );
        assert!(
            !reply_hint.contains("Ctrl-O"),
            "the reply hints must not grow a key the reply target ignores: {reply_hint}"
        );

        // The background hint names both verbs, honestly.
        let bg_hint = compose_hint(&ComposeState::new_background(None).target);
        assert!(bg_hint.contains("Enter start in background"), "{bg_hint}");
        assert!(bg_hint.contains("Ctrl-O run interactively"), "{bg_hint}");
        for dishonest in ["review", "edit", "before sending", "prefill", "pre-fill"] {
            assert!(
                !bg_hint.to_lowercase().contains(dishonest),
                "the prompt AUTO-SUBMITS, so the hint must not imply {dishonest:?}: {bg_hint}"
            );
        }
    }

    /// While a send is in flight for the selected session the pinned banner is
    /// SUPPRESSED (so it cannot desync the hit-test) and the send renders INLINE at
    /// the transcript tail: the echoed message under a `▶ you` turn plus a single
    /// `● claude` **cooking…** placeholder. The placeholder no longer depends on the
    /// agents poll; it reads `cooking…` before and after claude reports working.
    /// The `▶ you` echo drops the instant the real turn lands on disk; when the send
    /// finishes the banner yields back to the agent status.
    #[test]
    fn an_in_flight_send_renders_inline_and_suppresses_the_banner() {
        use super::super::app::Sending;

        let flatten_lines = |lines: &[Line<'static>]| -> String {
            lines
                .iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        app.selected = Some("sess-normal-1".to_string());

        // Nothing in flight and no reported agent -> no banner, no inline tail.
        assert!(preview_banner(&app).is_none());
        assert!(sending_tail(&app, 80).is_none());

        // In flight, nothing on disk yet (msg_count still the baseline) -> the
        // pinned banner is suppressed and the tail echoes the message + "cooking…".
        app.sending = Some(Sending {
            session_id: "sess-normal-1".to_string(),
            message: "please summarize this".to_string(),
            baseline_msg_count: 0,
        });
        assert!(
            preview_banner(&app).is_none(),
            "an in-flight send suppresses the pinned banner"
        );
        let tail = flatten_lines(&sending_tail(&app, 80).expect("an in-flight send has a tail"));
        assert!(
            tail.contains("\u{25b6} you") && tail.contains("please summarize this"),
            "the sent message is echoed under a `you` turn: {tail:?}"
        );
        assert!(
            tail.contains("\u{25cf} claude") && tail.contains("cooking"),
            "a pending claude turn reads 'cooking': {tail:?}"
        );
        assert!(!tail.contains("sending"));

        // Once claude reports it working the placeholder STILL reads "cooking…" —
        // the label is poll-independent.
        let mut reported = HashMap::new();
        reported.insert(
            "sess-normal-1".to_string(),
            ReportedAgent {
                kind: "background".to_string(),
                id: None,
                state: Some("working".to_string()),
                status: None,
                name: None,
            },
        );
        app.set_reported_agents(reported);
        let tail = flatten_lines(&sending_tail(&app, 80).expect("still in flight"));
        assert!(
            tail.contains("cooking") && !tail.contains("sending"),
            "reported working must still read 'cooking', never 'sending': {tail:?}"
        );

        // The real user turn lands on disk (turn count grows past the baseline) ->
        // the echo steps aside, leaving only the pending claude placeholder so the
        // real turn (rendered by the reload) is not doubled.
        app.sessions[0].msg_count = 1;
        let tail = flatten_lines(&sending_tail(&app, 80).expect("still in flight"));
        assert!(
            !tail.contains("please summarize this") && !tail.contains("\u{25b6} you"),
            "the echo yields to the real turn once it lands: {tail:?}"
        );
        assert!(
            tail.contains("\u{25cf} claude") && tail.contains("cooking"),
            "the pending claude placeholder stays until the send finishes: {tail:?}"
        );

        // Send done -> no inline tail; the banner yields back to the agent status.
        app.sending = None;
        assert!(sending_tail(&app, 80).is_none());
        let banner = preview_banner(&app).expect("the reported agent still has a banner");
        let banner_text = banner
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(
            !banner_text.contains("sending") && !banner_text.contains("cooking"),
            "no longer in flight: {banner_text:?}"
        );
    }

    /// A dispatched quick reply is reported EXACTLY ONCE: on the preview pane's
    /// `sending_tail`, not duplicated on the help line. The help line keeps its
    /// ordinary keymap cheat sheet while the reply is in flight.
    #[test]
    fn a_dispatched_reply_is_reported_on_the_preview_not_the_help_line() {
        use super::super::app::Sending;

        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        app.selected = Some("sess-normal-1".to_string());
        app.sending = Some(Sending {
            session_id: "sess-normal-1".to_string(),
            message: "please summarize this".to_string(),
            baseline_msg_count: 0,
        });

        let width = 80u16;
        let height = 20u16;
        let buffer = drawn_board(&mut app, width, height);
        let help_row = (0..width)
            .map(|x| {
                buffer
                    .cell((x, height - 1))
                    .map(|c| c.symbol())
                    .unwrap_or(" ")
            })
            .collect::<String>();
        assert!(
            !help_row.contains("cooking"),
            "the help line must not show the in-flight reply: {help_row:?}"
        );

        let board_text = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            board_text.contains("please summarize this"),
            "the echoed message must appear in the preview pane: {board_text}"
        );
        assert!(
            board_text.contains("cooking"),
            "the in-flight placeholder must appear in the preview pane: {board_text}"
        );
    }

    /// Task 3.11 (view side): an empty-buffer `Enter` in compose sets a transient
    /// nudge that wins the help line over the compose hint; the next compose
    /// keystroke clears it and the help line shows the hint again — specifically
    /// the reply's "Enter send" wording.
    #[test]
    fn empty_enter_nudge_yields_back_the_compose_hint_on_the_help_line() {
        use crate::tui::compose::COMPOSE_EMPTY_HINT;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (width, height) = DOCK_BOARD;
        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        crate::tui::compose::open(&mut app, "sess-normal-1".to_string(), None);
        assert!(
            !compose_uses_bottom_bar(app.is_composing(), height),
            "this board must dock so the help line is the ordinary one"
        );

        let help_row = |buffer: &ratatui::buffer::Buffer| -> String {
            full_row_text(buffer, height - 1, width)
        };

        // Empty buffer: Enter sets the transient nudge and the help line shows it.
        let _ = crate::tui::compose::handle_compose_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert_eq!(app.status.as_deref(), Some(COMPOSE_EMPTY_HINT));
        let buffer = drawn_board(&mut app, width, height);
        let nudge_row = help_row(&buffer);
        assert!(
            nudge_row.contains(COMPOSE_EMPTY_HINT),
            "the help line must show the empty-buffer nudge: {nudge_row:?}"
        );
        assert!(
            !nudge_row.contains("Enter send"),
            "the nudge must hide the compose hint: {nudge_row:?}"
        );

        // The next keystroke clears the status; the help line returns to the compose hint.
        let _ = crate::tui::compose::handle_compose_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        );
        assert!(
            app.status.is_none(),
            "the next keystroke must clear the nudge"
        );
        let buffer = drawn_board(&mut app, width, height);
        let hint_row = help_row(&buffer);
        assert!(
            hint_row.contains("Enter send"),
            "the help line must show the compose hint again: {hint_row:?}"
        );
        assert!(
            !hint_row.contains(COMPOSE_EMPTY_HINT),
            "the expired nudge must not linger: {hint_row:?}"
        );
    }

    #[test]
    fn render_preview_pins_scrollbar_thumb_to_track_bottom_when_scrolled_to_end() {
        // A freshly selected session starts `preview_follow_bottom = true`
        // (`App::set_selected`), so rendering it immediately below pins the
        // offset to the last page without any extra scroll keypresses.
        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        assert!(
            app.preview_follow_bottom,
            "a freshly selected session must start pinned to the newest turn"
        );

        // Narrow enough that the fixture's several transcript turns overflow
        // the viewport (so the scrollbar renders), tall enough to hold a
        // couple of track rows above the bottom arrow.
        let width = 80u16;
        let height = 8u16;
        let mut terminal = Terminal::new(TestBackend::new(width, height))
            .expect("build an in-memory test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_preview(frame, &mut app, area);
            })
            .expect("render_preview must not panic on a small viewport");

        // Recompute the same wrapped-height math `render_preview` uses, from
        // the (now cached) preview text, to confirm this viewport genuinely
        // overflows and to know the exact bottom-pinned offset independent of
        // this fixture's specific turn count.
        let inner_height = height - 2;
        let content_h = content_height(&mut app, width);
        assert!(
            content_h > usize::from(inner_height),
            "fixture must overflow the viewport for the scrollbar to render \
             (content_h={content_h}, inner_height={inner_height})"
        );
        let max_offset = (content_h - usize::from(inner_height)) as u32;
        assert_eq!(
            app.preview_scroll, max_offset,
            "follow-bottom must pin the offset to the last page"
        );

        // The thumb's bottom-most cell sits one row above the down arrow
        // (`↓`), which itself sits one row above the block's bottom border:
        // height-1 (border) - 1 (down arrow) - 1 (last track row).
        let begin_row = 1u16;
        let end_row = height - 2;
        let last_track_row = height - 3;
        let thumb_col = width - 1;
        let buffer = terminal.backend().buffer();
        let cell = buffer
            .cell((thumb_col, last_track_row))
            .expect("scrollbar column must be within the rendered buffer");
        assert_eq!(
            cell.symbol(),
            "█",
            "scrolled to the bottom, the thumb must reach the very last track \
             cell instead of stopping short of it"
        );

        // Scrolled all the way down: the end (down) arrow shows and the begin
        // (up) arrow is hidden, since the top of the transcript is not visible.
        let begin_cell = buffer
            .cell((thumb_col, begin_row))
            .expect("scrollbar column must be within the rendered buffer");
        assert_eq!(
            begin_cell.symbol(),
            " ",
            "scrolled to the bottom (not the top), the begin (up) arrow must be hidden"
        );
        let end_cell = buffer
            .cell((thumb_col, end_row))
            .expect("scrollbar column must be within the rendered buffer");
        assert_eq!(
            end_cell.symbol(),
            "↓",
            "scrolled to the bottom, the end (down) arrow must show"
        );
    }

    #[test]
    fn render_preview_shows_begin_arrow_and_hides_end_arrow_at_offset_zero() {
        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        // `App::preview_top` is the normal Home-key path back to the start; it
        // drops follow-bottom (unlike a freshly selected session, where offset
        // 0 and "not following" happen to coincide only by construction), so
        // this exercises the genuine top-of-track case.
        app.preview_top();

        let width = 80u16;
        let height = 8u16;
        let mut terminal = Terminal::new(TestBackend::new(width, height))
            .expect("build an in-memory test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_preview(frame, &mut app, area);
            })
            .expect("render_preview must not panic on a small viewport");
        assert_eq!(
            app.preview_scroll, 0,
            "Home must resolve to the very first offset"
        );

        let thumb_col = width - 1;
        let begin_row = 1u16;
        let end_row = height - 2;
        let buffer = terminal.backend().buffer();
        let begin_cell = buffer
            .cell((thumb_col, begin_row))
            .expect("scrollbar column must be within the rendered buffer");
        assert_eq!(
            begin_cell.symbol(),
            "↑",
            "scrolled to the top, the begin (up) arrow must show"
        );
        let end_cell = buffer
            .cell((thumb_col, end_row))
            .expect("scrollbar column must be within the rendered buffer");
        assert_eq!(
            end_cell.symbol(),
            " ",
            "scrolled to the top (not the bottom), the end (down) arrow must be hidden"
        );
    }

    #[test]
    fn render_preview_hides_both_arrows_and_detaches_thumb_on_a_partial_scroll() {
        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        // A genuine, tiny partial scroll: dropped follow-bottom, offset just
        // barely above zero.
        app.preview_follow_bottom = false;
        app.preview_scroll = 5;

        // An extremely narrow pane (inner_width 1) inflates the fixture's
        // handful of short lines into hundreds of wrapped rows against a
        // 6-row track (inner_height 8, minus the 2 reserved arrow rows) — the
        // huge content_h/track_length ratio that exposed the old rounding bug
        // (a tiny real scroll rounding straight back onto an edge track row).
        let width = 3u16;
        let height = 10u16;
        let mut terminal = Terminal::new(TestBackend::new(width, height))
            .expect("build an in-memory test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_preview(frame, &mut app, area);
            })
            .expect("render_preview must not panic on a narrow, tall viewport");

        let inner_height = height - 2;
        let content_h = content_height(&mut app, width);
        let max_offset = (content_h - usize::from(inner_height)) as u32;
        assert!(
            app.preview_scroll > 0 && app.preview_scroll < max_offset,
            "the requested offset (5) must remain a genuine INTERIOR scroll for \
             this geometry (offset={}, max_offset={max_offset})",
            app.preview_scroll
        );

        let thumb_col = width - 1;
        let begin_row = 1u16;
        let end_row = height - 2;
        let first_track_row = 2u16;
        let last_track_row = height - 3;
        let buffer = terminal.backend().buffer();

        for (row, label) in [(begin_row, "begin"), (end_row, "end")] {
            let cell = buffer
                .cell((thumb_col, row))
                .expect("scrollbar column must be within the rendered buffer");
            assert_eq!(
                cell.symbol(),
                " ",
                "a genuine partial scroll must hide the {label} arrow"
            );
        }
        for (row, label) in [(first_track_row, "first"), (last_track_row, "last")] {
            let cell = buffer
                .cell((thumb_col, row))
                .expect("scrollbar column must be within the rendered buffer");
            assert_ne!(
                cell.symbol(),
                "█",
                "a genuine partial scroll must detach the thumb from the {label} track row"
            );
        }
    }

    // --- transcript content height (what the pane can reach) ----------------

    /// A preview pane narrow enough that the `sample_session` fixture's turns WORD-
    /// WRAP, and short enough that the wrapped result overflows it several times
    /// over. 12 columns leaves an inner width of 10, where the fixture's prose breaks
    /// at word boundaries the character-packing model never charged for — which is
    /// the whole point: at a comfortable width the two models agree and nothing here
    /// could fail.
    const WRAPPING_PANE: (u16, u16) = (12, 10);

    /// The last thing said in the `sample_session` fixture, and the last thing the
    /// pane must be able to show. Its final wrapped row is one word, so the assertion
    /// below reads a single drawn row rather than reconstructing the wrap.
    const FIXTURE_LAST_WORD: &str = "logging.";

    /// The rows the character-packing model would have claimed for `app`'s transcript
    /// at the pane's inner width — the count `content_h` used to be derived from.
    ///
    /// Kept in the tests alone: production has one model now, and this exists so a
    /// case can PROVE it is one the two disagree about instead of asserting into a
    /// coincidence.
    fn packed_content_height(app: &mut App, width: u16) -> usize {
        let inner_width = width - 2;
        app.preview_text(inner_width)
            .lines
            .iter()
            .map(|l| wrapped_line_height(l.width(), inner_width))
            .sum()
    }

    /// Following the bottom of a WORD-WRAPPED transcript really reaches its last
    /// line.
    ///
    /// The symptom the whole change exists for: `max_offset` is
    /// `content_h - inner_height`, so a content height that under-counts the wrap
    /// leaves the tail of the transcript unreachable — the pane bottom-anchors, and
    /// still stops short of the newest turn, with no key that can get there. Asserted
    /// on the DRAWN bottom row, since "the offset is bigger now" is a proxy and this
    /// is the thing the user was missing.
    #[test]
    fn following_the_bottom_reaches_the_last_line_of_a_word_wrapped_transcript() {
        let (width, height) = WRAPPING_PANE;
        let mut app = banner_app(None);
        assert!(
            app.preview_follow_bottom,
            "the pane must be bottom-anchored, or this tests nothing a user sees"
        );

        let packed = packed_content_height(&mut app, width);
        let wrapped = content_height(&mut app, width);
        assert!(
            packed < wrapped,
            "this fixture/width must be one the two models DISAGREE about, or the \
             old code passes too (packed={packed}, wrapped={wrapped})"
        );

        let rows = inner_rows(&mut app, width, height);
        assert_eq!(
            rows.last().map(String::as_str),
            Some(FIXTURE_LAST_WORD),
            "the newest turn's last row must be the pane's bottom row; drawn rows: {rows:?}"
        );
    }

    /// The optimistic turns of an in-flight quick reply count toward the height the
    /// pane scrolls against.
    ///
    /// They are appended to the transcript AFTER the cache was filled, so a height
    /// read from the cache alone is short by exactly the tail — and the message the
    /// user just sent, plus the live "cooking…" placeholder, sits below the bottom
    /// of the pane while it is the one thing they are watching for.
    #[test]
    fn an_in_flight_reply_tail_counts_toward_the_scrolled_height() {
        use super::super::app::Sending;

        let (width, height) = WRAPPING_PANE;
        let inner_width = width - 2;
        let mut app = banner_app(None);
        app.selected = Some("sess-normal-1".to_string());
        let transcript_only = content_height(&mut app, width);

        app.sending = Some(Sending {
            session_id: "sess-normal-1".to_string(),
            message: "ping".to_string(),
            baseline_msg_count: 0,
        });
        let tail = sending_tail(&app, inner_width).expect("a send is in flight");
        let tail_rows = wrapped_text_rows(&tail, inner_width);
        assert!(tail_rows > 0, "the tail must have rows to be missed");

        let rows = inner_rows(&mut app, width, height);
        assert_eq!(
            app.preview_scroll as usize,
            transcript_only + tail_rows - usize::from(height - 2),
            "the resolved offset must be measured over the transcript AND the tail"
        );
        assert!(
            rows.last().is_some_and(|row| row.contains("cooking")),
            "the live 'cooking…' placeholder must be the pane's bottom row; drawn rows: {rows:?}"
        );
    }

    /// A pane wide enough for the draft card to FIT (so any scroll of it is a bug)
    /// while the fixture's transcript still overflows it and bottom-anchors well past
    /// zero — the two conditions the card trap needs, both re-asserted below rather
    /// than trusted here.
    const CARD_PANE: (u16, u16) = (40, 8);

    /// A new-session draft CARD is measured as itself, never as the transcript it
    /// replaced.
    ///
    /// The card is a handful of lines and the transcript behind it is not, so
    /// borrowing that height leaves a `max_offset` the card cannot fill: the offset
    /// the user's reading position left behind survives the clamp and pushes the card
    /// off the top of the pane, so `Ctrl-N` opens onto a blank box.
    #[test]
    fn a_draft_card_is_measured_as_itself_not_as_the_transcript_it_replaced() {
        let (width, height) = CARD_PANE;
        let inner_height = usize::from(height - 2);
        let mut app = banner_app(None);
        // Read to the bottom of a transcript that overflows this pane: the offset
        // left behind is what a stale height would keep alive.
        let rows = inner_rows(&mut app, width, height);
        assert!(
            content_height(&mut app, width) > inner_height && app.preview_scroll > 0,
            "the transcript must overflow and really be scrolled, or nothing can \
             survive into the card; drawn rows: {rows:?}"
        );

        crate::tui::compose::open_background(&mut app, Some("planner".to_string()));
        let card = draft_card(
            app.draft.as_ref().expect("the draft card is open"),
            &app.launch_dir,
            app.tick,
        );
        assert!(
            wrapped_text_rows(&card, width - 2) <= inner_height,
            "the card must FIT this pane, so any offset at all is the transcript's \
             leaking through"
        );

        let rows = inner_rows(&mut app, width, height);
        assert!(
            rows[0].contains(DRAFT_CARD_HEADLINE),
            "the card must start on the pane's first row, not be scrolled off by a \
             height borrowed from the transcript; drawn rows: {rows:?}"
        );
    }

    // --- the windowed transcript render -------------------------------------

    /// A preview pane the transcripts below overflow several times over, and NARROW
    /// enough that most of their lines word-wrap — so an offset routinely lands in
    /// the MIDDLE of a logical line and the window has a residual to get right.
    const WINDOW_PANE: (u16, u16) = (44, 12);

    /// The rows a WHOLE-transcript `Paragraph` paints at `offset` — the render the
    /// windowed one replaced, rebuilt here as the reference to match against.
    ///
    /// Deliberately NOT derived from the window: it hands the widget every line and
    /// the absolute offset, exactly as `render_preview` did before, so a window that
    /// starts a line early or a residual off by a row shows up as a row of text that
    /// disagrees.
    fn unwindowed_rows(
        lines: &[Line<'static>],
        offset: u16,
        (inner_w, inner_h): (u16, u16),
    ) -> Vec<String> {
        let area = Rect {
            x: 0,
            y: 0,
            width: inner_w,
            height: inner_h,
        };
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::Widget::render(
            Paragraph::new(Text::from(lines.to_vec()))
                .wrap(Wrap { trim: false })
                .scroll((offset, 0)),
            area,
            &mut buffer,
        );
        (0..inner_h)
            .map(|y| {
                (0..inner_w)
                    .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol().to_string()))
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// A board over a generated, wrapping, overflowing transcript with NO query, so
    /// the window can be compared against the whole-transcript render without marks
    /// entering into it.
    fn window_app(dir: &Path) -> App {
        App::new(
            vec![jump_session_at(dir, "sess-window-1")],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        )
    }

    /// THE window's contract: at EVERY offset the pane can be scrolled to, handing
    /// the widget only the lines the viewport can reach paints exactly what handing
    /// it the whole transcript painted.
    ///
    /// Driven over every offset from the top to past the end rather than a sample,
    /// because the ways a window goes wrong are positional: it starts one logical
    /// line early or late, or it drops the residual and snaps to that line's FIRST
    /// row. Each of those is invisible at offset 0 and at any offset that happens to
    /// fall on a line boundary, which is most of them on an unwrapped fixture — hence
    /// a pane narrow enough to wrap, re-asserted below.
    #[test]
    fn a_windowed_render_paints_what_the_whole_transcript_render_painted() {
        let (width, height) = WINDOW_PANE;
        let inner = (width - 2, height - 2);
        let dir = unique_temp_dir("window-parity");
        let mut app = window_app(&dir);

        let lines = app.preview_text(inner.0).lines;
        let prefix = wrapped_row_prefix(&lines, inner.0);
        let content_h = prefix.last().copied().expect("a non-empty prefix map");
        assert!(
            content_h > usize::from(inner.1) * 2,
            "the fixture must overflow this pane several times over (content_h={content_h})"
        );
        assert!(
            prefix.windows(2).any(|pair| pair[1] - pair[0] > 1),
            "some line must WRAP, or every offset lands on a line boundary and the \
             residual is never exercised"
        );
        let max_offset = content_h - usize::from(inner.1);

        app.preview_follow_bottom = false;
        // Past the end too: the clamp must still land the pane on the last page.
        for offset in 0..=(max_offset + 5) {
            app.preview_scroll = u32::try_from(offset).expect("a small test offset");
            let drawn = inner_rows(&mut app, width, height);
            let expected = unwindowed_rows(
                &lines,
                u16::try_from(offset.min(max_offset)).expect("a small test offset"),
                inner,
            );
            assert_eq!(
                drawn, expected,
                "the windowed render must match the whole-transcript render at offset {offset}"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An offset landing PART WAY into a wrapped logical line paints that line's
    /// LATER rows, not its first.
    ///
    /// The trap the residual exists for: a window whose first line is the one holding
    /// the offset, drawn with no residual, silently rewinds the pane to that line's
    /// start — a scroll that visibly refuses to move by a row at a time through long
    /// turns. Asserted against the whole-transcript render at the SAME offset, and
    /// against the fact that the two rows differ from each other, so it cannot pass by
    /// both being the line's first row.
    #[test]
    fn an_offset_inside_a_wrapped_line_paints_that_line_from_the_right_row() {
        let (width, height) = WINDOW_PANE;
        let inner = (width - 2, height - 2);
        let dir = unique_temp_dir("window-residual");
        let mut app = window_app(&dir);

        let lines = app.preview_text(inner.0).lines;
        let prefix = wrapped_row_prefix(&lines, inner.0);
        // A line that wraps to at least three rows, and an offset one row INTO it —
        // so the pane's top row is that line's SECOND row.
        let (line_idx, _) = prefix
            .windows(2)
            .enumerate()
            .find(|(_, pair)| pair[1] - pair[0] >= 3)
            .expect("the fixture must hold a line wrapping to three rows or more");
        let offset = prefix[line_idx] + 1;

        app.preview_follow_bottom = false;
        app.preview_scroll = u32::try_from(offset).expect("a small test offset");
        let drawn = inner_rows(&mut app, width, height);

        let at_line_start = unwindowed_rows(
            &lines,
            u16::try_from(prefix[line_idx]).expect("a small test offset"),
            inner,
        );
        assert_ne!(
            drawn[0], at_line_start[0],
            "the fixture's wrapped line must have DIFFERENT first and second rows, or \
             a dropped residual is undetectable"
        );
        assert_eq!(
            drawn[0], at_line_start[1],
            "the pane's top row must be the wrapped line's SECOND row; drawn: {drawn:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A window that does not start at line 0 still marks the RIGHT words.
    ///
    /// The marks are keyed to the whole transcript, so a window has to add its own
    /// start index back before looking one up. Reading the map at the window-relative
    /// index instead marks real occurrences onto whatever text happens to sit that
    /// many lines below the window's top — a wrong highlight with nothing on screen to
    /// betray it, which is why this asserts the marked CELLS say the query and that a
    /// scrolled pane really is windowed past line 0.
    #[test]
    fn marks_inside_a_window_that_starts_past_line_zero_land_on_the_query() {
        let (width, height) = WINDOW_PANE;
        let inner_w = width - 2;
        let dir = unique_temp_dir("window-marks");
        let mut app = jump_app(&dir);

        // Park the pane on the LAST match, which sits well below the top of the
        // transcript — so the window it is drawn in cannot start at line 0.
        let geometry = jump_geometry(&mut app, WINDOW_PANE);
        assert!(
            geometry.rows_above > usize::from(geometry.inner_h),
            "the target match must be more than one viewport down, or the window \
             starts at line 0 and this proves nothing (rows_above={})",
            geometry.rows_above
        );
        app.preview_follow_bottom = false;
        app.preview_scroll = u32::try_from(geometry.rows_above).expect("a small test offset");

        let window = app.preview_window(inner_w, geometry.rows_above, height - 2);
        assert!(
            window.start > 0,
            "the drawn window must really start past line 0, or the absolute-index \
             lookup is never exercised"
        );

        let drawn = preview_buffer(&mut app, width, height);
        let runs = marked_runs(&drawn, width, height);
        assert!(
            !runs.is_empty(),
            "the match this pane is parked on must be marked; rows: {:?}",
            (0..height)
                .map(|y| row_text(&drawn, y, width))
                .collect::<Vec<_>>()
        );
        assert!(
            runs.iter().all(|run| run == JUMP_QUERY),
            "only the query may be marked in a scrolled window, got: {runs:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ratatui's own vertical thumb glyph, read off the drawn cells below.
    const THUMB_GLYPH: &str = "\u{2588}";

    /// The scrollbar keeps describing the WHOLE transcript once the transcript stops
    /// being what the widget is handed.
    ///
    /// A scrollbar sized from the window would be a full-length thumb on every frame:
    /// the window IS the viewport, so it always fits. What makes it a scrollbar is
    /// that its travel spans everything there is to read — so a pane scrolled to the
    /// middle of a long transcript shows a SHORT thumb detached from both ends of the
    /// track, and only the transcript's true bottom shows the end arrow.
    #[test]
    fn the_scrollbar_describes_the_whole_transcript_not_the_window() {
        let (width, height) = WINDOW_PANE;
        let inner_h = height - 2;
        let dir = unique_temp_dir("window-scrollbar");
        let mut app = window_app(&dir);

        let content_h = content_height(&mut app, width);
        let max_offset = content_h - usize::from(inner_h);
        assert!(
            max_offset > usize::from(inner_h),
            "the transcript must be several viewports long, or the thumb's travel \
             says nothing (max_offset={max_offset})"
        );

        // Track rows, excluding the two reserved boundary-arrow slots.
        let track = 2..height - 2;
        let thumb_rows = |buffer: &ratatui::buffer::Buffer| -> Vec<u16> {
            track
                .clone()
                .filter(|&y| {
                    buffer
                        .cell((width - 1, y))
                        .is_some_and(|cell| cell.symbol() == THUMB_GLYPH)
                })
                .collect()
        };

        app.preview_follow_bottom = false;
        app.preview_scroll = u32::try_from(max_offset / 2).expect("a small test offset");
        let middle = preview_buffer(&mut app, width, height);
        let thumb = thumb_rows(&middle);
        assert!(
            !thumb.is_empty() && thumb.len() < track.len(),
            "a mid-scroll thumb must be present and SHORTER than the track — a \
             window-sized scrollbar would fill it; thumb rows: {thumb:?}"
        );
        assert_eq!(
            middle.cell((width - 1, height - 2)).map(|c| c.symbol()),
            Some(SCROLLBAR_ARROW_HIDDEN),
            "the end arrow belongs to the transcript's bottom, not the window's"
        );

        // And the transcript's real bottom — an offset the window itself cannot tell
        // apart from the mid-scroll one, since both hand the widget one viewport.
        app.preview_scroll = u32::try_from(max_offset).expect("a small test offset");
        let bottom = preview_buffer(&mut app, width, height);
        assert_eq!(
            bottom.cell((width - 1, height - 2)).map(|c| c.symbol()),
            Some(SCROLLBAR_END_ARROW),
            "only the whole transcript's last page may show the end arrow"
        );
        assert!(
            thumb_rows(&bottom).last() > thumb.last(),
            "and the thumb must have travelled DOWN the track between the two"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The url behind the link fixture below, and the label it renders as.
    const WINDOW_LINK_URL: &str = "https://example.com/windowed";
    const WINDOW_LINK_LABEL: &str = "docs";

    /// A transcript of SHORT turns — every rendered line fits the pane, so no line
    /// above the click is word-broken and the test below is about the WINDOW alone —
    /// ending in one markdown link, far enough down that the pane must scroll to
    /// reach it.
    ///
    /// That is also what this fixture CANNOT see, and why it has a sibling: with
    /// nothing wrapped above the click, a hit-test that mapped rows with a
    /// character-packing model of its own resolves the same line as the wrapper, so
    /// the drift such a model accumulates is invisible here. The wrapping case is
    /// [`wrapped_link_session`].
    fn window_link_session(dir: &Path) -> Session {
        let file = dir.join("sess-window-link.jsonl");
        let mut body: String = (1..=30).map(|i| format!("turn {i}\\n")).collect();
        body.push_str(&format!(
            "open [{WINDOW_LINK_LABEL}]({WINDOW_LINK_URL}) here"
        ));
        let jsonl = format!(
            concat!(
                r#"{{"type":"user","sessionId":"sess-window-link","cwd":"/tmp","#,
                r#""timestamp":"2026-07-01T10:00:00.000Z","#,
                r#""message":{{"role":"user","content":"{body}"}}}}"#,
                "\n",
            ),
            body = body,
        );
        std::fs::write(&file, jsonl).expect("write the windowed link fixture");
        Session {
            file,
            session_id: "sess-window-link".to_string(),
            cwd: PathBuf::from("/tmp"),
            git_branch: Some("main".to_string()),
            timestamp: None,
            repo: "repo".to_string(),
            label: "windowed link session".to_string(),
            root_uuid: None,
            msg_count: 0,
            content_index: String::new(),
        }
    }

    /// A mouse click resolves the link under it on a SCROLLED, windowed pane.
    ///
    /// `link_at` hit-tests in ABSOLUTE wrapped rows, and the windowed render is what
    /// turns that from a tautology into a claim: the widget is scrolled by a small
    /// RESIDUAL inside a slice now, so if the pane's absolute offset and its painted
    /// top row ever came apart, every click on a scrolled pane would open a link from
    /// somewhere else in the file. Aimed at the cell the label was actually PAINTED
    /// in — found by the UNDERLINED modifier the preview marks a label with, never
    /// computed from the geometry under test.
    #[test]
    fn a_click_resolves_the_link_under_it_on_a_scrolled_windowed_pane() {
        let (width, height) = WINDOW_PANE;
        let inner_w = width - 2;
        let dir = unique_temp_dir("window-link");
        let mut app = App::new(
            vec![window_link_session(&dir)],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );

        // The default bottom anchor scrolls the pane to the tail, where the link is.
        let buffer = preview_buffer(&mut app, width, height);
        assert!(
            app.preview_scroll > 0,
            "the fixture must overflow the pane, or nothing here is windowed"
        );
        let offset = usize::try_from(app.preview_scroll).expect("a small test offset");
        assert!(
            app.preview_window(inner_w, offset, height - 2).start > 0,
            "the drawn window must really start past line 0, or an absolute offset \
             and a window-relative one are indistinguishable"
        );

        let inner = Rect {
            x: 1,
            y: 1,
            width: inner_w,
            height: height - 2,
        };
        let (col, row) = (inner.y..inner.bottom())
            .flat_map(|y| (inner.x..inner.right()).map(move |x| (x, y)))
            .find(|&(x, y)| {
                buffer
                    .cell((x, y))
                    .is_some_and(|c| c.modifier.contains(Modifier::UNDERLINED))
            })
            .expect("the fixture's link label must be drawn inside the pane");

        let (row_prefix, regions) = app.preview_hit_context(inner_w);
        assert_eq!(
            link_at(col, row, inner, app.preview_scroll, &row_prefix, &regions),
            Some(WINDOW_LINK_URL),
            "a click on the cell the label was DRAWN on must open its url"
        );
        assert_eq!(
            link_at(
                col,
                row - 1,
                inner,
                app.preview_scroll,
                &row_prefix,
                &regions
            ),
            None,
            "and the row above it is another transcript line, not the link"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The url behind the WRAPPING link fixture below.
    const WRAP_LINK_URL: &str = "https://example.com/below-wrapping-lines";

    /// A turn body whose rendered line must WORD-WRAP into more rows than a
    /// `ceil(width / inner)` model charges for.
    ///
    /// Three tokens, each longer than half [`WINDOW_PANE`]'s inner width, so the
    /// wrapper has to break after every one of them while packing bills the same
    /// text at one row fewer — the per-line disagreement that used to ACCUMULATE
    /// down a transcript.
    const WRAP_LINK_BODY: &str =
        "synchronization-checkpoint instrumentation-rollout deployment-verification";

    /// How many wrapping turns sit ABOVE the link. Enough that the two models are
    /// many rows apart by the time the click happens, so the drift is the reason a
    /// pre-fix hit-test misses rather than an off-by-one that could go either way.
    const WRAP_LINK_TURNS: usize = 24;

    /// A transcript of LONG turns — every rendered body line WRAPS at the pane's
    /// inner width — ending in one markdown link, far enough down that the pane must
    /// scroll to reach it.
    ///
    /// The wrapping lines ABOVE the link are the whole point, and the deliberate
    /// opposite of [`window_link_session`]'s short turns: they are what a per-line
    /// character-packing walk mis-counts, one row at a time, all the way down to the
    /// click.
    fn wrapped_link_session(dir: &Path) -> Session {
        let file = dir.join("sess-wrap-link.jsonl");
        let mut out = String::new();
        for turn in 0..WRAP_LINK_TURNS {
            out.push_str(&format!(
                concat!(
                    r#"{{"type":"user","sessionId":"sess-wrap-link","cwd":"/tmp","#,
                    r#""timestamp":"2026-07-01T10:00:00.000Z","#,
                    r#""message":{{"role":"user","content":"{turn} {body}"}}}}"#,
                    "\n",
                ),
                turn = turn,
                body = WRAP_LINK_BODY,
            ));
        }
        out.push_str(&format!(
            concat!(
                r#"{{"type":"user","sessionId":"sess-wrap-link","cwd":"/tmp","#,
                r#""timestamp":"2026-07-01T10:00:00.000Z","#,
                r#""message":{{"role":"user","content":"open [docs]({url}) here"}}}}"#,
                "\n",
            ),
            url = WRAP_LINK_URL,
        ));
        std::fs::write(&file, out).expect("write the wrapping link fixture");
        Session {
            file,
            session_id: "sess-wrap-link".to_string(),
            cwd: PathBuf::from("/tmp"),
            git_branch: Some("main".to_string()),
            timestamp: None,
            repo: "repo".to_string(),
            label: "wrapping link session".to_string(),
            root_uuid: None,
            msg_count: 0,
            content_index: String::new(),
        }
    }

    /// A click still resolves its own link with WRAPPING lines above it.
    ///
    /// The case [`window_link_session`]'s short turns cannot reach, and the one the
    /// deleted `PREVIEW_LINES` cap used to bound: the hit-test's row mapping used to
    /// walk a character-packing model over every line above the click, so each
    /// word-broken line above it cost one row of drift and the click resolved that
    /// many lines too far down the file — a neighbouring line's url, or none. With no
    /// line cap left, nothing bounded how far that could go.
    ///
    /// The fixture PROVES it is such a case before it asserts anything, by measuring
    /// the same drift the old mapping accumulated; a fixture whose lines happen to
    /// fit the pane would pass against the bug.
    #[test]
    fn a_click_below_wrapping_lines_resolves_the_link_under_it() {
        let (width, height) = WINDOW_PANE;
        let inner_w = width - 2;
        let dir = unique_temp_dir("wrap-link");
        let mut app = App::new(
            vec![wrapped_link_session(&dir)],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );

        // The default bottom anchor scrolls the pane to the tail, where the link is.
        let buffer = preview_buffer(&mut app, width, height);
        let lines = app.preview_text(inner_w).lines;
        let exact = wrapped_row_prefix(&lines, inner_w)
            .last()
            .copied()
            .expect("a non-empty prefix map");
        let packed: usize = lines
            .iter()
            .map(|l| wrapped_line_height(l.width(), inner_w))
            .sum();
        assert!(
            exact > packed + usize::from(height),
            "the fixture's lines must WRAP enough that a packed walk drifts by more \
             than a viewport before the click — else the pre-fix mapping could still \
             land on the right line (exact={exact}, packed={packed})"
        );
        assert!(
            app.preview_scroll > 0,
            "the fixture must overflow the pane, or nothing here is scrolled"
        );

        let inner = Rect {
            x: 1,
            y: 1,
            width: inner_w,
            height: height - 2,
        };
        let (col, row) = (inner.y..inner.bottom())
            .flat_map(|y| (inner.x..inner.right()).map(move |x| (x, y)))
            .find(|&(x, y)| {
                buffer
                    .cell((x, y))
                    .is_some_and(|c| c.modifier.contains(Modifier::UNDERLINED))
            })
            .expect("the fixture's link label must be drawn inside the pane");

        let (row_prefix, regions) = app.preview_hit_context(inner_w);
        assert_eq!(
            link_at(col, row, inner, app.preview_scroll, &row_prefix, &regions),
            Some(WRAP_LINK_URL),
            "a click on the cell the label was DRAWN on must open its url, however \
             many wrapped lines sit above it"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- in-preview search-match marking ------------------------------------

    /// A preview pane wide enough that the fixture's turns are not wrapped into
    /// pieces too small to read a marked word off, and tall enough to show them.
    const MARK_PANE: (u16, u16) = (60, 20);

    /// A word the `sess-normal-1` fixture says in its SUMMARY and in two turns, so
    /// a query for it is both a label hit (name-only mode keeps the row) and a
    /// transcript hit (something to mark).
    const MARK_QUERY: &str = "webhook";

    /// The sample session carrying the label the store would derive from its
    /// summary, so a name-only query for [`MARK_QUERY`] keeps the row on the board.
    fn markable_session() -> Session {
        let mut session = sample_session();
        session.label = "Fix the payment webhook retries".to_string();
        session
    }

    // --- in-preview match jump ---------------------------------------------

    /// Preview pane for the jump tests. NARROW enough that the turns below have to
    /// word-wrap (which is what makes the wrapper's answer differ from a
    /// character-packing one) and SHORT enough that the transcript overflows it,
    /// so a scroll offset is a real position rather than a clamped zero.
    const JUMP_PANE: (u16, u16) = (44, 14);

    /// The word the jump tests search for. It sits INSIDE a longer token, so the
    /// mark is a substring hit rather than a whole line.
    const JUMP_QUERY: &str = "beacon";

    /// A turn body carrying [`JUMP_QUERY`]. Three tokens, each longer than half the
    /// pane's inner width, so the wrapper has to break after each one — which is
    /// exactly where a `ceil(width / inner)` model disagrees with it.
    const JUMP_BODY_HIT: &str =
        "telemetry-beacon-pipeline synchronization-checkpoint instrumentation-rollout";

    /// The same shape with no [`JUMP_QUERY`] in it, so a turn can pad the transcript
    /// without adding a match.
    const JUMP_BODY_MISS: &str =
        "synchronization-checkpoint instrumentation-rollout deployment-verification";

    /// Which of the turns below say [`JUMP_QUERY`]. Two of them, both well ABOVE the
    /// end of the transcript, so the jump's offset is what positions the pane rather
    /// than the bottom clamp — and so there is a second match to step back to.
    const JUMP_HIT_TURNS: [usize; 2] = [1, 4];

    /// How many turns the generated transcript holds. Enough to overflow
    /// [`JUMP_PANE`] several times over, with the last hit turn far from the tail.
    const JUMP_TURNS: usize = 12;

    /// Write a transcript into `dir` shaped for the geometry tests, and hand back a
    /// `Session` over it.
    ///
    /// Generated rather than checked in: what these tests need is a WRAPPING,
    /// OVERFLOWING pane with a match at a known depth, which is a statement about
    /// geometry, not about the JSONL format — the checked-in fixtures exist for
    /// format edge cases and are (rightly) too small to overflow anything.
    fn jump_session_at(dir: &Path, id: &str) -> Session {
        jump_session_of(dir, id, JUMP_TURNS)
    }

    /// The same transcript, `turns` turns long.
    ///
    /// A session GROWS while the board is open — claude writing the reply is exactly
    /// that — and rewriting the file longer is what a reload then sees. It is the only
    /// way to tell a pane that is FOLLOWING the newest turn from one merely parked on
    /// today's last row: both draw the same pane until the transcript moves.
    fn jump_session_of(dir: &Path, id: &str, turns: usize) -> Session {
        let path = dir.join(format!("{id}.jsonl"));
        let mut out =
            String::from(r#"{"type":"summary","summary":"Telemetry rollout","leafUuid":"j1"}"#);
        out.push('\n');
        for turn in 0..turns {
            let body = if JUMP_HIT_TURNS.contains(&turn) {
                JUMP_BODY_HIT
            } else {
                JUMP_BODY_MISS
            };
            let role = if turn % 2 == 0 { "user" } else { "assistant" };
            out.push_str(&format!(
                r#"{{"type":"{role}","sessionId":"{id}","cwd":"/Users/me/project-alpha","gitBranch":"main","timestamp":"2026-07-01T10:00:00.000Z","message":{{"role":"{role}","content":"{body}"}}}}"#
            ));
            out.push('\n');
        }
        std::fs::write(&path, out).expect("write the generated transcript");
        Session {
            file: path,
            session_id: id.to_string(),
            cwd: PathBuf::from("/Users/me/project-alpha"),
            git_branch: Some("main".to_string()),
            timestamp: None,
            repo: "project-alpha".to_string(),
            label: "Telemetry rollout".to_string(),
            root_uuid: None,
            msg_count: turns,
            // A CONTENT hit and not a label one, which is the case the autoscroll
            // exists for: the row says nothing about the query, so the pane has to.
            content_index: JUMP_BODY_HIT.to_string(),
        }
    }

    /// A board over [`jump_session`], already searching for [`JUMP_QUERY`] in
    /// name+content mode — the only mode the automatic jump fires in.
    fn jump_app(dir: &Path) -> App {
        let mut app = App::new(
            vec![jump_session_at(dir, "sess-jump-1")],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        app.toggle_search_mode();
        assert_eq!(app.search_mode, SearchMode::NameAndContent);
        app.push_query_str(JUMP_QUERY);
        assert!(
            app.selected.is_some(),
            "the query must keep the row on the board, or nothing is previewed"
        );
        app
    }

    /// The FIRST wrapped row `line` occupies at `inner_width`, painted by the same
    /// wrapper the pane uses.
    ///
    /// The ground truth the jump is checked against: it is read off a real render of
    /// that one line, never derived from the offset arithmetic under test.
    fn first_wrapped_row(line: &Line<'static>, inner_width: u16) -> String {
        let area = Rect {
            x: 0,
            y: 0,
            width: inner_width,
            height: 1,
        };
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::Widget::render(
            Paragraph::new(Text::from(vec![line.clone()])).wrap(Wrap { trim: false }),
            area,
            &mut buffer,
        );
        (0..inner_width)
            .filter_map(|x| buffer.cell((x, 0)).map(|cell| cell.symbol().to_string()))
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    /// Everything the jump tests need to state their preconditions: the pane's
    /// inner geometry, the transcript, and where the target match sits in it.
    struct JumpGeometry {
        inner_w: u16,
        inner_h: u16,
        /// Rows the pane leaves ABOVE a jumped-to match.
        lead: usize,
        /// The whole transcript's wrapped height.
        content_h: usize,
        /// The matched line the next jump targets.
        target: usize,
        /// Wrapped rows above `target` — the row it starts on, per the WRAPPER.
        rows_above: usize,
        /// The same count per the APPROXIMATE character-packing model.
        packed_above: usize,
        /// Every marked line, ascending.
        marked: Vec<usize>,
        lines: Vec<Line<'static>>,
    }

    /// Measure the pane the jump is about to be asserted against.
    fn jump_geometry(app: &mut App, (width, height): (u16, u16)) -> JumpGeometry {
        let inner_w = width - 2;
        let inner_h = height - 2;
        let lines = app.preview_text(inner_w).lines;
        let target = app
            .preview_match_target()
            .expect("the query must mark something in this transcript");
        let mut marked: Vec<usize> = app
            .preview_matches(inner_w)
            .expect("a selected session has a match map")
            .keys()
            .copied()
            .collect();
        marked.sort_unstable();
        JumpGeometry {
            inner_w,
            inner_h,
            lead: usize::from(inner_h / MATCH_JUMP_LEAD_DIVISOR),
            content_h: wrapped_text_rows(&lines, inner_w),
            target,
            rows_above: wrapped_text_rows(&lines[..target], inner_w),
            packed_above: lines[..target]
                .iter()
                .map(|line| wrapped_line_height(line.width(), inner_w))
                .sum(),
            marked,
            lines,
        }
    }

    /// The offset arithmetic, stated as arithmetic.
    #[test]
    fn match_jump_offset_leaves_a_third_of_the_viewport_above_the_match() {
        // A match deep in a transcript keeps `lead` rows of context above it.
        assert_eq!(
            match_jump_offset(23, 12),
            u32::from(23 - 12 / MATCH_JUMP_LEAD_DIVISOR)
        );
        // A match INSIDE the lead cannot scroll above the transcript's start.
        assert_eq!(match_jump_offset(4, 12), 0);
        assert_eq!(match_jump_offset(0, 12), 0);
        // A degenerate pane asks for no lead at all rather than dividing badly.
        assert_eq!(match_jump_offset(7, 0), 7);
        assert_eq!(match_jump_offset(7, 2), 7);
        // A match BEYOND `u16::MAX` wrapped rows is an ordinary position in the
        // `u32` offset domain, jumped to exactly — not pinned at 65,535, which is
        // where the pre-widening return type parked every one of them.
        assert_eq!(match_jump_offset(200_000, 30), 200_000 - 10);
        // Only a `usize` past `u32::MAX` saturates, and it saturates at the WIDER
        // ceiling.
        assert_eq!(match_jump_offset(usize::MAX, 30), u32::MAX);
    }

    /// THE core claim: the offset the jump resolves puts the matched line exactly
    /// where the wrapper paints it.
    ///
    /// It is asserted against a real render, never against the same arithmetic that
    /// produced it: the row drawn at the lead must be the target line's own first
    /// wrapped row, taken from a separate paint of that one line. Three
    /// preconditions keep it from passing for the wrong reason — the transcript
    /// must OVERFLOW (or every offset clamps to 0), the match must sit far enough
    /// from BOTH ends that neither clamp is what positions it, and the approximate
    /// character-packing model must DISAGREE with the wrapper here (otherwise the
    /// test cannot tell the two apart, which is the whole thing it exists to do).
    #[test]
    fn the_match_jump_parks_the_matched_line_where_the_wrapper_paints_it() {
        let dir = unique_temp_dir("jump-offset");
        let (width, height) = JUMP_PANE;
        let mut app = jump_app(&dir);
        let geo = jump_geometry(&mut app, JUMP_PANE);

        assert!(
            geo.marked.len() > 1,
            "the transcript must say the query more than once, or 'the most recent \
             match' is not a choice this test can check"
        );
        assert_eq!(
            Some(geo.target),
            geo.marked.last().copied(),
            "the pane opens on the MOST RECENT match — the end the transcript is \
             read from, and the end its bottom anchor already sits at"
        );
        assert!(
            geo.content_h > usize::from(geo.inner_h),
            "the transcript must overflow the pane, or every offset clamps to 0"
        );
        assert!(
            geo.rows_above > geo.lead,
            "the match must sit below the lead, or the top clamp positions it"
        );
        assert!(
            geo.rows_above - geo.lead <= geo.content_h - usize::from(geo.inner_h),
            "and far enough from the end that the bottom clamp does not"
        );
        assert_ne!(
            geo.packed_above, geo.rows_above,
            "the approximate packing model must disagree with the wrapper here, or \
             this test cannot tell a wrong measurement from a right one"
        );

        let drawn = preview_buffer(&mut app, width, height);
        let row = row_text(&drawn, 1 + geo.lead as u16, width);
        assert_eq!(
            row,
            first_wrapped_row(&geo.lines[geo.target], geo.inner_w),
            "the matched line must be painted exactly `lead` rows down; drawn: {:?}",
            (0..height)
                .map(|y| row_text(&drawn, y, width))
                .collect::<Vec<_>>()
        );
        assert!(
            marked_runs(&drawn, width, height)
                .iter()
                .any(|run| run == JUMP_QUERY),
            "and the line parked there must be the MARKED one"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A reload leaves the reader's viewport exactly where they put it.
    ///
    /// This is what the jump costs if it is armed in the wrong place. A live session
    /// appends turns at the watcher's cadence, so a jump on reload would yank the
    /// pane back to a match every few hundred milliseconds while the user is reading
    /// somewhere else. Asserted as the whole drawn pane, because what would be lost
    /// is the view, not a field.
    #[test]
    fn a_reload_leaves_the_readers_viewport_alone() {
        let dir = unique_temp_dir("jump-reload");
        let (width, height) = JUMP_PANE;
        let mut app = jump_app(&dir);
        let jumped = preview_buffer(&mut app, width, height);
        let jump_offset = app.preview_scroll;
        assert!(
            jump_offset > 0,
            "the jump must have moved the pane, or there is nothing to disturb"
        );

        // The user then reads somewhere else entirely.
        app.preview_top();
        let reading = preview_buffer(&mut app, width, height);
        assert_ne!(
            app.preview_scroll, jump_offset,
            "the fixture must leave the reader off the match"
        );
        let parked = app.preview_scroll;

        // The watcher's reload: every transcript re-read, the selection kept by id.
        app.apply_sessions(app.sessions.clone());
        let after = preview_buffer(&mut app, width, height);
        assert_eq!(
            app.preview_scroll, parked,
            "a reload must not move the preview's offset"
        );
        assert_eq!(
            buffer_rows(&after, width, height),
            buffer_rows(&reading, width, height),
            "and must repaint the same pane the reader was looking at"
        );
        assert_ne!(
            buffer_rows(&after, width, height),
            buffer_rows(&jumped, width, height),
            "the two panes must differ, or this proves nothing"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Selecting another session offers ITS match, the same way typing does.
    #[test]
    fn the_jump_fires_when_the_selection_moves_to_another_session() {
        let dir = unique_temp_dir("jump-select");
        let (width, height) = JUMP_PANE;
        let mut app = App::new(
            vec![
                jump_session_at(&dir, "sess-jump-1"),
                jump_session_at(&dir, "sess-jump-2"),
            ],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        app.toggle_search_mode();
        app.push_query_str(JUMP_QUERY);
        assert_eq!(app.filtered.len(), 2, "both rows must survive the query");
        preview_buffer(&mut app, width, height);
        let first = app.preview_scroll;

        // Read to the top of THIS session, then move to the next one.
        app.preview_top();
        preview_buffer(&mut app, width, height);
        assert_eq!(app.preview_scroll, 0, "the reader is at the top");

        app.move_selection(1);
        preview_buffer(&mut app, width, height);
        assert_eq!(
            app.preview_scroll, first,
            "a newly selected session must open on its match, not at the top or the tail"
        );
        assert!(
            !app.preview_follow_bottom,
            "a jump replaces the bottom anchor rather than fighting it next frame"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A name-only board marks the query but never moves the pane.
    #[test]
    fn a_name_only_board_marks_without_scrolling() {
        let dir = unique_temp_dir("jump-name-only");
        let (width, height) = JUMP_PANE;

        // Name-only: the LABEL carries the query, so the row survives the filter.
        let mut labelled = jump_session_at(&dir, "sess-jump-1");
        labelled.label = format!("{JUMP_QUERY} telemetry rollout");
        let mut app = App::new(vec![labelled], Scope::All, PathBuf::from("/tmp/launch"));
        assert_eq!(app.search_mode, SearchMode::NameOnly);
        app.push_query_str(JUMP_QUERY);
        preview_buffer(&mut app, width, height);
        let name_only_offset = app.preview_scroll;
        assert!(
            app.has_preview_matches(),
            "marking still happens in name-only mode"
        );

        // The bottom anchor, untouched: what the board has always done.
        let mut plain = App::new(
            vec![jump_session_at(&dir, "sess-jump-1")],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        preview_buffer(&mut plain, width, height);
        assert_eq!(
            name_only_offset, plain.preview_scroll,
            "a name-only search must leave the pane where an unsearched board puts it"
        );

        // The same board searching CONTENT does move — so the assertion above is
        // about the MODE, not about a jump that never works.
        let mut content = jump_app(&dir);
        preview_buffer(&mut content, width, height);
        assert_ne!(
            content.preview_scroll, name_only_offset,
            "content mode must scroll onto the match"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A jump requested while a DRAFT CARD owns the pane is dropped, not deferred.
    ///
    /// The card replaces the transcript, so the matched line indices address text
    /// that is not on screen. Deferring the request would fire it on whatever frame
    /// follows the card — moving a pane the user had already put somewhere.
    #[test]
    fn a_draft_card_swallows_the_match_jump() {
        let dir = unique_temp_dir("jump-card");
        let (width, height) = JUMP_PANE;
        let mut app = jump_app(&dir);
        // What the jump WOULD have done, measured on its own board.
        let mut reference = jump_app(&dir);
        preview_buffer(&mut reference, width, height);
        let jump_offset = reference.preview_scroll;

        // The card takes the pane before the jump is ever drawn.
        crate::tui::compose::open_background(&mut app, Some("planner".to_string()));
        let carded = preview_buffer(&mut app, width, height);
        assert!(
            (0..height)
                .map(|y| row_text(&carded, y, width))
                .any(|row| row.contains(DRAFT_CARD_HEADLINE)),
            "the card must own the pane"
        );

        app.close_compose();
        preview_buffer(&mut app, width, height);
        assert_ne!(
            app.preview_scroll, jump_offset,
            "the request must be dropped with the card, not deferred onto the \
             transcript that comes back"
        );
        assert!(
            app.preview_follow_bottom,
            "and the pane keeps the anchor it had"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Widening the search with `Tab` opens the pane on the match it just admitted.
    ///
    /// The row here is one the user found BY NAME and is already looking at, so the
    /// mode flip moves no selection — the flip itself is the whole event. The gate
    /// on the automatic jump is the mode, so opening that gate is a query change in
    /// every way that matters: the same text now matches the transcript too, and the
    /// pane owes the same answer a keystroke gets. It used to sit at the tail until
    /// the user typed one more character.
    #[test]
    fn widening_the_search_to_content_opens_the_pane_on_the_match() {
        let dir = unique_temp_dir("jump-widen");
        let (width, height) = JUMP_PANE;

        // What typing this query in content mode does, measured on its own board.
        let mut reference = jump_app(&dir);
        preview_buffer(&mut reference, width, height);
        let jump_offset = reference.preview_scroll;

        // The LABEL says the query too, so the row is on the board in both modes and
        // no selection changes across the toggle.
        let mut labelled = jump_session_at(&dir, "sess-jump-1");
        labelled.label = format!("{JUMP_QUERY} telemetry rollout");
        let mut app = App::new(vec![labelled], Scope::All, PathBuf::from("/tmp/launch"));
        assert_eq!(app.search_mode, SearchMode::NameOnly, "the default mode");
        app.push_query_str(JUMP_QUERY);
        preview_buffer(&mut app, width, height);
        let parked = app.preview_scroll;
        assert_ne!(
            parked, jump_offset,
            "a name-only board must start away from the match, or this proves nothing"
        );
        let before = app.selected.clone();

        app.toggle_search_mode();
        assert_eq!(
            app.selected, before,
            "the row never left the board, so no selection change can be what moves \
             the pane below"
        );
        preview_buffer(&mut app, width, height);
        assert_eq!(
            app.preview_scroll, jump_offset,
            "Tab must open the pane on the match, not leave it at the tail until \
             the next keystroke"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A jump requested while the pane is HIDDEN is dropped, not deferred.
    ///
    /// `Ctrl-/` takes the pane away without clearing anything behind it, so a query
    /// typed while it is gone still arms the one-shot and `render_preview` — its only
    /// consumer — never runs. Left armed, it fires on the frame the pane comes BACK
    /// on: the user re-opens the preview expecting the newest turn (what the toggle
    /// promises) and lands on a match from a query they have since moved on from.
    #[test]
    fn a_hidden_pane_swallows_the_match_jump() {
        let dir = unique_temp_dir("jump-hidden");
        let (width, height) = JUMP_PANE;

        // The two places this pane can end up: on the match, or at the newest turn.
        let mut reference = jump_app(&dir);
        preview_buffer(&mut reference, width, height);
        let jump_offset = reference.preview_scroll;
        let mut anchored = App::new(
            vec![jump_session_at(&dir, "sess-jump-1")],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        preview_buffer(&mut anchored, width, height);
        let bottom_offset = anchored.preview_scroll;
        assert_ne!(
            jump_offset, bottom_offset,
            "the fixture must tell a jump from the bottom anchor, or this proves nothing"
        );

        let mut app = App::new(
            vec![jump_session_at(&dir, "sess-jump-1")],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        app.toggle_search_mode();
        app.toggle_preview();
        assert!(!app.show_preview, "Ctrl-/ takes the pane away");

        // Searching with no pane on screen: the board still draws, and that frame is
        // where the request has to die.
        let mut terminal =
            Terminal::new(TestBackend::new(80, 24)).expect("build an in-memory test terminal");
        app.push_query_str(JUMP_QUERY);
        terminal
            .draw(|frame| render(frame, &mut app))
            .expect("the board must draw with no preview pane");

        // And the pane comes back where its own toggle puts it.
        app.toggle_preview();
        preview_buffer(&mut app, width, height);
        assert_eq!(
            app.preview_scroll, bottom_offset,
            "a re-opened pane must show the newest turn, not act on a request no \
             frame could see"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A jump requested with NOTHING selected is dropped, not deferred.
    ///
    /// The empty pane returns before the one-shot's only consumer, so the request
    /// outlives the frame that could not act on it. It then fires on a later frame in
    /// NAME-ONLY mode — whose whole rule is that it never moves the pane — because
    /// the mode gate lives at the arming site and a leaked flag is already past it.
    #[test]
    fn an_empty_pane_swallows_the_match_jump() {
        let dir = unique_temp_dir("jump-empty");
        let (width, height) = JUMP_PANE;
        const MISS: &str = "no-such-word-anywhere";

        // Where an unsearched board parks this transcript: the newest turn.
        let mut anchored = App::new(
            vec![jump_session_at(&dir, "sess-jump-1")],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        preview_buffer(&mut anchored, width, height);
        let bottom_offset = anchored.preview_scroll;

        let mut app = App::new(
            vec![jump_session_at(&dir, "sess-jump-1")],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        app.toggle_search_mode();
        app.push_query_str(MISS);
        assert!(
            app.selected.is_none(),
            "the query must empty the board, or the empty pane is never reached"
        );
        let empty = preview_buffer(&mut app, width, height);
        assert!(
            (0..height)
                .map(|y| row_text(&empty, y, width))
                .any(|row| row.contains("No session selected")),
            "the empty pane must be what that frame drew"
        );

        // The user gives up and searches by NAME instead. Name-only arms nothing, so
        // anything that moves the pane from here is the request left over above.
        app.toggle_search_mode();
        assert_eq!(app.search_mode, SearchMode::NameOnly);
        for _ in 0..MISS.chars().count() {
            app.pop_query_char();
        }
        app.push_query_str("telemetry");
        assert!(
            app.selected.is_some(),
            "the label carries this word, so the row comes back"
        );
        let geo = jump_geometry(&mut app, JUMP_PANE);
        assert_ne!(
            match_jump_offset(geo.rows_above, geo.inner_h),
            bottom_offset,
            "the fixture must tell a leaked jump from the anchor, or this proves nothing"
        );

        preview_buffer(&mut app, width, height);
        assert_eq!(
            app.preview_scroll, bottom_offset,
            "a name-only board sits at the newest turn: a dropped request must not \
             fire on a later frame"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `Shift-Up` / `Shift-Down` walk the pane between MARKED LINES, and clamp.
    #[test]
    fn the_shift_arrow_step_walks_between_marked_lines() {
        let dir = unique_temp_dir("jump-step");
        let (width, height) = JUMP_PANE;
        let mut app = jump_app(&dir);
        preview_buffer(&mut app, width, height);
        let geo = jump_geometry(&mut app, JUMP_PANE);
        let last = app.preview_scroll;
        assert_eq!(
            last,
            match_jump_offset(geo.rows_above, geo.inner_h),
            "the pane opens on the last match"
        );

        // Back one match.
        app.preview_match_step(false);
        preview_buffer(&mut app, width, height);
        let previous = app.preview_scroll;
        assert!(
            previous < last,
            "stepping back must move toward the older match, got {previous} then {last}"
        );
        let earlier = app
            .preview_match_target()
            .expect("the step parks on a concrete match");
        assert!(earlier < geo.target, "and on an EARLIER line");
        assert_eq!(
            row_text(
                &preview_buffer(&mut app, width, height),
                1 + geo.lead as u16,
                width
            ),
            first_wrapped_row(&geo.lines[earlier], geo.inner_w),
            "the earlier match must be parked at the same lead"
        );

        // Clamped at the first match rather than wrapping to the far end.
        app.preview_match_step(false);
        preview_buffer(&mut app, width, height);
        assert_eq!(
            app.preview_scroll, previous,
            "a step past the first match re-centers on it"
        );

        // Forward again, back to where it started.
        app.preview_match_step(true);
        preview_buffer(&mut app, width, height);
        assert_eq!(app.preview_scroll, last, "and forward returns to the last");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Put a quick reply in flight for the PREVIEWED session — so the pane has a live
    /// tail that wants the bottom — and hand back the rows that tail occupies.
    ///
    /// The baseline is the transcript's own turn count, so the `▶ you` echo has not
    /// been overtaken by a real turn on disk yet: the tail is at its tallest, which is
    /// the state a send spends its first seconds in.
    fn send_in_flight(app: &mut App, id: &str, inner_w: u16) -> usize {
        app.sending = Some(super::super::app::Sending {
            session_id: id.to_string(),
            message: "any update on the rollout?".to_string(),
            baseline_msg_count: JUMP_TURNS,
        });
        let tail =
            sending_tail(app, inner_w).expect("the send must be in flight for the previewed row");
        wrapped_text_rows(&tail, inner_w)
    }

    /// The offset that shows the last row of a transcript plus an in-flight reply's
    /// tail — where a pane that follows the bottom sits.
    fn bottom_offset(content_h: usize, tail_rows: usize, inner_h: u16) -> u32 {
        (content_h + tail_rows - usize::from(inner_h)) as u32
    }

    /// A match jump holds the pane for the WHOLE of an in-flight reply, not for the
    /// one frame that resolved it.
    ///
    /// The jump is a one-shot; a send is in flight for its whole multi-second life and
    /// its spinner redraws the board every tick. So a pane that followed the bottom
    /// whenever a send was in flight agreed with the jump on the frame that resolved
    /// it and snapped back to the newest turn on the very next one — which is every
    /// frame the reader actually looks at. One frame proves nothing here, so both are
    /// asserted, on the DRAWN row: what the reader lost was the view.
    #[test]
    fn the_match_jump_outlives_the_frames_of_an_in_flight_reply() {
        let dir = unique_temp_dir("jump-sending");
        let (width, height) = JUMP_PANE;
        let mut app = jump_app(&dir);
        let geo = jump_geometry(&mut app, JUMP_PANE);
        let tail_rows = send_in_flight(&mut app, "sess-jump-1", geo.inner_w);

        // The tail must want a DIFFERENT offset than the jump, or nothing here can
        // tell the two apart.
        let bottom = bottom_offset(geo.content_h, tail_rows, geo.inner_h);
        let parked = match_jump_offset(geo.rows_above, geo.inner_h);
        assert!(
            parked < bottom,
            "the match must sit above the in-flight tail, or the jump and the bottom \
             anchor agree (parked={parked}, bottom={bottom})"
        );

        // Frame N: the jump resolves.
        let matched_row = first_wrapped_row(&geo.lines[geo.target], geo.inner_w);
        let first = preview_buffer(&mut app, width, height);
        assert_eq!(app.preview_scroll, parked, "frame N must honour the jump");
        assert_eq!(
            row_text(&first, 1 + geo.lead as u16, width),
            matched_row,
            "frame N must park the matched line at the lead; drawn: {:?}",
            (0..height)
                .map(|y| row_text(&first, y, width))
                .collect::<Vec<_>>()
        );

        // Frame N+1: nothing happened but the spinner's redraw — which is what the
        // send does several times a second until it completes.
        app.tick += 1;
        let second = preview_buffer(&mut app, width, height);
        assert_eq!(
            app.preview_scroll, parked,
            "the next frame must not undo the jump the reader just asked for"
        );
        assert_eq!(
            row_text(&second, 1 + geo.lead as u16, width),
            matched_row,
            "and must still be painting the matched line at the lead; drawn: {:?}",
            (0..height)
                .map(|y| row_text(&second, y, width))
                .collect::<Vec<_>>()
        );

        // `End` is how the reader hands the pane back, and the SAME in-flight reply
        // then streams in at the tail — so holding the jump cannot have cost the
        // ordinary reply its auto-follow.
        app.preview_bottom();
        let ended = preview_buffer(&mut app, width, height);
        assert_eq!(
            app.preview_scroll, bottom,
            "End must hand the pane back to the newest row"
        );
        assert!(
            row_text(&ended, height - 2, width).contains(REPLY_COOKING_LABEL),
            "and the in-flight reply must be what is drawn there; drawn: {:?}",
            (0..height)
                .map(|y| row_text(&ended, y, width))
                .collect::<Vec<_>>()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A pane the reader positioned during a send STAYS there — including when they
    /// scroll DOWN past the last row.
    ///
    /// The release half of the anchor: `preview_follow_bottom` cleared means "the
    /// reader put this pane here", and only a key that says otherwise (`End`, another
    /// row, the preview toggle) takes it back. A scroll states a POSITION, never a
    /// subscription: paging down past the end of everything there is lands on the last
    /// row and stops, so the reply still being written does not drag the pane along.
    /// Proven by GROWING the transcript afterwards, since a pane parked on today's
    /// last row and a pane following the newest one draw identically until one
    /// arrives.
    #[test]
    fn a_scroll_past_the_end_leaves_the_pane_where_the_reader_put_it() {
        let dir = unique_temp_dir("jump-scroll-back");
        let (width, height) = JUMP_PANE;
        let mut app = jump_app(&dir);
        let geo = jump_geometry(&mut app, JUMP_PANE);
        let tail_rows = send_in_flight(&mut app, "sess-jump-1", geo.inner_w);
        let bottom = bottom_offset(geo.content_h, tail_rows, geo.inner_h);

        // Spend the query's pending jump on a frame of its own, so what positions the
        // pane below is the reader's own scrolling and nothing else.
        preview_buffer(&mut app, width, height);

        // The reader reads the start of the transcript while the reply cooks.
        app.preview_top();
        preview_buffer(&mut app, width, height);
        assert_eq!(
            app.preview_scroll, 0,
            "the pane must stay where the reader put it"
        );
        app.tick += 1;
        preview_buffer(&mut app, width, height);
        assert_eq!(
            app.preview_scroll, 0,
            "and stay there on the next frame too, not just the first"
        );

        // Then they page back down, past the end of everything there is.
        let pages = geo.content_h / usize::from(geo.inner_h) + 2;
        for _ in 0..pages {
            app.preview_page_down();
        }
        assert!(
            app.preview_scroll > bottom,
            "the fixture must really ask for rows past the last one (asked {}, \
             bottom={bottom})",
            app.preview_scroll
        );
        assert!(
            !app.preview_follow_bottom,
            "a scroll states where to look, never a request to keep following the \
             newest turn"
        );
        let landed = preview_buffer(&mut app, width, height);
        assert_eq!(
            app.preview_scroll, bottom,
            "the overshoot clamps onto the newest row"
        );
        assert!(
            row_text(&landed, height - 2, width).contains(REPLY_COOKING_LABEL),
            "which is where the in-flight reply is; drawn: {:?}",
            (0..height)
                .map(|y| row_text(&landed, y, width))
                .collect::<Vec<_>>()
        );
        let top = row_text(&landed, 1, width);

        // Claude writes the reply: the transcript GROWS under the pane. A pane that
        // had been handed back to the tail would ride down with it; this one was
        // never handed back, so it keeps showing the rows the reader stopped on.
        const LANDED_TURNS: usize = JUMP_TURNS + 4;
        app.apply_sessions(vec![jump_session_of(&dir, "sess-jump-1", LANDED_TURNS)]);
        let grown = content_height(&mut app, width);
        assert!(
            grown > geo.content_h + usize::from(geo.inner_h),
            "the reply must add more than a viewport, or a parked pane could still \
             show the tail (was {}, now {grown})",
            geo.content_h
        );
        // The tail shrinks as the transcript grows: the turn count has passed the
        // send-time baseline, so the `▶ you` echo yields to the real turn on disk and
        // only the pending `● claude` placeholder is left.
        let landed_tail = wrapped_text_rows(
            &sending_tail(&app, geo.inner_w).expect("the send is still in flight"),
            geo.inner_w,
        );
        assert!(
            landed_tail < tail_rows,
            "the echo must have yielded to the real turn (was {tail_rows}, now \
             {landed_tail})"
        );
        let following = bottom_offset(grown, landed_tail, geo.inner_h);
        assert!(
            following > bottom,
            "the new turns must move the bottom, or a followed pane would sit still \
             too (was {bottom}, now {following})"
        );
        let after = preview_buffer(&mut app, width, height);
        assert_eq!(
            app.preview_scroll, bottom,
            "scrolling to the end is not a subscription to whatever lands next"
        );
        assert_eq!(
            row_text(&after, 1, width),
            top,
            "so the reader keeps reading the row they were on; drawn: {:?}",
            (0..height)
                .map(|y| row_text(&after, y, width))
                .collect::<Vec<_>>()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Park the reader a quarter page above the newest turn and hand back that
    /// offset, with the query's pending jump already spent on a frame of its own.
    ///
    /// Near the tail on purpose: that is where a LATER clamp — a wider pane, a
    /// shorter transcript — can cut the offset short, which is the state the two
    /// CLAMP tests below are about; far from it, nothing clamps and they prove
    /// nothing.
    fn reader_parked_near_the_tail(app: &mut App, (width, height): (u16, u16)) -> u32 {
        preview_buffer(app, width, height);
        app.preview_bottom();
        preview_buffer(app, width, height);
        app.preview_half_up();
        let chosen = app.preview_scroll;
        assert!(
            !app.preview_follow_bottom,
            "scrolling up must hand the pane to the reader"
        );
        preview_buffer(app, width, height);
        assert_eq!(
            app.preview_scroll, chosen,
            "and the pane must sit where they put it, or nothing below is clamped"
        );
        chosen
    }

    /// A RESIZE that cuts a reader's offset short must not hand the pane back to the
    /// tail.
    ///
    /// Widening the pane wraps the same transcript into fewer rows, so an offset the
    /// reader chose can stop existing and the clamp cuts it back to the last row.
    /// That is the geometry moving under a reader who pressed nothing, and the draw
    /// site may not read it as a request: only a key re-arms the anchor. Inferring
    /// one here gave the pane away on the next turn that landed.
    ///
    /// Proven by GROWING the transcript afterwards, since a pane parked on today's
    /// last row and one following the newest draw identically until one arrives.
    #[test]
    fn a_resize_that_clamps_the_offset_leaves_the_pane_where_the_reader_put_it() {
        let dir = unique_temp_dir("anchor-resize");
        let (narrow, height) = JUMP_PANE;
        // Twice the columns: the same turns wrap into far fewer rows, which is what
        // makes the reader's offset unreachable at the new size.
        let wide = narrow * 2;
        let mut app = jump_app(&dir);
        let chosen = reader_parked_near_the_tail(&mut app, JUMP_PANE);

        // The terminal is widened. Nothing the reader did.
        preview_buffer(&mut app, wide, height);
        let clamped = app.preview_scroll;
        assert!(
            clamped < chosen,
            "the resize must really cut the offset short, or the re-arm this pins \
             was never reached (was {chosen}, now {clamped})"
        );
        let held = preview_buffer(&mut app, wide, height);
        let top = row_text(&held, 1, wide);

        // The session gains turns — the only thing that can tell a pane parked on the
        // last row from one following the newest.
        const GROWN_TURNS: usize = JUMP_TURNS + 8;
        app.apply_sessions(vec![jump_session_of(&dir, "sess-jump-1", GROWN_TURNS)]);
        let following = content_height(&mut app, wide) - usize::from(height - 2);
        assert!(
            following > clamped as usize,
            "the new turns must move the bottom, or a followed pane would sit still \
             too (bottom={following}, pane={clamped})"
        );
        let after = preview_buffer(&mut app, wide, height);
        assert_eq!(
            app.preview_scroll, clamped,
            "a resize is not a request to follow the newest turn"
        );
        assert_eq!(
            row_text(&after, 1, wide),
            top,
            "so the reader keeps reading the row they were on; drawn: {:?}",
            (0..height)
                .map(|y| row_text(&after, y, wide))
                .collect::<Vec<_>>()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A transcript that SHRINKS under a reader's offset must not hand the pane back
    /// to the tail either.
    ///
    /// The same clamp, reached the other way: the pane held still and the content
    /// moved. A re-read that renders fewer rows than the reader had scrolled past
    /// leaves an offset the content cannot satisfy, and the clamp cuts it to the last
    /// row exactly as the resize did — a fact about the transcript's new height, and
    /// no more a request than the resize was.
    #[test]
    fn a_shrinking_transcript_leaves_the_pane_where_the_reader_put_it() {
        let dir = unique_temp_dir("anchor-shrink");
        let (width, height) = JUMP_PANE;
        // Short enough that the reader's offset is past everything left, and still
        // long enough to hold both hit turns (so the row keeps its marks).
        const SHRUNK_TURNS: usize = 6;
        let mut app = jump_app(&dir);
        let chosen = reader_parked_near_the_tail(&mut app, JUMP_PANE);

        // The transcript is re-read shorter. Nothing the reader did.
        app.apply_sessions(vec![jump_session_of(&dir, "sess-jump-1", SHRUNK_TURNS)]);
        preview_buffer(&mut app, width, height);
        let clamped = app.preview_scroll;
        assert!(
            clamped < chosen,
            "the shrink must really cut the offset short, or the re-arm this pins \
             was never reached (was {chosen}, now {clamped})"
        );
        let held = preview_buffer(&mut app, width, height);
        let top = row_text(&held, 1, width);

        // And the turns come back.
        app.apply_sessions(vec![jump_session_at(&dir, "sess-jump-1")]);
        let following = content_height(&mut app, width) - usize::from(height - 2);
        assert!(
            following > clamped as usize,
            "the restored turns must move the bottom, or a followed pane would sit \
             still too (bottom={following}, pane={clamped})"
        );
        let after = preview_buffer(&mut app, width, height);
        assert_eq!(
            app.preview_scroll, clamped,
            "a transcript changing height is not a request to follow the newest turn"
        );
        assert_eq!(
            row_text(&after, 1, width),
            top,
            "so the reader keeps reading the row they were on; drawn: {:?}",
            (0..height)
                .map(|y| row_text(&after, y, width))
                .collect::<Vec<_>>()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every drawn row of a preview buffer, borders included, for whole-pane
    /// comparisons.
    fn buffer_rows(buffer: &ratatui::buffer::Buffer, width: u16, height: u16) -> Vec<String> {
        (0..height)
            .map(|y| full_row_text(buffer, y, width))
            .collect()
    }

    /// Render ONLY the preview pane and hand back the buffer, so a marked cell can
    /// be read without the list's own `REVERSED` selection highlight in the frame.
    fn preview_buffer(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height))
            .expect("build an in-memory test terminal");
        terminal
            .draw(|frame| render_preview(frame, app, frame.area()))
            .expect("render_preview must not panic");
        terminal.backend().buffer().clone()
    }

    /// Every contiguous run of cells carrying [`PREVIEW_MATCH_MODIFIER`], as text.
    ///
    /// Reads what was DRAWN rather than what the model intended: a mark that never
    /// reached a cell is not a highlight (PATTERNS — assert drawn cells).
    fn marked_runs(buffer: &ratatui::buffer::Buffer, width: u16, height: u16) -> Vec<String> {
        let mut runs: Vec<String> = Vec::new();
        for y in 0..height {
            let mut run = String::new();
            for x in 0..width {
                let marked = buffer.cell((x, y)).is_some_and(|cell| {
                    cell.modifier.contains(PREVIEW_MATCH_MODIFIER)
                        && !cell.symbol().trim().is_empty()
                });
                match (marked, run.is_empty()) {
                    (true, _) => run.push_str(
                        buffer
                            .cell((x, y))
                            .expect("a cell that was just read")
                            .symbol(),
                    ),
                    (false, false) => runs.push(std::mem::take(&mut run)),
                    (false, true) => {}
                }
            }
            if !run.is_empty() {
                runs.push(run);
            }
        }
        runs
    }

    /// The transcript marks the active query where it occurs — and marks NOTHING
    /// else, so the pane cannot claim a hit the query did not make.
    ///
    /// This is the content-search counterpart of the row-label highlight, and it is
    /// derived by re-searching the RENDERED lines: a position taken from
    /// `content_index` would address a different, lossy extraction of the same
    /// transcript and land on unrelated text here.
    #[test]
    fn the_preview_marks_the_query_where_it_occurs_in_the_transcript() {
        let (width, height) = MARK_PANE;
        let mut app = App::new(
            vec![markable_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );

        // Control: with no query, nothing is marked — so the assertion below can
        // actually fail.
        let clean = preview_buffer(&mut app, width, height);
        assert!(
            marked_runs(&clean, width, height).is_empty(),
            "an unsearched transcript must carry no marks"
        );

        app.push_query_str(MARK_QUERY);
        assert!(
            app.selected.is_some(),
            "the query must keep the session on the board, or nothing is previewed"
        );
        let drawn = preview_buffer(&mut app, width, height);
        let runs = marked_runs(&drawn, width, height);
        assert!(
            !runs.is_empty(),
            "the query occurs in this transcript and must be marked; rows: {:?}",
            (0..height)
                .map(|y| row_text(&drawn, y, width))
                .collect::<Vec<_>>()
        );
        assert!(
            runs.iter().all(|run| run == MARK_QUERY),
            "only the query may be marked, got: {runs:?}"
        );
    }

    /// Marking is a property of the QUERY, not of which haystack the filter is
    /// searching: both modes mark the same occurrences, because the seam scores the
    /// query against the display string either way.
    #[test]
    fn the_preview_marks_the_query_in_both_search_modes() {
        let (width, height) = MARK_PANE;
        let mut session = markable_session();
        // A content hit as well as a label hit, so the row survives EITHER mode.
        session.content_index = "the webhook keeps failing".to_string();
        let mut app = App::new(vec![session], Scope::All, PathBuf::from("/tmp/launch"));
        app.push_query_str(MARK_QUERY);
        assert_eq!(app.search_mode, SearchMode::NameOnly, "the default mode");
        let name_only = marked_runs(&preview_buffer(&mut app, width, height), width, height);
        assert!(!name_only.is_empty(), "name-only mode must mark the query");

        app.toggle_search_mode();
        assert_eq!(app.search_mode, SearchMode::NameAndContent);
        assert!(
            app.selected.is_some(),
            "the row must survive the wider mode too"
        );
        let both = marked_runs(&preview_buffer(&mut app, width, height), width, height);
        assert_eq!(both, name_only, "widening the filter marks the same runs");
    }

    /// A NEW-SESSION draft card is a placeholder for a session that does not exist
    /// yet: it has no transcript, so nothing on it can be a search hit.
    ///
    /// The trap this pins is precise. The match map is keyed by the TRANSCRIPT's
    /// line indices, and the card replaces those lines with four of its own — so an
    /// unsuppressed mark lands on whatever characters of the card happen to sit at
    /// the transcript's matched positions, marking words the user never searched
    /// for.
    #[test]
    fn a_draft_card_carries_no_search_marks() {
        let (width, height) = CARD_PANE;
        let mut app = App::new(
            vec![markable_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        // A query whose matches land at LOW char positions on the transcript's
        // first lines — i.e. positions the short card lines also have, so a
        // leaked mark would really be drawn.
        app.push_query_str("the");
        let transcript = preview_buffer(&mut app, width, height);
        assert!(
            !marked_runs(&transcript, width, height).is_empty(),
            "the transcript must mark this query, or the card proves nothing"
        );

        crate::tui::compose::open_background(&mut app, Some("planner".to_string()));
        let carded = preview_buffer(&mut app, width, height);
        let rows: Vec<String> = (0..height)
            .map(|y| row_text(&carded, y, width))
            .collect::<Vec<_>>();
        assert!(
            rows.iter().any(|row| row.contains(DRAFT_CARD_HEADLINE)),
            "the pane must be showing the card: {rows:?}"
        );
        assert!(
            marked_runs(&carded, width, height).is_empty(),
            "a draft card must carry no search marks: {rows:?}"
        );
    }

    // --- status preview banner ---------------------------------------------

    /// Preview pane size for the banner tests: narrow and SHORT enough that the
    /// `sample_session` fixture's transcript overflows it, which is the case the
    /// banner has to survive (a reported session's transcript grows, and the
    /// preview bottom-anchors by default). Each test re-asserts the overflow
    /// rather than trusting this comment.
    const BANNER_PANE: (u16, u16) = (80, 8);

    /// The sample session, optionally joined to a REPORTED agent carrying
    /// `state`. `state` picks the bucket: `Some("done")` is reported but NOT
    /// live, which is exactly the pair the banner must not conflate.
    ///
    /// Left in `App`'s DEFAULT scroll state on purpose: `preview_follow_bottom`
    /// starts true and is re-armed on every selection change, so this is the
    /// state a user actually sees. Nudging it to the top here would hide whether
    /// the banner survives the anchor it ships with.
    fn banner_app(state: Option<&str>) -> App {
        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        if let Some(state) = state {
            let mut reported = HashMap::new();
            reported.insert(
                "sess-normal-1".to_string(),
                ReportedAgent {
                    kind: "background".to_string(),
                    id: None,
                    state: Some(state.to_string()),
                    status: None,
                    name: None,
                },
            );
            app.set_reported_agents(reported);
        }
        app
    }

    /// The drawn text of one row INSIDE the preview's borders.
    fn row_text(buffer: &ratatui::buffer::Buffer, y: u16, width: u16) -> String {
        (1..width - 1)
            .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol().to_string()))
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    /// Render the preview at `(width, height)` and return every drawn row INSIDE
    /// its borders, top to bottom.
    fn inner_rows(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height))
            .expect("build an in-memory test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_preview(frame, app, area);
            })
            .expect("render_preview must not panic");
        let buffer = terminal.backend().buffer();
        (1..height - 1)
            .map(|y| row_text(buffer, y, width))
            .collect()
    }

    /// The wrapped height of `app`'s preview text at the pane's inner width — the
    /// very count `render_preview` scrolls against, read from the same cache — so a
    /// test can prove its fixture really overflows the viewport.
    fn content_height(app: &mut App, width: u16) -> usize {
        app.preview_wrapped_rows(width - 2)
    }

    #[test]
    fn the_status_banner_is_pinned_to_the_top_of_a_default_bottom_anchored_preview() {
        let (width, height) = BANNER_PANE;
        let mut reported = banner_app(Some("blocked"));

        // The state the banner must survive: `App`'s DEFAULT anchor over a
        // transcript TALLER than the pane. A banner prepended into the scrolled
        // text is pinned off the top here — reachable only via `Home`.
        assert!(
            reported.preview_follow_bottom,
            "the preview must be bottom-anchored by default, or this test proves nothing"
        );
        let inner_height = height - 2;
        let content_h = content_height(&mut reported, width);
        assert!(
            content_h > usize::from(inner_height),
            "the fixture must overflow the pane, or this test proves nothing \
             (content_h={content_h}, inner_height={inner_height})"
        );

        let rows = inner_rows(&mut reported, width, height);
        assert_eq!(
            rows[0], "bg needs input",
            "a reported session must LEAD with its status banner in the default view"
        );
        assert!(
            reported.preview_scroll > 0,
            "the transcript beneath the banner must really be scrolled to the \
             newest turn, not parked at the top where a banner survives for free"
        );

        // The banner steals a row from the pane, not from the transcript's tail:
        // the newest line stays on the bottom row, so the transcript scrolls
        // BENEATH the banner rather than being pushed down by it.
        let mut plain = banner_app(None);
        let plain_rows = inner_rows(&mut plain, width, height);
        assert_eq!(
            rows.last(),
            plain_rows.last(),
            "the newest transcript line must stay anchored to the pane's bottom row"
        );
        assert_ne!(
            plain_rows[0], "bg needs input",
            "the baseline row must be real transcript, or this test proves nothing"
        );
        // The banner costs the transcript EXACTLY one row of viewport — no
        // silent gap under it, no second reserved row.
        assert_eq!(
            reported.preview_viewport_h,
            plain.preview_viewport_h - 1,
            "the pinned banner must cost the transcript exactly one row"
        );
    }

    /// A FINISHED agent must keep its banner: the pane keys on REPORTED, not on
    /// live, so a `done` session still leads with `bg done` rather than silently
    /// losing the row the moment the agent wraps up.
    ///
    /// Guards the seam that liveness's corrected semantics could plausibly have
    /// broken — gating the BANNER on liveness (instead of only Enter's routing)
    /// would blank this row.
    #[test]
    fn a_done_session_still_leads_with_its_status_banner() {
        let (width, height) = BANNER_PANE;
        let mut app = banner_app(Some("done"));

        let rows = inner_rows(&mut app, width, height);
        assert_eq!(
            rows[0], "bg done",
            "a reported-but-finished session must still show its banner"
        );
    }

    #[test]
    fn the_status_banner_is_styled_with_a_named_color_and_bold() {
        // Styling is asserted separately from content (PATTERNS testing rules).
        let (width, height) = BANNER_PANE;
        let mut app = banner_app(Some("blocked"));
        let mut terminal = Terminal::new(TestBackend::new(width, height))
            .expect("build an in-memory test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_preview(frame, &mut app, area);
            })
            .expect("render_preview must not panic");
        let cell = terminal
            .backend()
            .buffer()
            .cell((1, 1))
            .expect("the banner's first cell is inside the rendered buffer");
        assert_eq!(
            cell.fg,
            Color::Cyan,
            "a NAMED ansi color (never RGB) marks the banner as board copy"
        );
        assert!(cell.modifier.contains(Modifier::BOLD));
    }

    /// A session file that EXISTS but holds no renderable turns yet — the real
    /// shape of a live agent that was just started, whose transcript renders to
    /// zero lines.
    ///
    /// Deliberately OUTSIDE the `store/` discovery root: it is handed straight to
    /// a synthetic `Session` and must never be discovered, or it would break the
    /// exact discovered/session counts `store`'s own tests pin.
    fn empty_transcript_fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("preview")
            .join("sess-live-empty-1.jsonl")
    }

    /// A SELECTED, LIVE session whose transcript renders EMPTY must still draw
    /// its banner: the one thing worth showing about a just-started agent is
    /// that it is running, and falling through to the empty-pane placeholder
    /// would both hide that and contradict `update`'s hit-test, which derives
    /// the transcript rect from `banner.is_some()` alone.
    #[test]
    fn a_live_session_with_an_empty_transcript_still_draws_its_banner() {
        let (width, height) = BANNER_PANE;
        let mut session = sample_session();
        session.file = empty_transcript_fixture();
        let mut app = App::new(vec![session], Scope::All, PathBuf::from("/tmp/launch"));
        let mut reported = HashMap::new();
        reported.insert(
            "sess-normal-1".to_string(),
            ReportedAgent {
                kind: "background".to_string(),
                id: None,
                state: Some("blocked".to_string()),
                status: None,
                name: None,
            },
        );
        app.set_reported_agents(reported);

        // The fixture must really render to nothing, or this test proves nothing
        // — it would just be re-testing the ordinary banner path.
        assert!(
            app.preview_text(width - 2).lines.is_empty(),
            "the fixture must render an EMPTY transcript for this test to reach \
             the empty-pane seam"
        );

        let rows = inner_rows(&mut app, width, height);
        assert_eq!(
            rows[0], "bg needs input",
            "a live session must still lead with its status banner when its \
             transcript is empty"
        );
        assert!(
            !rows.iter().any(|row| row.contains("No session selected.")),
            "a SELECTED live session must never fall through to the \
             nothing-selected placeholder: {rows:?}"
        );
    }

    #[test]
    fn the_status_banner_passes_an_unknown_state_through_verbatim() {
        // Fail-soft end to end: schema drift reaches the user unhidden.
        let app = banner_app(Some("compacting"));
        let banner = preview_banner(&app).expect("a reported session yields a banner");
        let text: String = banner.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "bg compacting");
    }

    /// The bordered preview block, as `render_preview` builds it. Shared with the
    /// geometry tests below so they measure against the REAL block rather than a
    /// look-alike.
    fn preview_block() -> Block<'static> {
        Block::default().borders(Borders::ALL).title(" preview ")
    }

    #[test]
    fn preview_split_reserves_one_row_for_a_banner_and_nothing_for_a_banner_less_pane() {
        // Off-origin on purpose: the preview is the RIGHT pane, so a split that
        // assumed x/y of 0 would pass at the origin and fail on a real board.
        let area = Rect {
            x: 37,
            y: 4,
            width: 43,
            height: 16,
        };
        // The rect a transcript occupied BEFORE the banner existed is ratatui's
        // own `Block::inner` — that is where `Paragraph::block` drew it. Asserting
        // against ratatui rather than against a restatement of our arithmetic is
        // what makes "banner-less geometry unchanged" a proof and not a tautology.
        let was = preview_block().inner(area);

        let (banner, transcript) = preview_split(area, false);
        assert_eq!(
            transcript, was,
            "a banner-less pane must hand the transcript the whole inner rect, exactly as before"
        );
        assert!(
            banner.is_empty(),
            "a banner-less pane must reserve no banner row"
        );

        let (banner, transcript) = preview_split(area, true);
        assert_eq!(
            banner,
            Rect {
                height: PREVIEW_BANNER_ROWS,
                ..was
            },
            "the banner pins to the pane's FIRST inner row"
        );
        assert_eq!(
            transcript,
            Rect {
                y: was.y + PREVIEW_BANNER_ROWS,
                height: was.height - PREVIEW_BANNER_ROWS,
                ..was
            },
            "the transcript starts one row lower and gives that row back"
        );
        assert_eq!(
            transcript.width, was.width,
            "the split is VERTICAL only: a banner and a banner-less pane share \
             one width, and therefore one width-scoped preview cache"
        );
    }

    #[test]
    fn preview_split_degrades_to_banner_only_rather_than_overlapping_in_a_short_pane() {
        // Inner height 1: there is room for the banner OR a transcript row, not
        // both. The transcript collapses instead of sharing the banner's row.
        let area = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 3,
        };
        let (banner, transcript) = preview_split(area, true);
        assert_eq!(banner.height, PREVIEW_BANNER_ROWS);
        assert_eq!(transcript.height, 0);
        assert_eq!(
            transcript.y,
            banner.y + banner.height,
            "the transcript must start BELOW the banner even with no room left"
        );

        // A pane with no inner rows at all reserves nothing and never underflows.
        for height in [0u16, 1, 2] {
            let (banner, transcript) = preview_split(Rect { height, ..area }, true);
            assert!(
                banner.is_empty() && transcript.is_empty(),
                "a pane of height {height} has no inner rows to split"
            );
        }
    }

    #[test]
    fn a_banner_less_preview_draws_byte_for_byte_what_one_blocked_paragraph_drew() {
        // The banner made `render_preview` draw the block and the transcript as
        // two passes into two rects. For a session claude never reported that must
        // still paint exactly the single `Paragraph::new(text).block(block)` it
        // replaced — rebuilt here from ratatui's own widgets and compared cell by
        // cell.
        let (width, height) = BANNER_PANE;
        let mut app = banner_app(None);
        let mut actual = Terminal::new(TestBackend::new(width, height))
            .expect("build an in-memory test terminal");
        actual
            .draw(|frame| {
                let area = frame.area();
                render_preview(frame, &mut app, area);
            })
            .expect("render_preview must not panic");

        // The reference: one blocked, wrapped, scrolled paragraph over the WHOLE
        // pane, at the offset the render above resolved.
        let text = app.preview_text(width - 2);
        // Narrowed exactly as `render_preview` narrows it for `Paragraph::scroll`
        // (ratatui's `Position.y` is `u16`), so the reference paragraph is drawn
        // from the same value the pane under test handed the widget.
        let offset = u16::try_from(app.preview_scroll).expect("this fixture fits a u16 offset");
        assert!(
            offset > 0,
            "a scrolled pane, or this compares only offset 0"
        );
        let mut expected = Terminal::new(TestBackend::new(width, height))
            .expect("build an in-memory test terminal");
        expected
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new(text)
                        .block(preview_block())
                        .wrap(Wrap { trim: false })
                        .scroll((offset, 0)),
                    frame.area(),
                );
            })
            .expect("the reference paragraph must not panic");

        // Every column but the rightmost, which is where the scrollbar draws its
        // own separate pass (the reference has none). That column is pinned
        // cell-by-cell, for banner-less sessions, by the scrollbar-geometry tests
        // above — so nothing here is left unasserted.
        let cells = |terminal: &Terminal<TestBackend>| -> Vec<(String, Style)> {
            let buffer = terminal.backend().buffer().clone();
            (0..height)
                .flat_map(|y| (0..width - 1).map(move |x| (x, y)))
                .filter_map(|(x, y)| {
                    buffer
                        .cell((x, y))
                        .map(|c| (c.symbol().to_string(), c.style()))
                })
                .collect()
        };
        assert_eq!(
            cells(&actual),
            cells(&expected),
            "a banner-less pane's geometry, content and styling must be untouched by the banner"
        );
    }

    // --- live badge (list row) --------------------------------------------

    /// A synthetic `ReportedAgent` carrying only what the classifier reads, so each
    /// badge test below states just the kind + qualifier source it cares about.
    fn agent(kind: &str, state: Option<&str>, status: Option<&str>) -> ReportedAgent {
        ReportedAgent {
            kind: kind.to_string(),
            id: None,
            state: state.map(str::to_owned),
            status: status.map(str::to_owned),
            name: None,
        }
    }

    /// Each bucket's color AND pulse together, over both qualifier sources.
    /// Table-driven so the full state -> (color, active) contract reads as one
    /// matrix — the pairing is the point: gray is only honest about a working
    /// agent because it PULSES, and green/yellow only read as "waiting" because
    /// they do not. `is_active` is re-asserted here (it has its own bucket table
    /// in `agents`) precisely because that pairing, not either half alone, is
    /// what makes the palette legible.
    #[test]
    fn each_bucket_maps_to_its_badge_color_and_pulse() {
        // (qualifier, expected color, expected active/pulsing)
        let cases = [
            // Waiting on the user -> the most prominent color, but STEADY.
            (Some("blocked"), Color::Yellow, false),
            // The other "it wants you" spelling: same bucket, so the SAME yellow
            // and the same steadiness.
            (Some("waiting"), Color::Yellow, false),
            // Up but not working -> steady, and green earns its place here
            // rather than being the old hardcoded badge color.
            (Some("idle"), Color::Green, false),
            // Quietly working -> gray, and the pulse is what marks it active.
            (Some("working"), Color::Gray, true),
            // ...and its other spelling must be indistinguishable.
            (Some("busy"), Color::Gray, true),
            // Finished -> green like idle (nothing is wanted from you) and
            // STEADY, because there is no work left to animate.
            (Some("done"), Color::Green, false),
            // Terminal (stopped/failed) -> DARKGRAY and STEADY: the job ended, so
            // it must not pulse and must not read green like a clean finish.
            (Some("stopped"), Color::DarkGray, false),
            (Some("failed"), Color::DarkGray, false),
            // FAIL-SOFT: schema drift tracks the working bucket...
            (Some("compacting"), Color::Gray, true),
            // ...and so does a record with no qualifier at all. Neither may
            // hide activity behind a steady dot.
            (None, Color::Gray, true),
        ];

        for (qualifier, color, active) in cases {
            // Sourced from `state`...
            let from_state = agent("background", qualifier, None);
            assert_eq!(
                badge_color(&from_state),
                color,
                "state={qualifier:?} should be {color:?}"
            );
            assert_eq!(
                agents::is_active(&from_state),
                active,
                "state={qualifier:?} should have active={active}"
            );

            // ...and identically via the `status` fallback (no `state` at all),
            // so the badge never depends on WHICH field the wire used.
            let from_status = agent("interactive", None, qualifier);
            assert_eq!(
                badge_color(&from_status),
                color,
                "status={qualifier:?} should be {color:?}"
            );
            assert_eq!(
                agents::is_active(&from_status),
                active,
                "status={qualifier:?} should have active={active}"
            );
        }

        // The joint-read bucket needs BOTH fields, so it sits outside the
        // single-qualifier loop: a working `state` its own `status` calls `idle`
        // is `WorkingButIdle`. It carries the working gray — but STEADY, so the
        // MISSING pulse, not a second color, is the whole tell.
        let interrupted = agent("background", Some("working"), Some("idle"));
        assert_eq!(
            badge_color(&interrupted),
            BADGE_WORKING,
            "the interrupted bucket shares the working gray base"
        );
        assert_eq!(BADGE_WORKING, Color::Gray, "and that base is gray");
        assert!(
            !agents::is_active(&interrupted),
            "it must be steady: the pulse would claim work claude's status denies"
        );
        // Both RESTING background buckets are in this test's coverage and their
        // SHADES are pinned apart above — the `stopped`/`failed` rows in the loop
        // demand `DarkGray`, this row demands the working `Gray` — so a collapse of
        // either onto the other's color fails here rather than silently erasing the
        // only thing that distinguishes two badges that both hold still.
    }

    /// `qualifier`'s state-then-status precedence must reach the badge too: the
    /// real shape for a waiting background agent is state `blocked` alongside
    /// status `idle`. Both buckets are steady, so only the COLOR proves which
    /// field won — and it must be `state` (yellow), not `status` (green).
    #[test]
    fn state_beats_status_for_the_badge_color() {
        let both = agent("background", Some("blocked"), Some("idle"));
        assert_eq!(badge_color(&both), Color::Yellow);
        assert!(!agents::is_active(&both));
    }

    /// The pulse's timing, over a FULL cycle: the dot is shown for `BLINK_TICKS`
    /// ticks, hidden for `BLINK_TICKS`, then shown again — 500ms on / 500ms off
    /// at the 250ms `watch::TICK` this counts. Pure, so the cadence is pinned
    /// without a terminal or a clock.
    #[test]
    fn blink_visible_alternates_phases_every_blink_ticks() {
        assert_eq!(
            BLINK_TICKS, 2,
            "the tick expectations below are written for a 2-tick phase; \
             retune them alongside BLINK_TICKS"
        );
        for tick in [0, 1] {
            assert!(
                blink_visible(tick),
                "tick {tick} is in the opening ON phase"
            );
        }
        for tick in [2, 3] {
            assert!(!blink_visible(tick), "tick {tick} is in the OFF phase");
        }
        for tick in [4, 5] {
            assert!(
                blink_visible(tick),
                "tick {tick} wraps into the next cycle's ON phase"
            );
        }
    }

    /// `App::tick` uses `wrapping_add`, so the counter rolls over instead of
    /// panicking. The phase must survive that: one cycle is `2 * BLINK_TICKS`
    /// ticks and `u64::MAX + 1` is a whole number of cycles, so the OFF phase
    /// ending at `u64::MAX` is followed by tick 0 opening an ON phase.
    #[test]
    fn blink_visible_phase_stays_aligned_across_the_tick_wrap() {
        assert!(
            blink_visible(u64::MAX - 2),
            "the last ON tick before the wrap"
        );
        assert!(!blink_visible(u64::MAX - 1));
        assert!(
            !blink_visible(u64::MAX),
            "the last tick before the wrap is OFF"
        );
        assert!(
            blink_visible(u64::MAX.wrapping_add(1)),
            "the wrap lands on tick 0, which must open a clean ON phase"
        );
    }

    /// One list row's drawn live badge, read back from the rendered buffer.
    ///
    /// Cells, not spans: this is what the terminal would actually paint, so a
    /// style that gets patched away at render time (e.g. by the List's
    /// `highlight_style`) cannot slip past these assertions.
    struct DrawnBadge {
        /// The row's full drawn text, so a test can tell WHICH session's badge
        /// this is without depending on row order or group-head placement.
        row: String,
        dot_fg: Color,
        /// The kind label drawn beside the dot (`bg`), as text.
        label: String,
        /// Per-cell `(fg, modifier)` of that label — kept per-cell so a badge
        /// styled unevenly across its own label cannot average out to a pass.
        label_cells: Vec<(Color, Modifier)>,
    }

    /// Scan a rendered list buffer for every DRAWN badge, locating each by its
    /// badge glyph — `●`, or `!` for a `NeedsInput` row (`badge_glyph` picks one
    /// per bucket) — rather than by a hardcoded column, so a layout tweak fails
    /// this test loudly instead of silently reading the wrong cells. The FIRST
    /// cell matching EITHER glyph is the badge: the timestamp and label to its
    /// left contain neither symbol, so the leftmost match is still the badge, and
    /// both glyphs are one cell wide so the `dot_x + 2` label offset is unchanged.
    ///
    /// Every REPORTED row is found in every pulse phase: the glyph is drawn
    /// unconditionally and the pulse only restyles it, so an absent badge means
    /// the row has no agent — never that it is mid-pulse. The phase is read off
    /// `dot_fg`, not off presence.
    fn drawn_badges(buffer: &ratatui::buffer::Buffer, width: u16, height: u16) -> Vec<DrawnBadge> {
        let mut badges = Vec::new();
        for y in 0..height {
            let Some(dot_x) = (0..width).find(|&x| {
                buffer.cell((x, y)).is_some_and(|cell| {
                    cell.symbol() == BADGE_DOT || cell.symbol() == BADGE_NEEDS_INPUT
                })
            }) else {
                continue; // Not a reported row.
            };
            let dot = buffer
                .cell((dot_x, y))
                .expect("the dot cell was just located in this buffer");

            // The kind label is the contiguous non-space run after the dot's
            // single separating space; the dim qualifier beyond it is separated
            // by a raw space, so the run stops exactly at the label's end.
            let mut label = String::new();
            let mut label_cells = Vec::new();
            for x in (dot_x + 2)..width {
                let Some(cell) = buffer.cell((x, y)) else {
                    break;
                };
                if cell.symbol() == " " {
                    break;
                }
                label.push_str(cell.symbol());
                label_cells.push((cell.fg, cell.modifier));
            }

            badges.push(DrawnBadge {
                row: row_text(buffer, y, width),
                dot_fg: dot.fg,
                label,
                label_cells,
            });
        }
        badges
    }

    /// One session per KNOWN shape: `(row label, state, status, badge color,
    /// does it pulse)`.
    ///
    /// Color and pulse are asserted as a PAIR because they are one signal: gray
    /// is only honest about a working agent while that agent's dot is pulsing.
    /// Every qualifier gets its own row rather than one row per bucket, so a
    /// bucket that silently stopped covering one of its two spellings fails here.
    ///
    /// The `label` is decoupled from `state` for exactly one row: `WorkingButIdle`
    /// shares the `working` STATE with the plain working bucket, so it needs its
    /// own `interrupted` label to stay a distinct, findable row while still
    /// carrying the `state`/`status` PAIR its classification is read from.
    ///
    /// The two STEADY non-green rows — `interrupted` (`WorkingButIdle`) and
    /// `stopped`/`failed` (`Ended`) — are both here on purpose: they rest alike but
    /// differ in SHADE, so rendering them side by side is what would catch either
    /// one being collapsed into the other's color.
    const BADGE_CASES: [(&str, &str, Option<&str>, Color, bool); 9] = [
        // Waiting on the user: the most prominent color, but STEADY.
        ("blocked", "blocked", None, Color::Yellow, false),
        // The same bucket under its other token: yellow, and steady TOO. This
        // row is the pulse lie being fixed — `waiting` once rendered as working.
        ("waiting", "waiting", None, Color::Yellow, false),
        // Up but not working: steady, and green is EARNED by this bucket
        // rather than being the badge's old hardcoded color.
        ("idle", "idle", None, Color::Green, false),
        // Quietly working: gray, and the pulse is what marks it active.
        ("working", "working", None, Color::Gray, true),
        // The same bucket under its other token: gray, and pulsing TOO.
        ("busy", "busy", None, Color::Gray, true),
        // The joint-read bucket: a working `state` its own `status` calls `idle`.
        // Same working gray, but STEADY — the missing pulse is the whole tell.
        ("interrupted", "working", Some("idle"), Color::Gray, false),
        // Finished: green (nothing is wanted from you) and steady. The poller
        // passes `--all` so EVERY `done` agent stays observable, reaped or not.
        ("done", "done", None, Color::Green, false),
        // The terminal bucket, under BOTH its tokens: dim gray and steady. Dim
        // rather than the working gray above (the job is over, not churning) and
        // not `done`'s green (it did not necessarily finish cleanly).
        ("stopped", "stopped", None, Color::DarkGray, false),
        ("failed", "failed", None, Color::DarkGray, false),
    ];

    /// A board carrying one REPORTED session per [`BADGE_CASES`] bucket, each
    /// labeled with its state so a drawn row is identifiable. Being badged says
    /// nothing about liveness — `done` is reported and badged all the same.
    ///
    /// Rendering all the buckets in a SINGLE `render_list` pass is the point — it
    /// proves each row derives its own badge from its own joined agent, which a
    /// per-row render could not.
    fn badge_board() -> App {
        let mut sessions = Vec::new();
        let mut reported = HashMap::new();
        for (label, state, status, _, _) in BADGE_CASES {
            let mut session = sample_session();
            session.session_id = format!("sess-{label}");
            session.label = format!("sess-{label}");
            reported.insert(
                session.session_id.clone(),
                ReportedAgent {
                    kind: "background".to_string(),
                    id: None,
                    state: Some(state.to_string()),
                    status: status.map(str::to_owned),
                    name: None,
                },
            );
            sessions.push(session);
        }
        let mut app = App::new(sessions, Scope::All, PathBuf::from("/tmp/launch"));
        app.set_reported_agents(reported);
        app
    }

    /// Wide enough for a whole badge row (badge + qualifier + label), tall
    /// enough for the group head plus every [`BADGE_CASES`] row (plus the
    /// block's two border rows) with slack — a row scrolled out of view would
    /// silently weaken every assertion below.
    ///
    /// Height is `BADGE_CASES.len()` + 1 group head + 2 borders + 2 slack rows, so
    /// it must GROW whenever a bucket row is added; the `badges.len() ==
    /// BADGE_CASES.len()` assertion in
    /// `render_list_colors_the_whole_badge_by_state` is what fails if it does not.
    const BADGE_BOARD_SIZE: (u16, u16) = (60, 14);

    /// Draw `app`'s list into an in-memory terminal and hand back the buffer —
    /// the cells a real terminal would paint.
    fn drawn_list(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height))
            .expect("build an in-memory test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_list(frame, app, area);
            })
            .expect("render_list must not panic");
        terminal.backend().buffer().clone()
    }

    /// The `x` at which `needle` starts on row `y`, matched cell by cell.
    ///
    /// Returns a COLUMN, not a byte offset, which is the unit the no-shift
    /// assertion needs: it is what the reader's eye tracks.
    fn column_of(buffer: &ratatui::buffer::Buffer, y: u16, width: u16, needle: &str) -> u16 {
        (0..width)
            .find(|&x| {
                needle.chars().enumerate().all(|(i, ch)| {
                    let cx = x + u16::try_from(i).expect("a needle shorter than a terminal row");
                    buffer
                        .cell((cx, y))
                        .is_some_and(|cell| cell.symbol() == ch.to_string())
                })
            })
            .unwrap_or_else(|| panic!("{needle:?} must be drawn on row {y}"))
    }

    /// The `y` of the single row whose drawn text contains `needle`.
    fn row_of(buffer: &ratatui::buffer::Buffer, width: u16, height: u16, needle: &str) -> u16 {
        let rows: Vec<u16> = (0..height)
            .filter(|&y| row_text(buffer, y, width).contains(needle))
            .collect();
        assert_eq!(
            rows.len(),
            1,
            "{needle:?} must identify exactly one drawn row, found {rows:?}"
        );
        rows[0]
    }

    /// Refinements 3 + 4: the WHOLE badge (dot + kind label) is colored by the
    /// agent's state, and every reported row — pulsing or not — shows its dot.
    /// The dot and label share that state color for every bucket EXCEPT
    /// `NeedsInput`, whose `!` reddens to the accent while its label stays yellow.
    #[test]
    fn render_list_colors_the_whole_badge_by_state() {
        let mut app = badge_board();
        // Pinned to the ON phase so every dot carries its BASE color: this test
        // is about the palette, and the phases themselves are pinned below.
        app.tick = 0;
        assert!(blink_visible(app.tick), "tick 0 must be the ON phase");

        let (width, height) = BADGE_BOARD_SIZE;
        let buffer = drawn_list(&mut app, width, height);

        let badges = drawn_badges(&buffer, width, height);
        assert_eq!(
            badges.len(),
            BADGE_CASES.len(),
            "each reported session must draw exactly one badge dot: {:?}",
            badges.iter().map(|b| &b.row).collect::<Vec<_>>()
        );

        for (label, _, _, color, _) in BADGE_CASES {
            let badge = badges
                .iter()
                .find(|badge| badge.row.contains(&format!("sess-{label}")))
                .unwrap_or_else(|| panic!("a badge row for the {label:?} session"));

            // Content first (structure, not styling): proves the cells read
            // below are really the kind label and not a drifted offset.
            assert_eq!(
                badge.label, "bg",
                "the dot must be followed by the kind label ({label:?} row: {:?})",
                badge.row
            );
            assert!(
                !badge.label_cells.is_empty(),
                "the {label:?} label must have drawn cells to assert over"
            );

            // The kind label always carries the bucket's badge color.
            for (fg, modifier) in &badge.label_cells {
                assert_eq!(*fg, color, "the {label:?} kind label must be {color:?}");
                // `contains`, not equality: the List's `highlight_style` layers
                // REVERSED (and its own BOLD) onto whichever row is selected, so
                // the selected badge's cells legitimately carry more than BOLD.
                assert!(
                    modifier.contains(Modifier::BOLD),
                    "the {label:?} kind label must survive to the buffer BOLD, got {modifier:?}"
                );
            }

            // The dot carries that SAME color, EXCEPT `NeedsInput`, whose `!`
            // diverges to the red accent while its label stays yellow — the
            // one-cell divergence that is the whole point of the red marker.
            let expected_dot_fg = if matches!(label, "blocked" | "waiting") {
                BADGE_NEEDS_INPUT_COLOR
            } else {
                color
            };
            assert_eq!(
                badge.dot_fg, expected_dot_fg,
                "the {label:?} dot must be {expected_dot_fg:?}, got {:?}",
                badge.dot_fg
            );
        }
    }

    /// The whole point of the change: the ONE bucket that wants the user reads
    /// LOUD on the list row. A `NeedsInput` row draws its translated `needs input`
    /// copy at the badge's own color + BOLD (matching the dot and kind label),
    /// while every OTHER bucket keeps its raw qualifier DIM — so the state that
    /// demands action is no longer the quietest text on the row.
    ///
    /// Both claims are read off the DRAWN cells (PATTERNS.md's "assert drawn
    /// cells" rule): a style the List patches away is one the user never sees.
    #[test]
    fn render_list_makes_the_needs_input_qualifier_prominent() {
        let mut app = badge_board();
        app.tick = 0; // Phase is irrelevant to a steady bucket; pin it anyway.

        let (width, height) = BADGE_BOARD_SIZE;
        let buffer = drawn_list(&mut app, width, height);

        // Locate a NeedsInput row by its UNIQUE session label — NEVER by the
        // phrase `needs input`, which two rows (`blocked` and `waiting`) now share,
        // so `row_of` would panic on the ambiguity.
        let needs_input = row_of(&buffer, width, height, "sess-blocked");
        let phrase = "needs input";
        let phrase_x = column_of(&buffer, needs_input, width, phrase);
        for (i, ch) in phrase.chars().enumerate() {
            let cell = buffer
                .cell((
                    phrase_x + u16::try_from(i).expect("a phrase shorter than a row"),
                    needs_input,
                ))
                .expect("a drawn `needs input` cell");
            assert_eq!(cell.symbol(), ch.to_string());
            assert_eq!(
                cell.fg,
                Color::Yellow,
                "the `needs input` phrase must carry the badge's own color, matching \
                 its dot and kind label ({:?})",
                cell.fg
            );
            // `contains`, since the List's `highlight_style` layers REVERSED|BOLD
            // onto the selected row and this row may be it.
            assert!(
                cell.modifier.contains(Modifier::BOLD),
                "the `needs input` phrase must draw at badge weight (BOLD), got {:?}",
                cell.modifier
            );
            assert!(
                !cell.modifier.contains(Modifier::DIM),
                "THE regression guard: `needs input` must NEVER be dim — that \
                 de-emphasis is exactly what made it the quietest text on the row"
            );
        }

        // The contrast: a NON-NeedsInput bucket keeps its raw qualifier DIM. Read
        // on `sess-idle`, which is NOT the default selection (that is the first
        // row, `sess-blocked`), so `highlight_style` cannot layer BOLD over the
        // DIM claim below.
        let idle = row_of(&buffer, width, height, "sess-idle");
        let idle_qualifier = "idle";
        let idle_x = column_of(&buffer, idle, width, idle_qualifier);
        for i in 0..idle_qualifier.chars().count() {
            let cell = buffer
                .cell((
                    idle_x + u16::try_from(i).expect("a qualifier shorter than a row"),
                    idle,
                ))
                .expect("a drawn `idle` qualifier cell");
            assert!(
                cell.modifier.contains(Modifier::DIM),
                "a non-NeedsInput qualifier stays DIM, exactly as before: {:?}",
                cell.modifier
            );
            assert!(
                !cell.modifier.contains(Modifier::BOLD),
                "and it must NOT draw at badge weight — only NeedsInput does"
            );
        }
    }

    /// The SHAPE channel: the ONE bucket that wants the user marks its badge with
    /// `!`, not the `●` every other bucket draws — a second signal layered on the
    /// yellow color, so a monochrome terminal or a color-blind reader still sees
    /// which row is asking. Read off the DRAWN cells (PATTERNS.md's rule).
    ///
    /// The glyph is chosen by BUCKET, never by pulse phase: `NeedsInput` is steady,
    /// so its badge cell — glyph AND style — is IDENTICAL in both phases. The pulse
    /// still only ever changes COLOR, and only for an ACTIVE bucket, so it can
    /// never touch this steady `!`.
    #[test]
    fn render_list_marks_needs_input_rows_with_a_bang() {
        let (width, height) = BADGE_BOARD_SIZE;

        // The badge cell (symbol + full style) drawn on the row labeled `label`,
        // located by the leftmost cell carrying EITHER badge glyph.
        let badge_cell = |tick: u64, label: &str| -> ratatui::buffer::Cell {
            let mut app = badge_board();
            app.tick = tick;
            let buffer = drawn_list(&mut app, width, height);
            let y = row_of(&buffer, width, height, label);
            let x = (0..width)
                .find(|&x| {
                    buffer.cell((x, y)).is_some_and(|cell| {
                        cell.symbol() == BADGE_NEEDS_INPUT || cell.symbol() == BADGE_DOT
                    })
                })
                .unwrap_or_else(|| panic!("a badge glyph must be drawn on the {label:?} row"));
            buffer
                .cell((x, y))
                .expect("the badge cell was just located in this buffer")
                .clone()
        };

        // Both NeedsInput spellings wear the `!`, Yellow + BOLD, and are STEADY:
        // the badge cell is byte-identical across the two pulse phases.
        for label in ["sess-blocked", "sess-waiting"] {
            let on = badge_cell(0, label);
            assert_eq!(
                on.symbol(),
                BADGE_NEEDS_INPUT,
                "the {label:?} NeedsInput row must mark its badge with `!`, the shape \
                 channel a monochrome or color-blind reader still sees"
            );
            assert_eq!(
                on.fg, BADGE_NEEDS_INPUT_COLOR,
                "the `!` wears the red accent — the label and qualifier stay yellow, \
                 so only this one glyph cell reddens ({:?})",
                on.fg
            );
            // `contains`, since the List's `highlight_style` layers REVERSED|BOLD
            // onto the selected row and this row may be it.
            assert!(
                on.modifier.contains(Modifier::BOLD),
                "the `!` draws at badge weight (BOLD), got {:?}",
                on.modifier
            );

            let off = badge_cell(BLINK_TICKS, label);
            assert_eq!(
                on, off,
                "NeedsInput is steady, so its `!` badge cell must be IDENTICAL in \
                 both pulse phases — the pulse changes color, never this glyph, and \
                 never a resting bucket at all"
            );
        }

        // Every OTHER bucket keeps the `●` dot — the shape channel is the one
        // bucket's alone, so nothing else changes (including the interrupted
        // bucket: it is not NeedsInput, so it must not borrow the `!`).
        for label in ["sess-idle", "sess-working", "sess-interrupted", "sess-done"] {
            assert_eq!(
                badge_cell(0, label).symbol(),
                BADGE_DOT,
                "the {label:?} row is not NeedsInput, so it must keep the `●` dot"
            );
        }
    }

    /// `badge_glyph` picks the shape channel by BUCKET: `!` for the ONE bucket that
    /// wants the user, `●` for every other. Derived from `classify`, so it covers
    /// both spellings of each two-token bucket and fails soft to `●` for an unknown
    /// or absent qualifier.
    #[test]
    fn badge_glyph_marks_only_needs_input_with_a_bang() {
        let glyph = |state: &str| badge_glyph(&agent("background", Some(state), None));

        // The ONE bucket that wants the user — both its spellings.
        assert_eq!(glyph("blocked"), BADGE_NEEDS_INPUT);
        assert_eq!(glyph("waiting"), BADGE_NEEDS_INPUT);

        // Every other KNOWN bucket keeps the dot.
        assert_eq!(glyph("idle"), BADGE_DOT);
        assert_eq!(glyph("working"), BADGE_DOT);
        assert_eq!(glyph("busy"), BADGE_DOT);
        assert_eq!(glyph("done"), BADGE_DOT);

        // FAIL-SOFT: an unknown qualifier is `Other`, which keeps the dot...
        assert_eq!(glyph("compacting"), BADGE_DOT);
        // ...and so does a record with no qualifier at all.
        assert_eq!(badge_glyph(&agent("background", None, None)), BADGE_DOT);
    }

    /// `badge_glyph_color` reddens ONLY the `NeedsInput` glyph; every other bucket
    /// keeps its `badge_color`. The label/qualifier color is `badge_color` in all
    /// cases (asserted where they are drawn), so this pins the single-cell accent
    /// at its source, over both spellings and the fail-soft buckets.
    #[test]
    fn badge_glyph_color_reddens_only_needs_input() {
        let color = |state: &str| badge_glyph_color(&agent("background", Some(state), None));

        // The ONE bucket that wants the user — both spellings — wears the accent.
        assert_eq!(color("blocked"), BADGE_NEEDS_INPUT_COLOR);
        assert_eq!(color("waiting"), BADGE_NEEDS_INPUT_COLOR);

        // Every other bucket's glyph keeps exactly its `badge_color`.
        for state in ["idle", "working", "busy", "done", "compacting"] {
            let a = agent("background", Some(state), None);
            assert_eq!(badge_glyph_color(&a), badge_color(&a), "state={state:?}");
        }
        // ...including a record with no qualifier at all (the `Other` bucket).
        let none = agent("background", None, None);
        assert_eq!(badge_glyph_color(&none), badge_color(&none));
    }

    /// THE core invariant: each row's badge glyph is drawn in BOTH pulse phases,
    /// for EVERY reported bucket — pulsing or steady. The pulse restyles the cell;
    /// it must never blank it. `drawn_badges` finds the badge by EITHER glyph
    /// (`●`, or `!` for the `NeedsInput` rows), so `blocked`/`waiting` are located
    /// by their `!` here — the glyph is chosen by bucket, but it stays constant
    /// across phases WITHIN a row all the same.
    ///
    /// This is the bug being fixed, pinned at its narrowest. Swapping the glyph
    /// for a blank mutated the row's text, which forced the terminal to re-detect
    /// the plain-text URL in the label beside it and flicker its underline every
    /// 500ms (see `pulse_color`). A phase-constant glyph is what makes that
    /// unrepresentable.
    #[test]
    fn render_list_draws_every_badge_dot_in_both_pulse_phases() {
        let (width, height) = BADGE_BOARD_SIZE;

        for (tick, on) in [(0, true), (BLINK_TICKS, false)] {
            let mut app = badge_board();
            app.tick = tick;
            assert_eq!(
                blink_visible(app.tick),
                on,
                "tick {tick} must be the {} phase",
                if on { "ON" } else { "OFF" }
            );

            let buffer = drawn_list(&mut app, width, height);
            let drawn: Vec<String> = drawn_badges(&buffer, width, height)
                .into_iter()
                .map(|badge| badge.row)
                .collect();

            for (label, _, _, _, _) in BADGE_CASES {
                let row = format!("sess-{label}");
                assert!(
                    drawn.iter().any(|drawn_row| drawn_row.contains(&row)),
                    "at tick {tick} (the {} phase) the {label:?} dot must STILL be drawn — \
                     the pulse changes color, never the glyph; drawn rows: {drawn:?}",
                    if on { "ON" } else { "OFF" }
                );
            }
        }
    }

    /// The other half of the invariant: what the pulse DOES change is the dot's
    /// color, and only for an ACTIVE bucket. A steady bucket's dot must be
    /// identical in both phases.
    ///
    /// The base color is asserted against `BADGE_CASES` (the palette's own
    /// contract) and the off-phase color against `pulse_color` — that split is
    /// deliberate: this test pins the WIRING (the renderer dims via `pulse_color`,
    /// off the same base), while `pulse_color`'s literal values are pinned by its
    /// own unit test. `assert_ne` is the user-visible claim underneath both: the
    /// dot must actually change.
    #[test]
    fn render_list_pulses_only_an_active_dots_color() {
        let (width, height) = BADGE_BOARD_SIZE;

        // (row label -> the dot's fg) at one tick.
        let phase = |tick: u64| -> HashMap<String, Color> {
            let mut app = badge_board();
            app.tick = tick;
            let buffer = drawn_list(&mut app, width, height);
            drawn_badges(&buffer, width, height)
                .into_iter()
                .filter_map(|badge| {
                    BADGE_CASES.iter().find_map(|(label, _, _, _, _)| {
                        badge
                            .row
                            .contains(&format!("sess-{label}"))
                            .then(|| ((*label).to_string(), badge.dot_fg))
                    })
                })
                .collect()
        };

        let on = phase(0);
        let off = phase(BLINK_TICKS);

        for (label, _, _, color, pulses) in BADGE_CASES {
            let on_fg = on[label];
            let off_fg = off[label];

            // The dot's ON-phase base is `badge_color`, EXCEPT `NeedsInput`, whose
            // `!` reddens to the accent. Only the pulsing buckets (never
            // `NeedsInput`) then dim off this base.
            let glyph_base = if matches!(label, "blocked" | "waiting") {
                BADGE_NEEDS_INPUT_COLOR
            } else {
                color
            };
            assert_eq!(
                on_fg, glyph_base,
                "the {label:?} dot must carry its base glyph color in the ON phase"
            );

            if pulses {
                assert_ne!(
                    on_fg, off_fg,
                    "the {label:?} dot is ACTIVE, so its color MUST change between \
                     phases — that color change IS the pulse"
                );
                assert_eq!(
                    off_fg,
                    pulse_color(color),
                    "the {label:?} dot's OFF phase must be its declared dim partner"
                );
            } else {
                assert_eq!(
                    on_fg, off_fg,
                    "the {label:?} bucket is at rest, so its dot must be steady: \
                     a pulse here would claim work is in flight"
                );
            }
        }
    }

    /// The label's half of the claim, and the one the dot tests above cannot
    /// make: the pulse restyles the DOT ONLY. The kind label beside it keeps its
    /// steady `badge_color` in BOTH phases.
    ///
    /// Asserted in the OFF phase ON PURPOSE, on a PULSING row. In the ON phase
    /// the steady style and the pulsing one are the SAME color, so a label
    /// wrongly wired to the dot's style is INDISTINGUISHABLE there and sails
    /// through; a resting row's dot never dims, so it cannot separate them
    /// either. The OFF phase of a pulsing row is the only frame where the two
    /// differ — so this asserts, in that ONE frame, that they have DIVERGED:
    /// color unifies the badge (the label still carries the dot's BASE), the
    /// pulse does not (the dot has dimmed away from it).
    ///
    /// This is what keeps the pulse a DOT pulse. A blinking text label would be
    /// noise on a board of live sessions, and `render_list`'s two spans exist
    /// solely to make that split expressible.
    #[test]
    fn render_list_never_pulses_the_kind_label() {
        let (width, height) = BADGE_BOARD_SIZE;
        let mut app = badge_board();
        app.tick = BLINK_TICKS;
        assert!(
            !blink_visible(app.tick),
            "tick {BLINK_TICKS} must be the OFF phase — the only phase in which a \
             steady label and a pulsing one differ at all"
        );

        let buffer = drawn_list(&mut app, width, height);
        let badges = drawn_badges(&buffer, width, height);

        let mut pulsing = 0;
        for (label, _, _, color, pulses) in BADGE_CASES {
            let badge = badges
                .iter()
                .find(|badge| badge.row.contains(&format!("sess-{label}")))
                .unwrap_or_else(|| panic!("a badge row for the {label:?} session"));

            // Content first (structure, not styling): proves the cells read
            // below are really the kind label and not a drifted offset.
            assert_eq!(
                badge.label, "bg",
                "the dot must be followed by the kind label ({label:?} row: {:?})",
                badge.row
            );
            assert!(
                !badge.label_cells.is_empty(),
                "the {label:?} label must have drawn cells to assert over"
            );

            for (fg, _) in &badge.label_cells {
                assert_eq!(
                    *fg, color,
                    "the {label:?} kind label must still be its steady {color:?} in the \
                     OFF phase — the label NEVER pulses, only the dot does"
                );
            }

            if !pulses {
                continue;
            }
            pulsing += 1;

            assert_eq!(
                badge.dot_fg,
                pulse_color(color),
                "the {label:?} dot must have dimmed in the OFF phase, or this row \
                 cannot show the divergence below"
            );
            // The divergence, in ONE frame: the dot has left the base color its
            // label still holds. This IS the requirement — a label that tracked
            // the dot's style would match here instead.
            for (fg, _) in &badge.label_cells {
                assert_ne!(
                    badge.dot_fg, *fg,
                    "the {label:?} row is ACTIVE and in the OFF phase, so its dot must \
                     have pulsed AWAY from its label's steady color: the two diverge \
                     here, and a label wired to the dot's pulsing style would not"
                );
            }
        }

        assert!(
            pulsing > 0,
            "at least one BADGE_CASES bucket must pulse, or the divergence is never \
             asserted and this test passes vacuously"
        );
    }

    /// `pulse_color`'s literal palette, pinned in one place so the render tests
    /// above can assert the WIRING without also re-encoding the values.
    ///
    /// NAMED ANSI on both sides (TERMINAL-SAFE STYLING): `DarkGray` is the dim
    /// gray, so the OFF phase reads as the same badge at lower intensity. Not
    /// `Modifier::DIM` — an attribute most terminals honor inconsistently, which
    /// is the exact trap that made the ANSI blink attribute ship inert.
    #[test]
    fn pulse_color_dims_the_working_base_and_passes_anything_else_through() {
        assert_eq!(pulse_color(BADGE_WORKING), BADGE_WORKING_DIM);
        assert_eq!(BADGE_WORKING, Color::Gray, "the pulsing bucket's base");
        assert_eq!(BADGE_WORKING_DIM, Color::DarkGray, "its dim partner");
        // FAIL-SOFT identity for a base with no declared partner. Harmless for a
        // RESTING bucket (it never dims), and pinned shut for a pulsing one by
        // `every_pulsing_buckets_badge_color_has_a_distinct_dim_partner`.
        assert_eq!(pulse_color(Color::Yellow), Color::Yellow);
        assert_eq!(pulse_color(Color::Green), Color::Green);
    }

    /// A synthetic agent that classifies into `bucket`.
    ///
    /// Returns the whole [`ReportedAgent`] rather than a lone qualifier because
    /// [`AgentActivity::WorkingButIdle`] is read from the raw `state`/`status`
    /// PAIR (a working `state` contradicted by an `idle` `status`), which a
    /// single qualifier token cannot express.
    ///
    /// EXHAUSTIVE on purpose: adding an `AgentActivity` bucket fails to compile
    /// here, which drags the author to the walk below — the one thing that keeps
    /// `pulse_color`'s silent identity fallback from swallowing a new pulsing
    /// bucket. (The walk's own list must then gain the bucket; a `match` cannot
    /// force that, so `ALL_BUCKETS` says so.)
    fn agent_reaching(bucket: AgentActivity) -> ReportedAgent {
        match bucket {
            AgentActivity::NeedsInput => agent("background", Some("blocked"), None),
            AgentActivity::Idle => agent("background", Some("idle"), None),
            AgentActivity::Working => agent("background", Some("working"), None),
            // The one joint-read bucket: a working `state` AND an idle `status`.
            AgentActivity::WorkingButIdle => agent("background", Some("working"), Some("idle")),
            AgentActivity::Done => agent("background", Some("done"), None),
            // Terminal (stopped/failed): resting, so the walk below skips it.
            AgentActivity::Ended => agent("background", Some("stopped"), None),
            // The fail-soft bucket: an unrecognized qualifier, or none at all.
            AgentActivity::Other => agent("background", Some("compacting"), None),
        }
    }

    /// Every `AgentActivity` bucket. Keep in sync with the enum — the exhaustive
    /// `match` in [`agent_reaching`] is what fails to compile and sends the
    /// author here when a bucket is added.
    const ALL_BUCKETS: [AgentActivity; 7] = [
        AgentActivity::NeedsInput,
        AgentActivity::Idle,
        AgentActivity::Working,
        AgentActivity::WorkingButIdle,
        AgentActivity::Done,
        AgentActivity::Ended,
        AgentActivity::Other,
    ];

    /// The trap in `pulse_color`'s identity fallback, pinned shut.
    ///
    /// That fallback is the right FAIL-SOFT default — an undeclared base renders
    /// steady rather than panicking. But it means a future PULSING bucket whose
    /// base has no declared dim partner would SILENTLY stop pulsing: green tests,
    /// dead feature, which is exactly how this feature shipped broken twice. So
    /// walk EVERY bucket and demand that every one `is_active` says pulses has a
    /// partner that actually differs from its base.
    ///
    /// It passes trivially today (one pulsing base, one arm). It exists for the
    /// day someone adds a second.
    #[test]
    fn every_pulsing_buckets_badge_color_has_a_distinct_dim_partner() {
        for bucket in ALL_BUCKETS {
            let agent = agent_reaching(bucket);
            assert_eq!(
                agents::classify(&agent),
                bucket,
                "agent_reaching({bucket:?}) must actually classify into that bucket, \
                 or this walk silently stops covering it"
            );

            if !agents::is_active(&agent) {
                continue; // A resting bucket never dims, so it needs no partner.
            }
            let base = badge_color(&agent);
            assert_ne!(
                pulse_color(base),
                base,
                "the {bucket:?} bucket PULSES, so its base {base:?} needs a dim partner \
                 declared in pulse_color — without one it falls through the identity \
                 fallback and renders steady, and nothing else would tell you"
            );
        }
    }

    // --- search cursor pulse ----------------------------------------------

    /// The query echoed by the search-cursor tests. Non-empty and free of
    /// spaces, so [`column_of`] can locate it as one contiguous run.
    const CURSOR_QUERY: &str = "needle";
    /// Wide enough for `search: ` + [`CURSOR_QUERY`] + the cursor, with slack.
    const SEARCH_LINE_WIDTH: u16 = 40;

    /// Draw `app`'s search line into an in-memory terminal and hand back the
    /// buffer — the cells a real terminal would paint.
    fn drawn_search(app: &App, width: u16) -> ratatui::buffer::Buffer {
        let mut terminal =
            Terminal::new(TestBackend::new(width, 1)).expect("build an in-memory test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_search(frame, app, area);
            })
            .expect("render_search must not panic");
        terminal.backend().buffer().clone()
    }

    /// A whole drawn row, borders included — the diagnostic counterpart to
    /// [`row_text`], which strips column 0 because the LIST it was written for
    /// sits inside a `Block`. The search line has no border, so reusing
    /// `row_text` here would silently eat the leading `s` of `search: `.
    fn full_row_text(buffer: &ratatui::buffer::Buffer, y: u16, width: u16) -> String {
        (0..width)
            .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol().to_string()))
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    /// A board whose search line echoes [`CURSOR_QUERY`] at `tick`.
    fn cursor_board(tick: u64) -> App {
        let mut app = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp/launch"));
        app.query = CURSOR_QUERY.to_string();
        app.tick = tick;
        app
    }

    /// The column the cursor occupies: immediately after the DRAWN query.
    ///
    /// Derived from where the query actually landed rather than hardcoded, so a
    /// change to the `search: ` prefix fails these tests loudly instead of
    /// silently reading the wrong cell.
    fn cursor_column(buffer: &ratatui::buffer::Buffer, width: u16) -> u16 {
        let query_x = column_of(buffer, 0, width, CURSOR_QUERY);
        query_x + u16::try_from(CURSOR_QUERY.chars().count()).expect("a query shorter than a row")
    }

    /// The search cursor is drawn in the pulse's VISIBLE phase — asserted as the
    /// GLYPH in the buffer, never as a style modifier: this cursor shipped
    /// carrying the ANSI blink attribute and never blinked once, which a
    /// modifier assertion would have called green.
    #[test]
    fn render_search_draws_the_cursor_in_the_pulses_visible_phase() {
        let app = cursor_board(0);
        assert!(blink_visible(app.tick), "tick 0 must be a visible phase");

        let buffer = drawn_search(&app, SEARCH_LINE_WIDTH);
        let x = cursor_column(&buffer, SEARCH_LINE_WIDTH);

        assert_eq!(
            buffer
                .cell((x, 0))
                .expect("the cursor column must be inside the drawn line")
                .symbol(),
            SEARCH_CURSOR,
            "the cursor glyph must be drawn right after the query in the visible phase; \
             drawn line: {:?}",
            full_row_text(&buffer, 0, SEARCH_LINE_WIDTH)
        );
    }

    /// The other half of the pulse: at `BLINK_TICKS` the glyph is GONE from its
    /// column, replaced by a same-width blank. Without this the "pulse" is a
    /// permanently-lit cursor.
    #[test]
    fn render_search_hides_the_cursor_in_the_pulses_hidden_phase() {
        let app = cursor_board(BLINK_TICKS);
        assert!(
            !blink_visible(app.tick),
            "tick {BLINK_TICKS} must be a hidden phase"
        );

        let buffer = drawn_search(&app, SEARCH_LINE_WIDTH);
        let x = cursor_column(&buffer, SEARCH_LINE_WIDTH);

        assert_eq!(
            buffer
                .cell((x, 0))
                .expect("the cursor column must be inside the drawn line")
                .symbol(),
            SEARCH_CURSOR_HIDDEN,
            "the cursor's column must be BLANK in the hidden phase; drawn line: {:?}",
            full_row_text(&buffer, 0, SEARCH_LINE_WIDTH)
        );
    }

    /// The anti-shift pin: the query must not move as the cursor pulses.
    ///
    /// Note the cursor is currently the LAST span on the line, so blanking the
    /// hidden phase and dropping the span paint identical cells — this test
    /// cannot tell those apart, and does not claim to. What it does pin is that
    /// nothing the pulse touches ever reflows the query beside it, which is what
    /// would read as a broken board rather than a pulse.
    #[test]
    fn render_search_keeps_the_query_column_stable_across_both_pulse_phases() {
        let columns: Vec<u16> = [0, BLINK_TICKS]
            .into_iter()
            .map(|tick| {
                let buffer = drawn_search(&cursor_board(tick), SEARCH_LINE_WIDTH);
                column_of(&buffer, 0, SEARCH_LINE_WIDTH, CURSOR_QUERY)
            })
            .collect();

        assert_eq!(
            columns[0], columns[1],
            "the query must start at the SAME column in the visible (tick 0, col {}) \
             and hidden (tick {BLINK_TICKS}, col {}) phases",
            columns[0], columns[1]
        );
    }

    /// Wide/tall enough for a whole board: header + list rows + search + help.
    ///
    /// Its users draw a [`badge_board`], so the height must clear every
    /// [`BADGE_CASES`] row on top of the header/search/help chrome and the list
    /// block's borders + group head — a row scrolled out of view fails their
    /// `row_of` lookup rather than passing vacuously, so this grows with the table.
    const FULL_BOARD_SIZE: (u16, u16) = (80, 17);

    /// Draw the WHOLE board and hand back the buffer.
    fn drawn_board(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height))
            .expect("build an in-memory test terminal");
        terminal
            .draw(|frame| render(frame, app))
            .expect("render must not panic");
        terminal.backend().buffer().clone()
    }

    /// Whether row `y` of `buffer` has `glyph` drawn anywhere on it.
    fn row_has_glyph(buffer: &ratatui::buffer::Buffer, y: u16, width: u16, glyph: &str) -> bool {
        (0..width).any(|x| {
            buffer
                .cell((x, y))
                .is_some_and(|cell| cell.symbol() == glyph)
        })
    }

    /// The fg of the badge dot drawn on row `y` — the leftmost `●`, which is the
    /// badge's (the list is the left pane).
    fn dot_fg_on_row(buffer: &ratatui::buffer::Buffer, y: u16, width: u16) -> Color {
        let x = (0..width)
            .find(|&x| {
                buffer
                    .cell((x, y))
                    .is_some_and(|cell| cell.symbol() == BADGE_DOT)
            })
            .unwrap_or_else(|| panic!("a badge dot must be drawn on row {y} in EVERY phase"));
        buffer
            .cell((x, y))
            .expect("the dot cell was just located in this buffer")
            .fg
    }

    /// There is exactly ONE phase source on the board: the search cursor and an
    /// active badge's dot both read `blink_visible(App::tick)`, so they pulse
    /// TOGETHER rather than drifting against each other.
    ///
    /// Reading both out of a SINGLE rendered frame is the point — two separate
    /// renders could not prove they agree within one paint.
    ///
    /// The two are read through DIFFERENT properties because they pulse
    /// differently BY DESIGN, and that asymmetry is the fix, not an oversight:
    /// the cursor shows/hides (nothing on the search line is auto-detected by the
    /// terminal, so mutating that line's text is free), while the dot must hold
    /// its glyph and swap COLOR instead, or the URL sharing its row flickers (see
    /// `pulse_color`). So "in phase" here reads: the cursor is drawn exactly when
    /// the dot carries its BASE color, and blanked exactly when the dot carries
    /// its dim partner.
    #[test]
    fn the_search_cursor_and_an_active_badge_dot_pulse_in_phase() {
        let (width, height) = FULL_BOARD_SIZE;

        for (tick, on) in [(0, true), (BLINK_TICKS, false)] {
            let mut app = badge_board();
            app.tick = tick;

            let buffer = drawn_board(&mut app, width, height);
            // `render`'s layout is header(1) | body(fill) | search(1) | help(1).
            let search_y = height - 2;
            let cursor_drawn = row_has_glyph(&buffer, search_y, width, SEARCH_CURSOR);
            // `working` is an ACTIVE bucket, so its dot is one whose phase should
            // track the cursor's.
            let working_y = row_of(&buffer, width, height, "sess-working");
            let dot_fg = dot_fg_on_row(&buffer, working_y, width);

            assert_eq!(
                cursor_drawn,
                on,
                "at tick {tick} the search cursor must{} be drawn; line: {:?}",
                if on { "" } else { " NOT" },
                full_row_text(&buffer, search_y, width)
            );
            let dot_on = dot_fg == badge_color(&agent("background", Some("working"), None));
            assert_eq!(
                dot_on,
                on,
                "at tick {tick} the active dot must carry its {} color; row: {:?}",
                if on { "BASE" } else { "dim partner's" },
                row_text(&buffer, working_y, width)
            );
            assert_eq!(
                cursor_drawn, dot_on,
                "the cursor and the active dot must share one phase at tick {tick}: \
                 both are driven by blink_visible(App::tick), so the cursor is drawn \
                 exactly when the dot is at full color"
            );
        }
    }

    /// Task 4.2: while a `Ctrl-X` leader chord is pending, the which-key hint takes
    /// over the help line so the follow-up keys are discoverable — asserted on the
    /// DRAWN cells, not on the source string.
    #[test]
    fn a_pending_chord_takes_over_the_help_line_with_the_which_key_hint() {
        let (width, height) = FULL_BOARD_SIZE;
        let mut app = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp/launch"));
        app.pending_chord = true;

        let buffer = drawn_board(&mut app, width, height);
        // `render`'s layout is header(1) | body(fill) | search(1) | help(1), so the
        // help line — where the hint takes over — is the LAST row.
        let help_y = height - 1;
        let text = full_row_text(&buffer, help_y, width);

        assert!(
            text.contains(&chord_hint(false)),
            "a pending chord must draw the which-key hint on the help line; drawn: {text:?}"
        );
        // `d` names BOTH targets the confirm offers, so the chord hint stays in
        // step with the delete modal's own choices (AGENTS.md KEEP KEY DOCS IN SYNC).
        for needle in [
            "x hide",
            "d delete row/lineage",
            "h show/hide hidden",
            "r reload",
            "Esc cancel",
        ] {
            assert!(
                text.contains(needle),
                "the which-key hint must list {needle:?}; drawn: {text:?}"
            );
        }
    }

    /// The hint's own column budget: its LONGEST form must still fit an
    /// 80-column terminal, since the help row is truncated rather than wrapped
    /// and the tail carries `Esc cancel`.
    #[test]
    fn the_chord_hint_fits_an_eighty_column_terminal() {
        let widest = chord_hint(true);
        assert!(
            widest.chars().count() <= 80,
            "the which-key hint must not overflow an 80-column help row: {} cols in {widest:?}",
            widest.chars().count()
        );
    }

    #[test]
    fn chord_hint_flips_the_x_verb_with_the_selected_rows_hidden_state() {
        // A visible row hides; a hidden row exposes (there `x` un-hides it).
        assert!(chord_hint(false).contains("x hide"));
        assert!(!chord_hint(false).contains("expose"));
        assert!(chord_hint(true).contains("x expose"));
        assert!(!chord_hint(true).contains("x hide"));
    }

    #[test]
    fn wrap_message_splits_a_long_prompt_and_keeps_a_short_one_whole() {
        // A short prompt stays on one line.
        assert_eq!(
            wrap_message("Delete this?", 60),
            vec!["Delete this?".to_string()]
        );
        // The delete confirmation is wider than the box, so it wraps; every wrapped
        // line fits the width and no word is dropped or reordered.
        let msg = "Permanently delete this session's transcript from disk? \
                   This can't be undone.";
        let lines = wrap_message(msg, 60);
        assert!(lines.len() >= 2, "a long prompt must wrap: {lines:?}");
        assert!(lines.iter().all(|l| l.chars().count() <= 60));
        assert_eq!(
            lines.join(" "),
            msg.split_whitespace().collect::<Vec<_>>().join(" "),
            "wrapping preserves every word in order"
        );
        // A degenerate zero width never panics and still yields one line.
        assert_eq!(wrap_message("", 0).len(), 1);
    }

    // --- new-session agent picker overlay ---------------------------------

    use crate::defined_agents::DefinedAgent;

    #[test]
    fn modal_list_row_marks_the_selected_row_and_trails_the_description() {
        // A selected row leads with the highlight marker and reverses the label.
        let sel = modal_list_row("planner", Some("plans work"), true);
        let text: String = sel.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.starts_with("› "),
            "a selected row leads with the highlight marker: {text:?}"
        );
        assert!(text.contains("planner") && text.contains("plans work"));
        let name = sel
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "planner")
            .expect("the label span is present");
        assert!(
            name.style.add_modifier.contains(Modifier::REVERSED),
            "the selected label is reversed"
        );

        // An unselected, description-less row is padded and not reversed.
        let unsel = modal_list_row("planner", None, false);
        let text: String = unsel.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(
            text, "  planner",
            "unselected row is padded, no description"
        );
        let name = unsel
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "planner")
            .expect("the label span is present");
        assert!(
            !name.style.add_modifier.contains(Modifier::REVERSED),
            "an unselected label is not reversed"
        );
    }

    #[test]
    fn render_draws_the_agent_picker_overlay_without_panicking() {
        // Full-frame render with the picker open must lay the overlay out (height
        // math + centering) and draw over the board without panicking.
        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        app.open_agent_picker(vec![
            DefinedAgent {
                name: "planner".to_string(),
                description: Some("plans work".to_string()),
            },
            DefinedAgent {
                name: "reviewer".to_string(),
                description: None,
            },
        ]);
        let mut terminal =
            Terminal::new(TestBackend::new(80, 24)).expect("build an in-memory test terminal");
        terminal
            .draw(|frame| render(frame, &mut app))
            .expect("render must not panic with the agent picker open");
        assert!(
            app.modal.is_some(),
            "rendering must not disturb the open picker"
        );
        // The picker has TWO verbs, so its footer must advertise both — a key the
        // user cannot discover may as well not be bound (KEEP KEY DOCS IN SYNC).
        let drawn = (0..24)
            .map(|y| full_row_text(terminal.backend().buffer(), y, 80))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            drawn.contains("Enter draft") && drawn.contains("^O interactive"),
            "the picker footer must name both Enter and Ctrl-O:\n{drawn}"
        );
    }

    #[test]
    fn render_draws_the_running_session_choice_overlay_without_panicking() {
        // Full-frame render with the Row-layout running-session overlay open must
        // lay out and draw the button strip over the board without panicking.
        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        app.open_live_choice("sess-live".to_string());
        let mut terminal =
            Terminal::new(TestBackend::new(80, 24)).expect("build an in-memory test terminal");
        terminal
            .draw(|frame| render(frame, &mut app))
            .expect("render must not panic with the running-session overlay open");
        assert!(
            app.modal.is_some(),
            "rendering must not disturb the open overlay"
        );
    }

    #[test]
    fn the_delete_confirm_modal_shows_its_full_message_without_clipping() {
        // The delete prompt is far wider than the modal, so it must wrap across
        // rows — the opening clause, the irreversibility warning AND the blast
        // radius all have to reach the screen. Regression guard for the clipped
        // "... This" the old single-line render produced.
        //
        // Opened through the REAL `open_delete_confirm` rather than a synthetic
        // Modal, so the copy under test is the copy that ships: the blast-radius
        // sentence is the honest half (the guard now admits parked background
        // agents, so the user must be told the agent itself survives and can write
        // a fresh transcript), and a hand-copied fixture would keep passing while
        // that sentence changed or vanished.
        let (width, height) = (80u16, 24u16);
        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        app.open_delete_confirm();
        assert!(app.modal.is_some(), "the confirm is open");

        let buffer = drawn_board(&mut app, width, height);
        let screen: String = (0..height)
            .map(|y| full_row_text(&buffer, y, width))
            .collect::<Vec<_>>()
            .join("\n");
        // Read only the modal's OWN columns — the same centered span
        // `centered_rect` places it in, minus its borders. Flattening the whole
        // row would splice the panes' borders and the preview's text into the
        // middle of a wrapped sentence, which says nothing about clipping.
        let inner_x = usize::from((width - MODAL_WIDTH) / 2 + 1);
        let inner_w = usize::from(MODAL_WIDTH - 2);
        // Wrapping breaks lines at spaces, so collapsing runs of whitespace lets a
        // phrase split across two rows read back as the phrase — while text
        // genuinely CLIPPED away is still missing.
        let flat = (0..height)
            .map(|y| {
                full_row_text(&buffer, y, width)
                    .chars()
                    .skip(inner_x)
                    .take(inner_w)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for needle in [
            "Permanently delete this transcript from disk?",
            "This can't be undone.",
            "a background agent keeps running in Claude Code until you stop it there",
            "can write a new transcript.",
            // The button strip is part of the same wrapped box.
            "Delete this",
            "Cancel",
        ] {
            assert!(
                flat.contains(needle),
                "the wrapped delete confirm must show {needle:?} in full; screen:\n{screen}"
            );
        }
    }

    /// A `members`-strong fork lineage in one folder with `hidden` of it soft-HIDDEN
    /// — the DISCLOSING shape of the delete confirm, which the test above does not
    /// cover (it opens a LONE session, where `delete_confirm_message` adds nothing).
    ///
    /// Built from [`sample_session`] so the confirm renders over a real board, and
    /// through `hidden_ids` + `root_uuid` rather than a synthetic `Modal` so the
    /// modal under test is the one `open_delete_confirm` actually builds. The
    /// selected id is the NEWEST member, which is the lineage head.
    ///
    /// The PARTIAL shape — older members hidden, the head not — is NOT reachable by
    /// hiding: `App::toggle_hidden_selected` flips a whole lineage as ONE unit. It is
    /// reachable the way a user meets it, a set PERSISTED while the lineage was
    /// smaller plus a later fork joining as the new head, so the set is seeded and the
    /// board is then rebuilt through `apply_sessions` — the PUBLIC reload path that
    /// such a fork actually arrives on. The rebuild is the point: a board left as
    /// `App::new` filtered it would still count the hidden members into the head's
    /// `(+N)` marker, a board the running app cannot draw.
    fn hidden_lineage_board(members: usize, hidden: usize) -> App {
        let ids: Vec<String> = (0..members).map(|i| format!("disc-{i:02}")).collect();
        let sessions: Vec<Session> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let mut s = sample_session();
                s.session_id = id.clone();
                s.label = id.clone();
                s.root_uuid = Some("disc-root".to_string());
                // Distinct stamps so the newest — `disc-00` — is the head
                // deterministically.
                s.timestamp = Some(
                    OffsetDateTime::from_unix_timestamp(
                        1_752_000_000 - i64::try_from(i).expect("a small fixture index") * 3_600,
                    )
                    .expect("a valid fixture timestamp"),
                );
                s
            })
            .collect();
        let mut app = App::new(sessions.clone(), Scope::All, PathBuf::from("/tmp/launch"));
        // `App::new` LOADS the persisted hidden set, so start from a known one —
        // the counts below are exact, and an inherited id must not reach them.
        app.hidden_ids.clear();
        // Hide from the OLD end, so the selected head stays visible: the surprising
        // shape is a board showing one row while the button counts several.
        app.hidden_ids
            .extend(ids.iter().rev().take(hidden).cloned());
        // Re-derive `filtered`, the fold counts and the selection from that set. The
        // head survives the reload by id, so the selection lands where a startup's
        // `select_first` would leave it.
        app.apply_sessions(sessions);
        app
    }

    /// The disclosure is not free, and this pins the price the doc block on
    /// `app::delete_confirm_message` quotes.
    ///
    /// The added sentence costs exactly ONE wrapped row (4 → 5), so the `Row` box
    /// grows 10 → 11. `centered_rect` CLAMPS that height and `render_modal` draws
    /// top-down with no vertical scroll, so the extra row pushes the button strip
    /// and the `Esc cancel` footer off a short terminal one row sooner than the
    /// non-disclosing confirm did: the strip needs 9 rows where it needed 8, the
    /// footer 11 where it needed 10.
    ///
    /// Both numbers and their non-disclosing baselines are asserted, because the
    /// point is the DELTA — a test that only pinned the disclosing side would keep
    /// passing if the plain prompt regressed to match it. Nothing here is estimated:
    /// this test IS the measurement the doc block cites, kept so the doc cannot
    /// drift away from the render.
    #[test]
    fn the_disclosing_delete_confirm_costs_one_row_and_keeps_cancel_default() {
        /// Terminal heights the sweep covers — well below and well above every
        /// threshold under test, so a moved threshold shows up as a changed answer
        /// rather than as a sweep that ran out of room.
        const SWEEP: std::ops::RangeInclusive<u16> = 4..=16;

        // The shortest terminal that still draws `needle` somewhere on screen.
        let shortest_showing = |app: &mut App, needle: &str| -> Option<u16> {
            SWEEP.clone().find(|&h| {
                let buffer = drawn_board(app, 80, h);
                let screen: String = (0..h)
                    .map(|y| full_row_text(&buffer, y, 80))
                    .collect::<Vec<_>>()
                    .join("\n");
                screen.contains(needle)
            })
        };

        // --- the disclosing confirm -------------------------------------
        let mut app = hidden_lineage_board(3, 2);
        app.open_delete_confirm();
        let modal = app.modal.clone().expect("the confirm is open");
        assert!(
            modal
                .message
                .starts_with("3 in this lineage, 2 of them hidden."),
            "the fixture must be on the DISCLOSING path: {:?}",
            modal.message
        );
        assert_eq!(
            wrap_message(&modal.message, MODAL_WIDTH - 2).len(),
            5,
            "the disclosure costs exactly one wrapped row over the plain prompt's four"
        );
        assert_eq!(
            shortest_showing(&mut app, "Delete lineage (3)"),
            Some(9),
            "a disclosing confirm needs a 9-row terminal to draw its button strip"
        );
        assert_eq!(
            shortest_showing(&mut app, "Esc cancel"),
            Some(11),
            "a disclosing confirm needs an 11-row terminal to draw its footer"
        );
        // The residual cost is legibility, never the safe default: `Cancel` is
        // preselected, so Esc/Enter still cancel with the strip off screen.
        assert_eq!(
            modal.choices[modal.selected].label, "Cancel",
            "the safe default stays preselected on the disclosing path"
        );

        // --- the plain confirm, same lineage, nothing hidden -------------
        // Built with `hidden` at zero rather than cleared afterwards: clearing the
        // set behind `filtered` would put the baseline board back into the
        // unreachable state the fixture exists to avoid.
        let mut plain = hidden_lineage_board(3, 0);
        plain.open_delete_confirm();
        let plain_modal = plain.modal.clone().expect("the confirm is open");
        assert_eq!(
            wrap_message(&plain_modal.message, MODAL_WIDTH - 2).len(),
            4,
            "the non-disclosing prompt is unchanged at four wrapped rows"
        );
        assert_eq!(
            shortest_showing(&mut plain, "Delete lineage (3)"),
            Some(8),
            "the non-disclosing confirm still draws its strip on an 8-row terminal"
        );
        assert_eq!(
            shortest_showing(&mut plain, "Esc cancel"),
            Some(10),
            "the non-disclosing confirm still draws its footer on a 10-row terminal"
        );
    }

    /// The rejected alternative, pinned so the doc block's WIDTH rationale stays
    /// falsifiable: putting the counts in the `Delete lineage` label instead of the
    /// message costs `Cancel` — the SAFE DEFAULT — ten columns of legibility.
    ///
    /// `render_modal` never wraps a `Row` strip (it only centers it), so the strip
    /// truncates instead of reflowing. This renders BOTH strips, the shipped one and
    /// the alternative, and pins the narrowest terminal each still draws `Cancel`
    /// whole on. The labelled strip is deliberately not shipped code — it is the
    /// measurement the rationale rests on, and without it those numbers are a claim
    /// no test can contradict.
    #[test]
    fn counts_in_the_lineage_label_would_cost_cancel_ten_columns() {
        // The real disclosing copy, so the box being measured is the shipped box.
        let mut app = hidden_lineage_board(3, 2);
        app.open_delete_confirm();
        let message = app.modal.clone().expect("the confirm is open").message;

        let narrowest_whole_cancel = |lineage_label: &str| -> Option<u16> {
            (20u16..=70).find(|&w| {
                let modal = Modal {
                    title: "delete session".to_string(),
                    message: message.clone(),
                    layout: ModalLayout::Row,
                    choices: ["Delete this", lineage_label, "Cancel"]
                        .into_iter()
                        .map(|label| ModalChoice {
                            label: label.to_string(),
                            description: None,
                            action: ModalAction::Cancel,
                        })
                        .collect(),
                    selected: 2,
                    session_id: None,
                };
                let mut terminal =
                    Terminal::new(TestBackend::new(w, 24)).expect("build a test terminal");
                terminal
                    .draw(|frame| render_modal(frame, &modal))
                    .expect("render must not panic at any width");
                let buffer = terminal.backend().buffer().clone();
                (0..24u16)
                    .map(|y| full_row_text(&buffer, y, w))
                    .collect::<Vec<_>>()
                    .join("\n")
                    .contains("Cancel")
            })
        };
        assert_eq!(
            narrowest_whole_cancel("Delete lineage (5)"),
            Some(50),
            "the shipped strip keeps Cancel whole down to a 50-column terminal"
        );
        assert_eq!(
            narrowest_whole_cancel("Delete lineage (5, 2 hidden)"),
            Some(60),
            "counts in the label cost Cancel ten columns — the reason they live in \
             the message instead"
        );
    }

    /// How far the LEADING counts actually survive being clipped — the honest
    /// bound on `delete_confirm_message`'s clip-resistance argument.
    ///
    /// `wrap_message` wraps to the modal-width CONSTANT, not to the clamped area, so
    /// a terminal narrower than the box drops each message row's TAIL. Leading with
    /// the counts is the most durable placement available, but it is NOT absolute:
    /// a MULTI-DIGIT count can still be truncated into a shorter, plausible number.
    /// This pins both ends of that — the width where both counts still read, and the
    /// width where the second one is silently cut into a wrong one — so the doc
    /// block cannot claim a safety it does not have.
    #[test]
    fn the_disclosed_counts_survive_clipping_to_a_third_of_the_box() {
        // 12 members, 10 hidden: the smallest shape where BOTH counts are
        // multi-digit, which is the only shape the truncation risk shows up in.
        let mut app = hidden_lineage_board(12, 10);
        app.open_delete_confirm();
        let message = app.modal.clone().expect("the confirm is open").message;
        assert!(
            message.starts_with("12 in this lineage, 10 of them hidden."),
            "the fixture must produce two multi-digit counts: {message:?}"
        );

        // The modal's first MESSAGE row at terminal width `w` — the row directly
        // under its titled top border, which is where the counts are. Anchored on
        // the TITLE rather than on a border glyph, because the board behind the
        // overlay draws bordered panes of its own.
        let first_message_row = |app: &mut App, w: u16| -> String {
            let buffer = drawn_board(app, w, 24);
            (0..24u16)
                .map(|y| full_row_text(&buffer, y, w))
                .skip_while(|row| !row.contains("delete session"))
                .nth(1)
                .expect("the modal draws a message row under its titled top border")
                // Trim the box's own side borders off the ends.
                .trim_matches('\u{2502}')
                .to_string()
        };

        // Half the box's width: clipped, but every digit of both counts survives.
        let at_30 = first_message_row(&mut app, 30);
        assert!(
            at_30.starts_with("12 in this lineage, 10"),
            "both counts must still read whole at 30 columns: {at_30:?}"
        );

        // A third of the box's width: the hidden count is cut from 10 to 1. This is
        // the residual risk the doc block names rather than denies — a plausible
        // wrong number, not a visibly broken one.
        assert_eq!(
            first_message_row(&mut app, 23),
            "12 in this lineage, 1",
            "at 23 columns the multi-digit hidden count clips into a shorter one"
        );
    }

    // --- the pulse against a URL-bearing row ------------------------------

    /// A realistic board: several reported agents across buckets (exactly ONE
    /// pulsing), a selected session whose preview pane is populated from a real
    /// fixture, and a session label carrying a URL sharing its row with a badge —
    /// the exact shape of the user's flicker report.
    fn linked_label_board() -> App {
        // (session_id, state, label)
        let cases: [(&str, &str, &str); 5] = [
            (
                "sess-url",
                "blocked",
                "Let's evaluate https://docs.rs/ratatui-markdown/latest/ratatui_markdown/ \
                 rather than rolling our own renderer",
            ),
            ("sess-working", "working", "Refactor the JSONL parser"),
            ("sess-done", "done", "Ship the release workflow"),
            ("sess-blocked", "blocked", "Waiting on the webhook fix"),
            ("sess-idle", "idle", "Audit the terminal restore path"),
        ];
        let mut sessions = Vec::new();
        let mut reported = HashMap::new();
        for (offset, (id, state, label)) in cases.into_iter().enumerate() {
            let mut session = sample_session();
            session.session_id = id.to_string();
            session.label = label.to_string();
            // Real, distinct datestamps so `short_time` renders a full column.
            session.timestamp = Some(
                OffsetDateTime::from_unix_timestamp(
                    1_752_000_000 + i64::try_from(offset).expect("a small fixture index") * 3_600,
                )
                .expect("a valid fixture timestamp"),
            );
            reported.insert(
                session.session_id.clone(),
                ReportedAgent {
                    kind: "background".to_string(),
                    id: None,
                    state: Some(state.to_string()),
                    status: None,
                    name: None,
                },
            );
            sessions.push(session);
        }
        let mut app = App::new(sessions, Scope::All, PathBuf::from("/tmp/launch"));
        app.set_reported_agents(reported);
        // The URL row is the selected one, so its badge and its URL share a row
        // AND its preview pane is populated from the fixture on disk.
        app.selected = Some("sess-url".to_string());
        app.query = "the".to_string();
        app
    }

    /// A realistic viewport: tall enough to show every session, and wide enough
    /// that the URL-bearing ROW is drawn with a URL PREFIX on screen.
    ///
    /// A prefix, NOT the whole URL — the list pane is a fraction of this width
    /// (`DEFAULT_LIST_PERCENT`), so the label is truncated well before the URL
    /// ends. That is deliberate and enough: the invariant is that the pulse does
    /// not disturb the URL text the terminal SEES, and a terminal only scans the
    /// VISIBLE line. Do not widen this board to fit the whole URL — this shape
    /// is the user's reported one, and the invariant does not depend on it.
    const LINKED_BOARD_SIZE: (u16, u16) = (120, 40);

    /// The URL carried by [`linked_label_board`]'s `sess-url` row, which is
    /// `blocked` and therefore STEADY — which is exactly why the fixture below
    /// exists to move it onto the pulsing row.
    const FIXTURE_URL: &str = "https://docs.rs/ratatui-markdown/latest/ratatui_markdown/";

    /// [`linked_label_board`] with [`FIXTURE_URL`] moved onto the PULSING row —
    /// the worst case for the flicker report, since that row is the only one
    /// whose cells the pulse touches at all.
    fn url_on_the_pulsing_row() -> App {
        let mut app = linked_label_board();
        let working = app
            .sessions
            .iter_mut()
            .find(|s| s.session_id == "sess-working")
            .expect("the pulsing fixture row");
        working.label = format!("Assess {FIXTURE_URL} rather than rolling our own");
        app.query = String::new();
        app
    }

    /// The board at `tick`, drawn through the FULL `render` entry point.
    fn linked_board_at(tick: u64) -> ratatui::buffer::Buffer {
        let (width, height) = LINKED_BOARD_SIZE;
        let mut app = url_on_the_pulsing_row();
        app.tick = tick;
        drawn_board(&mut app, width, height)
    }

    /// A CONTROL, and the determinism guard underneath every phase test here:
    /// two renders at the SAME tick with NO state change must paint identical
    /// buffers. Any diff would mean the render path reads a clock / iterates a
    /// `HashMap` / sorts unstably — which would repaint cells on every event
    /// regardless of the pulse, and would also make the phase diffs below
    /// meaningless (they could not attribute a changed cell to the pulse).
    #[test]
    fn two_renders_at_the_same_tick_paint_identical_buffers() {
        let (width, height) = LINKED_BOARD_SIZE;
        let mut app = linked_label_board();
        app.tick = 0;
        let first = drawn_board(&mut app, width, height);
        let second = drawn_board(&mut app, width, height);

        let changed = first.diff(&second);
        assert!(
            changed.is_empty(),
            "a same-tick re-render must not churn; changed cells: {:?}",
            changed
                .iter()
                .map(|(x, y, cell)| (*x, *y, cell.symbol()))
                .collect::<Vec<_>>()
        );
    }

    /// THE bug, pinned at the buffer: when a URL shares a row with a PULSING
    /// badge, the pulse must not change any SYMBOL on that row — only styles.
    ///
    /// This is what the flicker actually was. We emit plain-text URLs (no OSC 8),
    /// so the terminal auto-detects links by TEXT PATTERN; mutating any of the
    /// line's text forces it to re-scan and re-render that line's URL underline.
    /// The dot's old glyph->blank swap was such a mutation. A style-only change
    /// leaves the text byte-identical, so there is nothing to re-detect.
    ///
    /// SCOPED TO THE AGENT'S ROW ON PURPOSE — do not "strengthen" this
    /// board-wide. The search cursor legitimately DOES change symbol between
    /// phases (show/hide is correct for a cursor, and its line carries no URL to
    /// disturb), so a whole-board version would fail on the cursor's cell for a
    /// reason that has nothing to do with this invariant.
    ///
    /// The non-empty assertion is load-bearing: it proves `Buffer::diff` reports
    /// STYLE-ONLY differences at all (`Cell`'s `PartialEq` compares fg/bg/
    /// modifier alongside the symbol). Without it, a `diff` that only noticed
    /// symbols would make this test pass vacuously — green over the very bug it
    /// claims to pin.
    #[test]
    fn the_pulse_changes_only_style_and_never_a_symbol_on_a_url_bearing_row() {
        let (width, height) = LINKED_BOARD_SIZE;
        let on = linked_board_at(0);
        let off = linked_board_at(BLINK_TICKS);

        let y = row_of(&on, width, height, "Assess");
        let changed: Vec<_> = on
            .diff(&off)
            .into_iter()
            .filter(|(_, cy, _)| *cy == y)
            .collect();

        assert!(
            !changed.is_empty(),
            "the pulse must actually change SOMETHING on the URL row, or this test \
             proves nothing — a Buffer::diff blind to style-only changes would make \
             it pass vacuously"
        );
        for (x, cy, after) in changed {
            let before = on
                .cell((x, cy))
                .expect("a changed cell must exist in both buffers");
            assert_eq!(
                before.symbol(),
                after.symbol(),
                "the pulse changed the SYMBOL at ({x},{cy}) from {:?} to {:?} — it may \
                 only restyle cells. Mutating this row's text forces the terminal to \
                 re-detect the URL in the label and flicker its underline; row: {:?}",
                before.symbol(),
                after.symbol(),
                row_text(&on, y, width).trim_end()
            );
        }
    }

    /// The stronger, complementary half: the pulse must not touch the URL's own
    /// cells AT ALL — not even their style. If every changed cell on the row sits
    /// left of the URL, we never re-emit the link text under any encoding.
    ///
    /// Under the symbol invariant above this became a much sharper claim than it
    /// was written as. It once tolerated the dot's glyph swap (that cell is left
    /// of the URL, so the diff stayed in bounds while the terminal still re-read
    /// the mutated line); now the pulse's ONLY reachable effect is a style change
    /// on a single cell, and this pins that the cell is not one the link is made
    /// of.
    #[test]
    fn the_pulse_never_rewrites_a_url_cell_on_a_pulsing_row() {
        let (width, height) = LINKED_BOARD_SIZE;
        let on = linked_board_at(0);
        let off = linked_board_at(BLINK_TICKS);

        let y = row_of(&on, width, height, "Assess");
        let url_x = column_of(&on, y, width, "https");
        let changed: Vec<_> = on
            .diff(&off)
            .into_iter()
            .filter(|(_, cy, _)| *cy == y)
            .collect();

        // Local vacuity guard: `all` over an EMPTY diff is TRUE, so a pulse that
        // died entirely would pass the bounds claim below while proving nothing.
        // Its sibling above guards the same board, but this test must not depend
        // on another test being present to be meaningful.
        assert!(
            !changed.is_empty(),
            "the pulse must actually change SOMETHING on the URL row, or the bounds \
             assertion below holds vacuously over an empty diff"
        );
        assert!(
            changed.iter().all(|(x, _, _)| *x < url_x),
            "the pulse must never rewrite a cell of the URL text itself (URL starts \
             at x={url_x}); changed columns: {:?}; row: {:?}",
            changed.iter().map(|(x, _, _)| *x).collect::<Vec<_>>(),
            row_text(&on, y, width).trim_end()
        );
    }

    // --- fork-lineage rows -------------------------------------------------

    /// The label a fork lineage's members SHARE.
    ///
    /// Identical across the lineage by construction — a background hand-off
    /// copies the transcript, so `label::finalize_label` derives the same label
    /// from both files. That is the whole reported bug, and it is why a child row
    /// must spend its width on something else. Long enough that the narrow board
    /// below genuinely cannot fit it beside a marker.
    const LINEAGE_LABEL: &str = "I see kinda double-sessions in the sessions list";
    /// The lineage-LESS control row's label, distinct so it is addressable.
    const LONE_LABEL: &str = "I kinda don't like style of the README";

    /// Session ids of the real shape (uuids), so [`short_id`] has a genuine first
    /// group to cut at.
    const BG_ID: &str = "2265afd8-3c03-466b-92fd-977c716018f3";
    const ANCESTOR_ID: &str = "e4a59d02-1111-2222-3333-444444444444";
    const LONE_ID: &str = "c6ce9d37-5555-6666-7777-888888888888";

    /// The bg copy kept growing after the fork, so it is the NEWER member and
    /// therefore the lineage's head (D1).
    const BG_TS: i64 = 200;
    /// The stalled foreground ancestor, older and folded away by default.
    const ANCESTOR_TS: i64 = 100;
    const LONE_TS: i64 = 50;

    /// Turn counts that make the pair's members tell a STORY, since that is the
    /// whole reason a child row carries one: the bg copy took the prompt and did
    /// the work, the foreground ancestor stalled at the fork point holding almost
    /// nothing. Multi-digit on purpose — a count clipped to fit would read back
    /// as a smaller, entirely plausible number, which is what
    /// [`fit_child_msgs`] refuses to let happen.
    const BG_MSGS: usize = 171;
    const ANCESTOR_MSGS: usize = 6;
    const LONE_MSGS: usize = 42;

    fn at(unix_secs: i64) -> Option<OffsetDateTime> {
        Some(OffsetDateTime::from_unix_timestamp(unix_secs).expect("a valid test timestamp"))
    }

    fn lineage_session(
        id: &str,
        root: &str,
        label: &str,
        unix_secs: i64,
        msg_count: usize,
    ) -> Session {
        Session {
            file: PathBuf::from(format!("/tmp/{id}.jsonl")),
            session_id: id.to_string(),
            cwd: PathBuf::from("/Users/me/project-alpha"),
            git_branch: Some("main".to_string()),
            timestamp: at(unix_secs),
            repo: "project-alpha".to_string(),
            label: label.to_string(),
            root_uuid: Some(root.to_string()),
            msg_count,
            content_index: String::new(),
        }
    }

    /// A board holding ONE background-fork lineage — the `bg` copy that kept
    /// growing plus the stalled `ancestor` it forked from, sharing a root uuid, a
    /// repo+branch and a label — beside a `lone` session whose lineage is only
    /// itself.
    ///
    /// The lone row is the CONTROL, and it is why all three render in a single
    /// pass: folding must be invisible to it, which a board of nothing but
    /// lineage members could never show.
    fn lineage_board() -> App {
        let sessions = vec![
            lineage_session(
                ANCESTOR_ID,
                "fork-root",
                LINEAGE_LABEL,
                ANCESTOR_TS,
                ANCESTOR_MSGS,
            ),
            lineage_session(BG_ID, "fork-root", LINEAGE_LABEL, BG_TS, BG_MSGS),
            lineage_session(LONE_ID, "other-root", LONE_LABEL, LONE_TS, LONE_MSGS),
        ];
        App::new(sessions, Scope::All, PathBuf::from("/tmp/launch"))
    }

    /// Wide enough to draw a whole lineage row (gutter + timestamp + label +
    /// marker) with room to spare, tall enough for the group head plus all three
    /// session rows EXPANDED and the block's two borders — a row scrolled out of
    /// view would silently weaken every assertion below.
    const LINEAGE_BOARD_SIZE: (u16, u16) = (100, 8);

    /// A pane too narrow to fit [`LINEAGE_LABEL`] beside the marker — the only
    /// width at which the reservation in [`fit_label`] is observable at all.
    const LINEAGE_NARROW_WIDTH: u16 = 40;

    #[test]
    fn fit_label_holds_the_marker_back_and_spends_what_is_left_on_the_label() {
        let marker = lineage_marker(1).chars().count();

        // Room for everything: the label is handed over whole and untouched.
        assert_eq!(fit_label("short", 40, 0, marker), "short");

        // Exactly the columns left once the marker is reserved: still whole.
        let budget = 20 - marker;
        let exact = "x".repeat(budget);
        assert_eq!(fit_label(&exact, 20, 0, marker), exact);

        // One column too many: the LABEL is what gives, and it gives up one more
        // column to say so, so the fit never overruns its budget.
        let fitted = fit_label(&"x".repeat(budget + 1), 20, 0, marker);
        assert!(fitted.ends_with(LABEL_ELLIPSIS));
        assert_eq!(
            fitted.chars().count() + marker,
            20,
            "label + marker fill the row exactly, never more: {fitted:?}"
        );
    }

    #[test]
    fn fit_label_gives_the_last_columns_to_the_marker_not_the_label() {
        let marker = lineage_marker(12).chars().count();

        // A row whose prefix has already eaten everything: the label vanishes
        // outright rather than shoving the marker off the edge.
        assert_eq!(fit_label("anything", 10, 10, marker), "");
        // And a prefix that OVERRUNS the row saturates rather than panicking —
        // a terminal can always be dragged narrower than the layout wants.
        assert_eq!(fit_label("anything", 4, 99, marker), "");
    }

    /// The default: three sessions, two rows, and the surviving head says so.
    #[test]
    fn render_list_marks_a_folded_head_with_a_dim_hidden_count() {
        let mut app = lineage_board();
        let (width, height) = LINEAGE_BOARD_SIZE;
        let buffer = drawn_list(&mut app, width, height);

        // Folded is the default, so only the head draws the shared label.
        let head = row_of(&buffer, width, height, LINEAGE_LABEL);
        let text = row_text(&buffer, head, width);
        assert!(
            text.contains("(+1)"),
            "the folded head must wear the count of what it stands for, or the \
             ancestor has silently vanished: {text:?}"
        );

        // Read the style off the DRAWN cells, not off the span we built: a
        // marker the List restyles away is a marker the user never sees.
        // `contains`, because the selected row is patched REVERSED | BOLD.
        let x = column_of(&buffer, head, width, "(+1)");
        for (i, ch) in "(+1)".chars().enumerate() {
            let cell = buffer
                .cell((
                    x + u16::try_from(i).expect("a marker shorter than a row"),
                    head,
                ))
                .expect("a drawn marker cell");
            assert_eq!(cell.symbol(), ch.to_string());
            assert!(
                cell.modifier.contains(Modifier::DIM),
                "the marker is a dim footnote on the row, not a second label"
            );
        }
    }

    /// The narrow pane, which is the whole reason the marker is reserved first.
    #[test]
    fn render_list_keeps_the_hidden_count_when_the_pane_is_too_narrow_for_the_label() {
        let mut app = lineage_board();
        let (_, height) = LINEAGE_BOARD_SIZE;
        let width = LINEAGE_NARROW_WIDTH;
        let buffer = drawn_list(&mut app, width, height);

        // Found by the marker, since the label is necessarily cut at this width.
        let head = row_of(&buffer, width, height, "(+1)");
        let text = row_text(&buffer, head, width);

        assert!(
            !text.contains(LINEAGE_LABEL),
            "the fixture must be too narrow for the whole label, or it proves \
             nothing about what gets cut: {text:?}"
        );
        assert!(
            text.contains(LABEL_ELLIPSIS),
            "the LABEL is what gives way, and says so: {text:?}"
        );
        // The marker is the row's LAST drawn thing: nothing was pushed past it
        // off the right edge, which is how it would silently disappear.
        assert!(
            text.trim_end().ends_with("(+1)"),
            "the marker must survive a narrow pane — it is the only thing saying \
             this row stands for another session: {text:?}"
        );
    }

    /// Expanding: the ancestor comes back, indented, saying what makes it
    /// different rather than repeating what does not.
    #[test]
    fn render_list_indents_an_expanded_child_and_shows_what_differs_from_its_head() {
        let mut app = lineage_board();
        app.expand_selected();
        let (width, height) = LINEAGE_BOARD_SIZE;
        let buffer = drawn_list(&mut app, width, height);

        // The child is addressable by its ID — which is the point: that is what
        // it draws INSTEAD of the label it would otherwise duplicate.
        let child = row_of(&buffer, width, height, &short_id(ANCESTOR_ID));
        let child_text = row_text(&buffer, child, width);
        assert!(
            !child_text.contains(LINEAGE_LABEL),
            "a child must not repeat the label it shares with its head — the \
             width is exactly what it has to spend on the difference: {child_text:?}"
        );

        // An open head stands in for nobody, so its marker is gone: `(+N)` may
        // only ever count rows that are really hidden.
        let head = row_of(&buffer, width, height, LINEAGE_LABEL);
        let head_text = row_text(&buffer, head, width);
        assert!(
            !head_text.contains("(+"),
            "an expanded head hides nothing and must not claim otherwise: {head_text:?}"
        );

        // The indent is REAL, measured in drawn columns: the child's timestamp
        // starts right of its head's, which is what the eye reads as hanging off
        // it. A gutter const that stopped indenting fails here.
        let head_ts = column_of(&buffer, head, width, &short_time(at(BG_TS)));
        let child_ts = column_of(&buffer, child, width, &short_time(at(ANCESTOR_TS)));
        assert!(
            child_ts > head_ts,
            "the child must be indented past its head ({child_ts} vs {head_ts})"
        );
        assert!(
            child_text.contains('↳'),
            "and it must say which way it hangs: {child_text:?}"
        );

        // What the child spends the reclaimed width ON. The id says WHICH
        // session; only this says whether it is worth going back to.
        assert!(
            child_text.contains(&child_msgs(ANCESTOR_MSGS)),
            "a child must report how much conversation it holds — the one field \
             that separates the stub the fork stalled from the member carrying \
             the work: {child_text:?}"
        );

        // Read the style off the DRAWN cells: the count is an annotation on the
        // id, not a second identity, and a count the List restyles away is a
        // count the user never sees. `contains`, since the selected row is
        // patched REVERSED | BOLD.
        let count = format!("{ANCESTOR_MSGS}{CHILD_MSGS_SUFFIX}");
        let x = column_of(&buffer, child, width, &count);
        for (i, ch) in count.chars().enumerate() {
            let cell = buffer
                .cell((
                    x + u16::try_from(i).expect("a count shorter than a row"),
                    child,
                ))
                .expect("a drawn count cell");
            assert_eq!(cell.symbol(), ch.to_string());
            assert!(
                cell.modifier.contains(Modifier::DIM),
                "the count hangs off the id as a dim annotation, leaving the id \
                 the row's one undimmed field to scan by"
            );
        }
    }

    /// The narrow pane, and the rule that a wrong number beats no number is
    /// FALSE: the segment goes whole or not at all.
    #[test]
    fn render_list_drops_a_childs_turn_count_whole_rather_than_clipping_it() {
        let mut app = lineage_board();
        let (_, height) = LINEAGE_BOARD_SIZE;
        let width = LINEAGE_NARROW_WIDTH;
        app.expand_selected();
        let buffer = drawn_list(&mut app, width, height);

        let id = short_id(ANCESTOR_ID);
        let child = row_of(&buffer, width, height, &id);
        let text = row_text(&buffer, child, width);

        // The fixture must genuinely be too narrow, or it pins nothing: at a
        // width that fits the count, dropping and keeping look the same.
        assert!(
            !text.contains(CHILD_MSGS_SUFFIX),
            "this pane cannot afford the count, so none of it may be drawn: {text:?}"
        );

        // The claim with the teeth. `!contains(" msgs")` alone is satisfied by
        // the very bug this forbids — appending the segment and letting the List
        // hard-clip it leaves `…e4a59d02  6 m`, which contains no " msgs" and
        // would sail through. Only "the id is still the last thing on the row"
        // can see that a fragment was pushed past it.
        assert!(
            text.trim_end().ends_with(&id),
            "the id must remain the row's last drawn field: anything after it is \
             a count fragment the List cut mid-number, and a clipped count is not \
             a smaller count — it is a WRONG one ({ANCESTOR_MSGS} msgs clipped \
             reads back as a plausible other number): {text:?}"
        );

        // And the drop costs the row nothing it used to have: a child on a pane
        // too narrow for the count draws exactly what it drew before there was
        // one.
        let unselected = " ".repeat(LIST_HIGHLIGHT_SYMBOL.chars().count());
        assert_eq!(
            text.trim_end(),
            format!(
                "{unselected}{CHILD_GUTTER}{}  {id}",
                short_time(at(ANCESTOR_TS))
            ),
        );
    }

    /// A fork lineage whose members BOTH wear a `NeedsInput` badge, so the wider
    /// `needs input` phrase (11 cols against `blocked`'s 7) eats into the row
    /// before the child's turn-count split — the width interaction Task 2.6
    /// guards. No lone control row: this fixture exists only to stress that split.
    fn needs_input_lineage_board() -> App {
        let sessions = vec![
            lineage_session(
                ANCESTOR_ID,
                "fork-root",
                LINEAGE_LABEL,
                ANCESTOR_TS,
                ANCESTOR_MSGS,
            ),
            lineage_session(BG_ID, "fork-root", LINEAGE_LABEL, BG_TS, BG_MSGS),
        ];
        let mut reported = HashMap::new();
        for id in [ANCESTOR_ID, BG_ID] {
            reported.insert(
                id.to_string(),
                ReportedAgent {
                    kind: "background".to_string(),
                    id: None,
                    state: Some("blocked".to_string()),
                    status: None,
                    name: None,
                },
            );
        }
        let mut app = App::new(sessions, Scope::All, PathBuf::from("/tmp/launch"));
        app.set_reported_agents(reported);
        app
    }

    /// Task 2.6: the wider `needs input` phrase must degrade through the EXISTING
    /// all-or-nothing `fit_child_msgs` rule, never corrupt the row. On a
    /// `NeedsInput` lineage rendered too narrow to afford the child's turn count
    /// beyond its badge, the render must not panic and the count must drop WHOLE —
    /// a count clipped mid-number is a confidently wrong number.
    #[test]
    fn render_list_degrades_a_needs_input_lineage_child_count_all_or_nothing() {
        let (_, height) = LINEAGE_BOARD_SIZE;
        // Wide enough to draw the `needs input` badge and the child's 8-char id,
        // but too narrow to fit the turn count beyond them — the regime where the
        // wider phrase forces `fit_child_msgs`'s all-or-nothing drop. The guard
        // assertions below fail loudly if a layout tweak moves the row out of it,
        // so this width can never silently stop testing the degradation.
        let width = 56;

        let mut app = needs_input_lineage_board();
        app.expand_selected();
        // Rendering through a real TestBackend at all is the "no panic" half of
        // the claim — `drawn_list` unwraps the draw.
        let buffer = drawn_list(&mut app, width, height);

        // The child is addressable by its FULL id, which also confirms the width
        // is in the intended regime: the badge sits left of the id, so a clipped
        // id would make `row_of` panic here rather than pass on a wrong row.
        let id = short_id(ANCESTOR_ID);
        let child = row_of(&buffer, width, height, &id);
        let text = row_text(&buffer, child, width);

        // The row really does wear the wider badge this test is about.
        assert!(
            text.contains("needs input"),
            "the child must draw the `needs input` badge whose extra width squeezes \
             the count: {text:?}"
        );

        // All-or-nothing: the count is dropped WHOLE. `fit_child_msgs` reserves
        // exactly what it draws, so the badge can only push the count off entirely,
        // leaving the id the row's last field with no fragment shoved past it.
        assert!(
            !text.contains(CHILD_MSGS_SUFFIX),
            "at this width the count cannot fit beyond the wider badge, so none of \
             it may be drawn — a clipped `{ANCESTOR_MSGS} msgs` reads back as a \
             plausible wrong number: {text:?}"
        );
        assert!(
            text.trim_end().ends_with(&id),
            "with the count dropped, the id must remain the row's last drawn field: \
             anything after it is a count fragment the List cut mid-number: {text:?}"
        );

        // The folded default renders without panic at the same width too, its head
        // now carrying BOTH the `needs input` badge and the `(+1)` hidden-count.
        let mut folded_app = needs_input_lineage_board();
        let folded = drawn_list(&mut folded_app, width, height);
        let head = row_of(&folded, width, height, "(+1)");
        assert!(
            row_text(&folded, head, width).contains("needs input"),
            "the folded NeedsInput head must draw its `needs input` badge and its \
             `(+1)` marker together without panic or corruption"
        );
    }

    #[test]
    fn fit_child_msgs_is_all_or_nothing_and_never_ellipsizes() {
        let segment = child_msgs(BG_MSGS);
        let width = segment.chars().count();

        // Room to spare, and room for exactly the segment: drawn whole.
        assert_eq!(
            fit_child_msgs(BG_MSGS, width + 10, 0).as_deref(),
            Some(segment.as_str())
        );
        assert_eq!(
            fit_child_msgs(BG_MSGS, width, 0).as_deref(),
            Some(segment.as_str()),
            "the exact fit is a fit — the segment reserves what it draws, no more"
        );

        // One column short: the whole segment goes. Not a shorter one, and above
        // all not an ellipsized one — `17…` would be read as 17.
        assert_eq!(
            fit_child_msgs(BG_MSGS, width - 1, 0),
            None,
            "a count that cannot be drawn whole is not drawn at all"
        );

        // The columns a row's own fields already spent count against it, and a
        // prefix that OVERRUNS the pane saturates rather than panicking — a
        // terminal can always be dragged narrower than the layout wants.
        assert_eq!(fit_child_msgs(BG_MSGS, 100, 100 - width), Some(segment));
        assert_eq!(fit_child_msgs(BG_MSGS, 4, 99), None);
    }

    /// The control, and the sharpest claim in this file: folding is INVISIBLE to
    /// a row with no lineage. Not "close to" what the board always drew — the
    /// same cells.
    #[test]
    fn render_list_leaves_a_row_with_no_hidden_members_exactly_as_it_was() {
        let mut app = lineage_board();
        let (width, height) = LINEAGE_BOARD_SIZE;
        let buffer = drawn_list(&mut app, width, height);

        let lone = row_of(&buffer, width, height, LONE_LABEL);
        let text = row_text(&buffer, lone, width);

        // Unselected rows are padded by the width of the selection marker.
        let unselected = " ".repeat(LIST_HIGHLIGHT_SYMBOL.chars().count());
        assert_eq!(
            text,
            format!(
                "{unselected}{ROW_GUTTER}{}  {LONE_LABEL}",
                short_time(at(LONE_TS))
            ),
            "a session with a lineage of one must draw exactly what it always \
             has: no marker, no indent, no ellipsis, nothing new to notice"
        );

        // And at a width where the label cannot fit: still untouched. Nothing
        // reserves anything on this row, so it is HARD-CLIPPED by the List
        // exactly as it always was, rather than width-fitted into an ellipsis it
        // never used to have. The wide board above cannot see this — its label
        // fits either way — which is precisely why the narrow half is here.
        let narrow = drawn_list(&mut app, LINEAGE_NARROW_WIDTH, height);
        let lone = row_of(&narrow, LINEAGE_NARROW_WIDTH, height, "I kinda");
        let text = row_text(&narrow, lone, LINEAGE_NARROW_WIDTH);
        assert!(
            !text.contains(LABEL_ELLIPSIS),
            "a row with nothing hidden reserves nothing, so its label must be \
             clipped by the List and never fitted: {text:?}"
        );
    }

    /// Task 3.5: while show-hidden is on, a soft-hidden session row is drawn with
    /// a `[hidden]` marker AND dimmed. Both claims are read off the DRAWN cells
    /// (PATTERNS §7 "assert drawn cells, not modifiers"), and the styling is a
    /// named `Modifier`, never RGB or an embedded ANSI escape.
    #[test]
    fn render_list_marks_and_dims_a_hidden_row_under_show_hidden() {
        // Two plain sessions in one group; ids chosen so the SHOWN row sorts
        // first (and is thus the default selection), leaving the hidden row
        // unselected so its dim is not entangled with the selection's REVERSED.
        let mut shown = sample_session();
        shown.session_id = "sess-a-shown".to_string();
        shown.label = "sess-a-shown".to_string();
        let mut hush = sample_session();
        hush.session_id = "sess-z-hush".to_string();
        hush.label = "sess-z-hush".to_string();

        let mut app = App::new(vec![shown, hush], Scope::All, PathBuf::from("/tmp/launch"));
        app.hidden_ids.insert("sess-z-hush".to_string());
        // Reveal hidden rows (false -> true) and re-filter so the hidden row draws.
        app.toggle_show_hidden();

        let (width, height) = (40u16, 8u16);
        let buffer = drawn_list(&mut app, width, height);

        // The marker lands on the hidden session's row (content first, so the
        // cells asserted below are really that row).
        let y = row_of(&buffer, width, height, "[hidden]");
        let row = row_text(&buffer, y, width);
        assert!(
            row.contains("sess-z-hush"),
            "the [hidden] marker must sit on the hidden session's own row: {row:?}"
        );

        // Every non-blank drawn cell of that row is DIM — the whole row reads as
        // demoted, not just the marker.
        for x in 1..width - 1 {
            let cell = buffer.cell((x, y)).expect("a cell within the list border");
            if cell.symbol() == " " {
                continue;
            }
            assert!(
                cell.modifier.contains(Modifier::DIM),
                "a revealed hidden row must be drawn DIM; cell {:?} at x={x} was {:?}",
                cell.symbol(),
                cell.modifier
            );
        }

        // The control: the non-hidden row is NOT dimmed, so the dim is a property
        // of being hidden, not of the whole list.
        let sy = row_of(&buffer, width, height, "sess-a-shown");
        let sx = column_of(&buffer, sy, width, "sess-a-shown");
        let scell = buffer
            .cell((sx, sy))
            .expect("a cell within the list border");
        assert!(
            !scell.modifier.contains(Modifier::DIM),
            "a non-hidden row's label must not be dimmed, got {:?}",
            scell.modifier
        );
    }
}
