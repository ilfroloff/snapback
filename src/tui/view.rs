//! View rendering.
//!
//! Draws the two-pane layout: a session list on the left, a readable transcript
//! preview on the right, plus a header/help line and a search input line. In the
//! all-folders scope the list shows repo -> branch group heads (git-log-style
//! folder head shown once per group); in the current-folder scope it is a flat,
//! datestamp-led, newest-first list with no group heads. Every session row leads
//! with its datestamp column. Groups and selection are styled with ratatui (no
//! hand-written ANSI).

use std::collections::{HashMap, HashSet};

use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};
use ratatui::Frame;
use time::OffsetDateTime;

use crate::agents::{self, AgentActivity, ReportedAgent};
use crate::search::SearchMode;
use crate::store::preview::LinkRegion;

use super::app::{resolve_list_width, App, Modal, ModalChoice, ModalLayout, Row, Scope};

/// Render the whole UI for one frame.
///
/// Takes `&mut App` so the list's scroll offset (managed by ratatui's
/// `ListState`) can be written back into the model, keeping scroll preserved
/// across reloads, and so the preview text can be lazily rendered + cached.
pub fn render(frame: &mut Frame, app: &mut App) {
    // header (1) | body (fill) | search (1) | help (1)
    let [header_area, body_area, search_area, help_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    render_header(frame, app, header_area);
    render_body(frame, app, body_area);
    render_search(frame, app, search_area);
    render_help(frame, app, help_area);
    // A modal (the running-session choice or the new-session agent picker) sits
    // ON TOP of the board when open. The two overlays are now one `Option<Modal>`,
    // so at most one ever draws — a fact made structural, not conventional.
    if let Some(modal) = &app.modal {
        render_modal(frame, modal);
    }
}

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

/// The top status line: title, active scope, search mode, and counts on the
/// left, with the crate version indicator right-aligned on the same row.
fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let scope = match app.scope {
        Scope::CurrentFolder => {
            let dir = app
                .launch_dir
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| app.launch_dir.to_string_lossy().into_owned());
            format!("folder:{dir}")
        }
        Scope::All => "all folders".to_string(),
    };
    let mode = match app.search_mode {
        SearchMode::NameOnly => "name",
        SearchMode::NameAndContent => "name+content",
    };

    let header = Line::from(vec![
        Span::styled(
            " snapback ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(scope, Style::default().fg(Color::Green)),
        Span::raw("  ·  search: "),
        Span::styled(mode, Style::default().fg(Color::Yellow)),
        Span::raw("  ·  "),
        Span::styled(
            format!("{} / {} sessions", app.filtered.len(), app.sessions.len()),
            Style::default().add_modifier(Modifier::DIM),
        ),
    ]);
    frame.render_widget(Paragraph::new(header), area);

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
    }
}

/// The grouped session list with git-log-style folder heads and a highlighted
/// selection. The `ListState` offset is seeded from and written back to
/// `app.scroll` so scroll survives reloads.
fn render_list(frame: &mut Frame, app: &mut App, area: Rect) {
    let rows = app.rows();
    let block = Block::default().borders(Borders::ALL).title(" sessions ");

    if rows.is_empty() {
        let msg = match app.scope {
            Scope::CurrentFolder => {
                "No sessions in this folder.\nPress Ctrl-A to show all folders."
            }
            Scope::All => "No sessions found.",
        };
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
    // session label the query matched, so the row can highlight them. This
    // needs `&mut app` (nucleo's matcher carries scratch state), so it is done
    // in a pass BEFORE the immutable item-building borrow below. We snapshot the
    // labels first (a short, display-capped clone each) so the `&mut` match call
    // never overlaps the `&app.sessions` borrow. An empty query skips the work
    // entirely — nothing is highlighted.
    let highlights: HashMap<usize, HashSet<usize>> = if app.query.is_empty() {
        HashMap::new()
    } else {
        let labels: Vec<(usize, String)> = rows
            .iter()
            .filter_map(|row| match row {
                // A child row draws no label (it shows what DIFFERS from its
                // head instead), so it has nothing to highlight.
                Row::Session {
                    index,
                    child: false,
                    ..
                } => Some((*index, app.sessions[*index].label.clone())),
                Row::Session { child: true, .. } | Row::Group { .. } => None,
            })
            .collect();
        labels
            .into_iter()
            .filter_map(|(i, label)| {
                let matched = app.match_indices(&label);
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
                    // beside it comes from the ~1s poll. Liveness is unaskable in
                    // a render (see `preview_split`) and a polled snapshot is not
                    // authority for it, so the row REPORTS what claude said and
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
        // Unknown/absent tracks the working bucket, matching `is_active`.
        AgentActivity::Working | AgentActivity::Other => BADGE_WORKING,
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
pub(crate) fn preview_banner(app: &App) -> Option<Line<'static>> {
    let agent = app.reported_agent(app.selected.as_deref()?)?;
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

/// Split the preview pane's `area` into its `(banner, transcript)` rects.
///
/// The pane's inner area — inside the block's borders, which steal one cell per
/// side (this mirrors `Block::inner` for `Borders::ALL`) — is divided into a
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
    let inner = Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
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
    let (banner_area, transcript_area) = preview_split(area, banner.is_some());
    // The transcript's width is also the table shrink-to-fit budget, so it must
    // be resolved BEFORE rendering the preview text (which fits GFM tables to
    // it). The banner split is vertical only, so this width — and therefore the
    // width-scoped preview cache — is the same with or without a banner.
    let inner_width = transcript_area.width;
    let inner_height = transcript_area.height;

    let text = app.preview_text(inner_width);
    // Nothing selected (no text AND no banner, since a banner implies a SELECTED
    // session claude reported). A reported session whose transcript is still
    // empty falls through instead: its banner is the one thing worth drawing, and
    // keeping the banner unconditional is what lets the hit-test below derive the
    // same geometry from `banner.is_some()` alone.
    if text.lines.is_empty() && banner.is_none() {
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

    // The wrapped height sums each styled line's display width (via `Line::width`).
    let content_h = wrapped_rows(text.lines.iter().map(Line::width), inner_width);
    let offset = clamp_preview_offset(
        app.preview_follow_bottom,
        app.preview_scroll,
        content_h,
        inner_height,
    );
    // Persist the resolved geometry so the scroll keys stay in bounds and can
    // size a page on the next keypress.
    app.preview_scroll = offset;
    app.preview_viewport_h = inner_height;

    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((offset, 0)),
        transcript_area,
    );

    if content_h > usize::from(inner_height) {
        // The max offset `clamp_preview_offset` can ever produce for THIS
        // geometry (mirrors that fn's own formula); needed here to know when
        // a boundary arrow should show and to size the thumb-detachment remap
        // below.
        let max_offset = content_h
            .saturating_sub(usize::from(inner_height))
            .min(usize::from(u16::MAX)) as u16;
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
}

/// WRAPPED row count of ONE logical line of display `width` at `inner_width`,
/// matching `Wrap { trim: false }`: `ceil(width / inner_width)`, and at least one
/// row (a blank line still takes a row). The SINGLE wrap-height model shared by
/// the scrollbar/content-height path ([`wrapped_rows`]) and mouse link
/// hit-testing ([`visual_to_content`] / [`link_at`]), so the two can never
/// disagree about where a content line lands on screen.
fn wrapped_line_height(width: usize, inner_width: u16) -> usize {
    // Guard a zero-width viewport (degenerate layout) so we never divide by zero.
    let inner = usize::from(inner_width.max(1));
    width.div_ceil(inner).max(1)
}

/// Total WRAPPED row count for a set of per-line display widths at `inner_width`.
/// Sums [`wrapped_line_height`] over every line. Pure so the bottom-anchor math is
/// unit-testable without a terminal.
fn wrapped_rows<I: IntoIterator<Item = usize>>(line_widths: I, inner_width: u16) -> usize {
    line_widths
        .into_iter()
        .map(|w| wrapped_line_height(w, inner_width))
        .sum()
}

/// Map a `visual_row` (rows from the top of the wrapped content) to the
/// `(content_row, sub_row)` it lands on — which logical line, and which wrapped
/// sub-row within that line — using the SAME [`wrapped_line_height`] model as the
/// scrollbar. `None` when the visual row is past the end of the content. Pure so
/// the mapping is unit-testable from widths alone.
fn visual_to_content(
    line_widths: &[usize],
    inner_width: u16,
    visual_row: usize,
) -> Option<(usize, usize)> {
    let mut acc = 0usize;
    for (content_row, &width) in line_widths.iter().enumerate() {
        let height = wrapped_line_height(width, inner_width);
        if visual_row < acc + height {
            return Some((content_row, visual_row - acc));
        }
        acc += height;
    }
    None
}

/// The url of a preview link under a mouse click at screen `(col, row)`, or `None`.
///
/// `inner` is the preview pane's INNER rect (inside the borders), `scroll_offset`
/// the resolved vertical offset in wrapped rows (`App::preview_scroll`), and
/// `line_widths` the per-content-line display widths of the drawn text. The click
/// is translated to content coordinates and matched against a [`LinkRegion`]:
/// screen row -> wrapped visual row (via `scroll_offset`) -> `(content_row,
/// sub_row)` -> content column. A region whose `col_start..col_end` on that row
/// contains the content column yields its url.
///
/// Because a region spans the label's full content-column range, a link that
/// SOFT-WRAPS across visual rows is hit on ANY of its wrapped segments for free
/// (each segment's cells map back into the same content-column range) — no special
/// case. v1: the char-wrap column model is EXACT for any link on a line that fits
/// the inner width (the common case; no wrapping there); for a link on a
/// soft-wrapped line it uses the same ceil model the scrollbar does, so
/// hit-testing never diverges from the MEASURED layout. Pure and terminal-free.
pub(crate) fn link_at<'a>(
    col: u16,
    row: u16,
    inner: Rect,
    scroll_offset: u16,
    line_widths: &[usize],
    regions: &'a [LinkRegion],
) -> Option<&'a str> {
    if !inner.contains(Position { x: col, y: row }) {
        return None;
    }
    let rel_col = usize::from(col - inner.x);
    let rel_row = usize::from(row - inner.y);
    let visual_row = usize::from(scroll_offset) + rel_row;
    let (content_row, sub_row) = visual_to_content(line_widths, inner.width, visual_row)?;
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
/// transcript never underflows past zero and a huge one never overflows `u16`.
fn clamp_preview_offset(
    follow_bottom: bool,
    requested: u16,
    content_h: usize,
    viewport_h: u16,
) -> u16 {
    let max_offset = content_h
        .saturating_sub(usize::from(viewport_h))
        .min(usize::from(u16::MAX)) as u16;
    if follow_bottom {
        max_offset
    } else {
        requested.min(max_offset)
    }
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
    offset: u16,
    max_offset: u16,
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
    usize::from(offset).max(lo).min(hi)
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
fn chord_hint(selected_hidden: bool) -> String {
    let x = if selected_hidden { "expose" } else { "hide" };
    format!("^X  x {x} · d delete · h show/hide hidden · Esc cancel")
}

/// The bottom help line: the keybinding cheat sheet, a transient board status
/// (e.g. a resume refusal) when one is set, or the [`CHORD_HINT`] while a `Ctrl-X`
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
    let line = match &app.status {
        Some(status) => {
            let flat = status.split_whitespace().collect::<Vec<_>>().join(" ");
            Line::from(vec![Span::styled(
                flat,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )])
        }
        None => Line::from(vec![Span::styled(
            "↑↓/jk move · ←/→ fold/expand · Enter resume · ^F fork · ^N new · ^X hide/del · type to search · Tab name/content · ^A scope · ^/ preview · PgUp/PgDn·^U/^D·Home/End·wheel scroll · q/Esc quit",
            Style::default().add_modifier(Modifier::DIM),
        )]),
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

/// The generic modal overlay: a centered bordered box with a title, a message, its
/// choices (a `Row` button strip or a vertical `List`), and a footer help line.
///
/// Drawn last (on top of the board) with a [`Clear`] so the board shows through
/// only outside the box. The choices, the highlight, and the routing all live on
/// the [`Modal`] in [`App`], so this is pure presentation. Styled with named
/// colors + modifiers only (terminal-safe). The message accent and footer are
/// derived from the layout, preserving each overlay's original chrome: a `Row`
/// reads as a warning/confirm (`Yellow`, `←/→ … Enter confirm`, centered), a
/// `List` as a picker (`Cyan`, `↑/↓ … Enter start`, left-aligned).
fn render_modal(frame: &mut Frame, modal: &Modal) {
    let (accent, footer) = match modal.layout {
        ModalLayout::Row => (
            Color::Yellow,
            "\u{2190}/\u{2192} choose \u{b7} Enter confirm \u{b7} Esc cancel",
        ),
        ModalLayout::List => (Color::Cyan, "↑/↓ choose · Enter start · Esc cancel"),
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

/// Break `label` into consecutive `(text, is_match)` runs on CHAR boundaries.
///
/// A char at CHAR position `p` is a match iff `matched` contains `p`; adjacent
/// chars of the same match-state are coalesced into one run so the span count
/// stays small. Iterates `chars().enumerate()`, so every boundary is a valid
/// char boundary — multi-byte / unicode labels are safe (never a raw byte
/// slice) — and any index in `matched` that is past the label's last char is
/// simply never encountered, so an out-of-range index (e.g. from a
/// width-truncated label) is ignored rather than panicking.
///
/// Pure and terminal-free so the run breakdown is unit-testable on its own; the
/// same helper backs both the flat and the grouped list (they share one session
/// row renderer).
fn highlight_runs(label: &str, matched: &HashSet<usize>) -> Vec<(String, bool)> {
    let mut runs: Vec<(String, bool)> = Vec::new();
    for (char_pos, ch) in label.chars().enumerate() {
        let is_match = matched.contains(&char_pos);
        match runs.last_mut() {
            Some((text, run_match)) if *run_match == is_match => text.push(ch),
            _ => runs.push((ch.to_string(), is_match)),
        }
    }
    runs
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

    #[test]
    fn wrapped_rows_counts_each_line_ceil_over_inner_width() {
        // 10 -> ceil(10/8)=2, empty(0) -> 1, 25 -> ceil(25/8)=4  => 7 rows.
        assert_eq!(wrapped_rows([10usize, 0, 25], 8), 7);
    }

    #[test]
    fn wrapped_rows_empty_and_exact_lines() {
        // Blank lines each take a row; an exact multiple does not gain a phantom.
        assert_eq!(wrapped_rows([0usize, 0, 0], 5), 3);
        assert_eq!(wrapped_rows([16usize], 8), 2, "16/8 is exactly 2 rows");
        assert_eq!(wrapped_rows([7usize], 8), 1, "shorter than width => 1 row");
    }

    #[test]
    fn wrapped_rows_guards_zero_inner_width() {
        // A degenerate zero-width viewport must not divide by zero.
        assert_eq!(wrapped_rows([5usize], 0), 5);
    }

    // --- preview link hit-testing (content<->screen mapping) --------------

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

    #[test]
    fn visual_to_content_maps_rows_across_wrapped_lines() {
        // Line 0 wraps to 3 rows (45/20), line 1 is 1 row, line 2 is 1 row.
        let widths = [45usize, 0, 10];
        assert_eq!(visual_to_content(&widths, 20, 0), Some((0, 0)));
        assert_eq!(
            visual_to_content(&widths, 20, 2),
            Some((0, 2)),
            "3rd wrap row of line 0"
        );
        assert_eq!(
            visual_to_content(&widths, 20, 3),
            Some((1, 0)),
            "line 1 starts after 3 rows"
        );
        assert_eq!(visual_to_content(&widths, 20, 4), Some((2, 0)));
        assert_eq!(
            visual_to_content(&widths, 20, 5),
            None,
            "past the end of content"
        );
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
        let widths = [3usize, 0, 10];
        let regions = [region(2, 4, 8, "u")];
        // Inside the label (content col 4..7) -> the url.
        assert_eq!(
            link_at(inner.x + 4, inner.y + 2, inner, 0, &widths, &regions),
            Some("u")
        );
        assert_eq!(
            link_at(inner.x + 7, inner.y + 2, inner, 0, &widths, &regions),
            Some("u")
        );
        // One cell past the end (col_end is exclusive) -> None.
        assert_eq!(
            link_at(inner.x + 8, inner.y + 2, inner, 0, &widths, &regions),
            None
        );
        // One cell before the start -> None.
        assert_eq!(
            link_at(inner.x + 3, inner.y + 2, inner, 0, &widths, &regions),
            None
        );
    }

    #[test]
    fn link_at_is_none_on_blank_rows_and_outside_the_pane() {
        let inner = inner_rect();
        let widths = [3usize, 0, 10];
        let regions = [region(2, 4, 8, "u")];
        // A click on the blank content line 1 hits no region.
        assert_eq!(
            link_at(inner.x + 4, inner.y + 1, inner, 0, &widths, &regions),
            None
        );
        // A click left of the inner rect is rejected outright.
        assert_eq!(link_at(0, inner.y + 2, inner, 0, &widths, &regions), None);
        // A click below the content (inside the pane, past the last line) -> None.
        assert_eq!(
            link_at(inner.x + 4, inner.y + 5, inner, 0, &widths, &regions),
            None
        );
    }

    #[test]
    fn link_at_hits_a_soft_wrapped_link_on_its_second_visual_row() {
        let inner = inner_rect();
        // One content line 45 cells wide wraps into 3 visual rows (inner width 20).
        // A link at content columns 25..30 lives on the SECOND wrapped row.
        let widths = [45usize];
        let regions = [region(0, 25, 30, "w")];
        // Second visual row, column 7 => content col 20 + 7 = 27, inside 25..30.
        assert_eq!(
            link_at(inner.x + 7, inner.y + 1, inner, 0, &widths, &regions),
            Some("w"),
            "a wrapped link is clickable on its second visual segment"
        );
        // The SAME column on the first visual row is content col 7 -> no link.
        assert_eq!(
            link_at(inner.x + 7, inner.y, inner, 0, &widths, &regions),
            None
        );
    }

    #[test]
    fn link_at_respects_the_scroll_offset() {
        let inner = inner_rect();
        // Five unwrapped lines; a link on line 3 spanning columns 0..3.
        let widths = [3usize, 3, 3, 3, 3];
        let regions = [region(3, 0, 3, "s")];
        // Scrolled down 2 rows, screen row rel 1 => visual row 3 => content line 3.
        assert_eq!(
            link_at(inner.x + 1, inner.y + 1, inner, 2, &widths, &regions),
            Some("s")
        );
        // Without the scroll, the same screen cell is content line 1 -> no link.
        assert_eq!(
            link_at(inner.x + 1, inner.y + 1, inner, 0, &widths, &regions),
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
        let inner_width = width - 2;
        let inner_height = height - 2;
        let text = app.preview_text(inner_width);
        let content_h = wrapped_rows(text.lines.iter().map(Line::width), inner_width);
        assert!(
            content_h > usize::from(inner_height),
            "fixture must overflow the viewport for the scrollbar to render \
             (content_h={content_h}, inner_height={inner_height})"
        );
        let max_offset = (content_h - usize::from(inner_height)) as u16;
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

        let inner_width = width - 2;
        let inner_height = height - 2;
        let text = app.preview_text(inner_width);
        let content_h = wrapped_rows(text.lines.iter().map(Line::width), inner_width);
        let max_offset = (content_h - usize::from(inner_height)) as u16;
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

    /// The wrapped height of `app`'s preview text at the pane's inner width —
    /// the same `wrapped_rows` model `render_preview` scrolls against — so a test
    /// can prove its fixture really overflows the viewport.
    fn content_height(app: &mut App, width: u16) -> usize {
        let inner_width = width - 2;
        let text = app.preview_text(inner_width);
        wrapped_rows(text.lines.iter().map(Line::width), inner_width)
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
        let offset = app.preview_scroll;
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

    /// One session per KNOWN qualifier: `(state, badge color, does it pulse)`.
    ///
    /// Color and pulse are asserted as a PAIR because they are one signal: gray
    /// is only honest about a working agent while that agent's dot is pulsing.
    /// Every qualifier gets its own row rather than one row per bucket, so a
    /// bucket that silently stopped covering one of its two spellings fails here.
    const BADGE_CASES: [(&str, Color, bool); 6] = [
        // Waiting on the user: the most prominent color, but STEADY.
        ("blocked", Color::Yellow, false),
        // The same bucket under its other token: yellow, and steady TOO. This
        // row is the pulse lie being fixed — `waiting` once rendered as working.
        ("waiting", Color::Yellow, false),
        // Up but not working: steady, and green is EARNED by this bucket
        // rather than being the badge's old hardcoded color.
        ("idle", Color::Green, false),
        // Quietly working: gray, and the pulse is what marks it active.
        ("working", Color::Gray, true),
        // The same bucket under its other token: gray, and pulsing TOO.
        ("busy", Color::Gray, true),
        // Finished: green (nothing is wanted from you) and steady. Only
        // observable at all because the poller passes `--all`.
        ("done", Color::Green, false),
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
        for (state, _, _) in BADGE_CASES {
            let mut session = sample_session();
            session.session_id = format!("sess-{state}");
            session.label = format!("sess-{state}");
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
        app
    }

    /// Wide enough for a whole badge row (badge + qualifier + label), tall
    /// enough for the group head plus every [`BADGE_CASES`] row (plus the
    /// block's two border rows) with slack — a row scrolled out of view would
    /// silently weaken every assertion below.
    const BADGE_BOARD_SIZE: (u16, u16) = (60, 12);

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

        for (state, color, _) in BADGE_CASES {
            let badge = badges
                .iter()
                .find(|badge| badge.row.contains(&format!("sess-{state}")))
                .unwrap_or_else(|| panic!("a badge row for the {state:?} session"));

            // Content first (structure, not styling): proves the cells read
            // below are really the kind label and not a drifted offset.
            assert_eq!(
                badge.label, "bg",
                "the dot must be followed by the kind label ({state:?} row: {:?})",
                badge.row
            );
            assert!(
                !badge.label_cells.is_empty(),
                "the {state:?} label must have drawn cells to assert over"
            );

            // The kind label always carries the bucket's badge color.
            for (fg, modifier) in &badge.label_cells {
                assert_eq!(*fg, color, "the {state:?} kind label must be {color:?}");
                // `contains`, not equality: the List's `highlight_style` layers
                // REVERSED (and its own BOLD) onto whichever row is selected, so
                // the selected badge's cells legitimately carry more than BOLD.
                assert!(
                    modifier.contains(Modifier::BOLD),
                    "the {state:?} kind label must survive to the buffer BOLD, got {modifier:?}"
                );
            }

            // The dot carries that SAME color, EXCEPT `NeedsInput`, whose `!`
            // diverges to the red accent while its label stays yellow — the
            // one-cell divergence that is the whole point of the red marker.
            let expected_dot_fg = if matches!(state, "blocked" | "waiting") {
                BADGE_NEEDS_INPUT_COLOR
            } else {
                color
            };
            assert_eq!(
                badge.dot_fg, expected_dot_fg,
                "the {state:?} dot must be {expected_dot_fg:?}, got {:?}",
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
        // bucket's alone, so nothing else changes.
        for label in ["sess-idle", "sess-working", "sess-done"] {
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

            for (state, _, _) in BADGE_CASES {
                let row = format!("sess-{state}");
                assert!(
                    drawn.iter().any(|drawn_row| drawn_row.contains(&row)),
                    "at tick {tick} (the {} phase) the {state:?} dot must STILL be drawn — \
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

        // (state -> the dot's fg) at one tick.
        let phase = |tick: u64| -> HashMap<String, Color> {
            let mut app = badge_board();
            app.tick = tick;
            let buffer = drawn_list(&mut app, width, height);
            drawn_badges(&buffer, width, height)
                .into_iter()
                .filter_map(|badge| {
                    BADGE_CASES.iter().find_map(|(state, _, _)| {
                        badge
                            .row
                            .contains(&format!("sess-{state}"))
                            .then(|| ((*state).to_string(), badge.dot_fg))
                    })
                })
                .collect()
        };

        let on = phase(0);
        let off = phase(BLINK_TICKS);

        for (state, color, pulses) in BADGE_CASES {
            let on_fg = on[state];
            let off_fg = off[state];

            // The dot's ON-phase base is `badge_color`, EXCEPT `NeedsInput`, whose
            // `!` reddens to the accent. Only the pulsing buckets (never
            // `NeedsInput`) then dim off this base.
            let glyph_base = if matches!(state, "blocked" | "waiting") {
                BADGE_NEEDS_INPUT_COLOR
            } else {
                color
            };
            assert_eq!(
                on_fg, glyph_base,
                "the {state:?} dot must carry its base glyph color in the ON phase"
            );

            if pulses {
                assert_ne!(
                    on_fg, off_fg,
                    "the {state:?} dot is ACTIVE, so its color MUST change between \
                     phases — that color change IS the pulse"
                );
                assert_eq!(
                    off_fg,
                    pulse_color(color),
                    "the {state:?} dot's OFF phase must be its declared dim partner"
                );
            } else {
                assert_eq!(
                    on_fg, off_fg,
                    "the {state:?} bucket is at rest, so its dot must be steady: \
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
        for (state, color, pulses) in BADGE_CASES {
            let badge = badges
                .iter()
                .find(|badge| badge.row.contains(&format!("sess-{state}")))
                .unwrap_or_else(|| panic!("a badge row for the {state:?} session"));

            // Content first (structure, not styling): proves the cells read
            // below are really the kind label and not a drifted offset.
            assert_eq!(
                badge.label, "bg",
                "the dot must be followed by the kind label ({state:?} row: {:?})",
                badge.row
            );
            assert!(
                !badge.label_cells.is_empty(),
                "the {state:?} label must have drawn cells to assert over"
            );

            for (fg, _) in &badge.label_cells {
                assert_eq!(
                    *fg, color,
                    "the {state:?} kind label must still be its steady {color:?} in the \
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
                "the {state:?} dot must have dimmed in the OFF phase, or this row \
                 cannot show the divergence below"
            );
            // The divergence, in ONE frame: the dot has left the base color its
            // label still holds. This IS the requirement — a label that tracked
            // the dot's style would match here instead.
            for (fg, _) in &badge.label_cells {
                assert_ne!(
                    badge.dot_fg, *fg,
                    "the {state:?} row is ACTIVE and in the OFF phase, so its dot must \
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

    /// The qualifier that classifies into `bucket`.
    ///
    /// EXHAUSTIVE on purpose: adding an `AgentActivity` bucket fails to compile
    /// here, which drags the author to the walk below — the one thing that keeps
    /// `pulse_color`'s silent identity fallback from swallowing a new pulsing
    /// bucket. (The walk's own list must then gain the bucket; a `match` cannot
    /// force that, so `ALL_BUCKETS` says so.)
    fn qualifier_reaching(bucket: AgentActivity) -> Option<&'static str> {
        match bucket {
            AgentActivity::NeedsInput => Some("blocked"),
            AgentActivity::Idle => Some("idle"),
            AgentActivity::Working => Some("working"),
            AgentActivity::Done => Some("done"),
            // The fail-soft bucket: an unrecognized qualifier, or none at all.
            AgentActivity::Other => Some("compacting"),
        }
    }

    /// Every `AgentActivity` bucket. Keep in sync with the enum — the exhaustive
    /// `match` in [`qualifier_reaching`] is what fails to compile and sends the
    /// author here when a bucket is added.
    const ALL_BUCKETS: [AgentActivity; 5] = [
        AgentActivity::NeedsInput,
        AgentActivity::Idle,
        AgentActivity::Working,
        AgentActivity::Done,
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
            let agent = agent("background", qualifier_reaching(bucket), None);
            assert_eq!(
                agents::classify(&agent),
                bucket,
                "qualifier_reaching({bucket:?}) must actually classify into that bucket, \
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
    const FULL_BOARD_SIZE: (u16, u16) = (80, 14);

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
        for needle in ["x hide", "d delete", "h show/hide hidden", "Esc cancel"] {
            assert!(
                text.contains(needle),
                "the which-key hint must list {needle:?}; drawn: {text:?}"
            );
        }
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
        use crate::tui::app::{Modal, ModalAction, ModalChoice, ModalLayout};
        // The delete prompt is wider than the modal, so it must wrap across rows —
        // both the opening and the closing clause have to reach the screen. Regression
        // guard for the clipped "... This" the old single-line render produced.
        let (width, height) = (80u16, 24u16);
        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        app.modal = Some(Modal {
            title: "delete session".to_string(),
            message: "Permanently delete this session's transcript from disk? \
                      This can't be undone."
                .to_string(),
            layout: ModalLayout::Row,
            choices: vec![
                ModalChoice {
                    label: "Delete".to_string(),
                    description: None,
                    action: ModalAction::Delete,
                },
                ModalChoice {
                    label: "Cancel".to_string(),
                    description: None,
                    action: ModalAction::Cancel,
                },
            ],
            selected: 1,
            session_id: Some("sess-live".to_string()),
        });

        let buffer = drawn_board(&mut app, width, height);
        let screen: String = (0..height)
            .map(|y| full_row_text(&buffer, y, width))
            .collect::<Vec<_>>()
            .join("\n");
        for needle in ["Permanently delete", "can't be undone."] {
            assert!(
                screen.contains(needle),
                "the wrapped delete message must show {needle:?} in full; screen:\n{screen}"
            );
        }
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
