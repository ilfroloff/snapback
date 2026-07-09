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

use ratatui::layout::{Alignment, Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};
use ratatui::Frame;
use time::OffsetDateTime;

use crate::search::SearchMode;
use crate::store::preview::LinkRegion;

use super::app::{resolve_list_width, App, LiveChoice, Row, Scope};

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
    // The running-session choice sits ON TOP of the board when open.
    if app.pending_live.is_some() {
        render_live_choice(frame, app);
    }
    // The new-session agent picker likewise overlays the board. The two are
    // mutually exclusive (each owns the keyboard while open), so at most one draws.
    if app.pending_agent.is_some() {
        render_agent_pick(frame, app);
    }
}

/// Prefix for the version indicator (`v0.1.0`); the leading `v` is the
/// conventional marker readers expect before a semver string.
const VERSION_PREFIX: &str = "v";

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

/// Build the header's version label (`v0.1.0`) from the compile-time crate
/// version so it always tracks `Cargo.toml`.
fn version_label() -> String {
    format!("{}{}", VERSION_PREFIX, env!("CARGO_PKG_VERSION"))
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
                Row::Session(i) => Some((*i, app.sessions[*i].label.clone())),
                Row::Group { .. } => None,
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
            Row::Session(i) => {
                let session = &app.sessions[*i];
                let mut spans = vec![
                    Span::raw("  "),
                    Span::styled(
                        short_time(session.timestamp),
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                    Span::raw("  "),
                ];
                // Compact live badge in its own column: `● bg` / `● live` (+ a dim
                // state/status qualifier). Non-live rows show nothing here. Joined
                // strictly by full session_id.
                if let Some(agent) = app.live_agent(&session.session_id) {
                    spans.push(Span::styled(
                        format!("\u{25cf} {}", agent.kind_label()),
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ));
                    if let Some(qualifier) = agent.qualifier() {
                        spans.push(Span::raw(" "));
                        spans.push(Span::styled(
                            qualifier.to_string(),
                            Style::default().add_modifier(Modifier::DIM),
                        ));
                    }
                    spans.push(Span::raw("  "));
                }
                // The visible label: under an active query, matched chars are
                // split out into light-blue spans; otherwise it is one raw span.
                // The base style is `default()` — the List's `highlight_style`
                // composes the selection over these spans at render time.
                match highlights.get(i) {
                    Some(matched) => spans.extend(highlight_label_spans(
                        &session.label,
                        matched,
                        Style::default(),
                        Style::default()
                            .fg(Color::LightBlue)
                            .add_modifier(Modifier::BOLD),
                    )),
                    None => spans.push(Span::raw(session.label.clone())),
                }
                ListItem::new(Line::from(spans))
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
        .highlight_symbol("› ");

    let mut state = ListState::default();
    *state.offset_mut() = app.scroll.min(rows.len().saturating_sub(1));
    state.select(selected_row);
    frame.render_stateful_widget(list, area, &mut state);
    // Persist the offset ratatui computed so scroll is stable across redraws
    // and preserved across reloads.
    app.scroll = state.offset();
}

/// The readable transcript preview for the selected session, vertically
/// scrollable and anchored to the newest turn by default.
///
/// The scroll offset lives in `App` but its bounds are only known here (the
/// inner width/height and the wrapped content height), so — mirroring how
/// `render_list` writes back `app.scroll` — this clamps the offset against the
/// wrapped content and writes both the resolved offset and the viewport height
/// back into `App`.
///
/// A vertical scrollbar is drawn over the block's own right border (the
/// idiomatic ratatui composition: the `Scrollbar` widget is rendered as a
/// SEPARATE pass on a one-row vertical inset of the same `area`, so its track
/// lands exactly on the border column rather than stealing a content column)
/// whenever the wrapped content overflows the viewport. When the content fits
/// entirely (`content_h <= inner_height`), the scrollbar is skipped entirely —
/// there is nothing to scroll, so no thumb is drawn — rather than rendering a
/// full-length/inactive thumb, keeping "a scrollbar is visible" a reliable
/// signal that there is more transcript to see.
fn render_preview(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" preview ");

    // Borders steal one row/col on each side; the inner area is what wraps. The
    // inner width is also the table shrink-to-fit budget, so it must be resolved
    // BEFORE rendering the preview text (which fits GFM tables to it).
    let inner_width = area.width.saturating_sub(2);
    let inner_height = area.height.saturating_sub(2);

    let text = app.preview_text(inner_width);
    if text.lines.is_empty() {
        // Nothing selected: keep the scroll bookkeeping sane and still record the
        // viewport height so a later selection can size a page.
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
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((offset, 0)),
        area,
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
        // A vertical margin of 1 keeps the track off the block's top/bottom
        // border corners and title, landing it on the border's own column.
        frame.render_stateful_widget(
            scrollbar,
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
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
fn render_search(frame: &mut Frame, app: &App, area: Rect) {
    let line = Line::from(vec![
        Span::styled("search: ", Style::default().fg(Color::Cyan)),
        Span::raw(app.query.clone()),
        Span::styled("▏", Style::default().add_modifier(Modifier::SLOW_BLINK)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// The bottom help line: the keybinding cheat sheet, or a transient board
/// status (e.g. a resume refusal) when one is set. A status wins the line and is
/// flattened to a single row (newlines -> spaces) since the help area is 1 tall.
fn render_help(frame: &mut Frame, app: &App, area: Rect) {
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
            "↑↓/jk move · Enter resume · ^F fork · ^N new · type to search · Tab name/content · ^A scope · ^/ preview · PgUp/PgDn·^U/^D·Home/End·wheel scroll · q/Esc quit",
            Style::default().add_modifier(Modifier::DIM),
        )]),
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// The running-session choice overlay: a small centered prompt offering Attach /
/// Fork / Cancel for a session `claude -r` would refuse to plain-resume.
///
/// Drawn last (on top of the board) with a [`Clear`] so the board shows through
/// only outside the box. The highlighted choice is reversed; the target session
/// id and the routing live in [`App`], so this is pure presentation.
fn render_live_choice(frame: &mut Frame, app: &App) {
    let Some(pending) = &app.pending_live else {
        return;
    };
    let area = centered_rect(frame.area(), 62, 7);

    let mut options: Vec<Span> = Vec::new();
    for (idx, choice) in LiveChoice::ORDER.iter().enumerate() {
        if idx > 0 {
            options.push(Span::raw("    "));
        }
        let mut style = Style::default().add_modifier(Modifier::BOLD);
        if *choice == pending.selected {
            style = style.add_modifier(Modifier::REVERSED);
        }
        options.push(Span::styled(format!(" {} ", choice.label()), style));
    }

    let lines = vec![
        Line::from(Span::styled(
            "This session is running — it can't be plain-resumed.",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(options),
        Line::from(""),
        Line::from(Span::styled(
            "\u{2190}/\u{2192} choose \u{b7} Enter confirm \u{b7} Esc cancel",
            Style::default().add_modifier(Modifier::DIM),
        )),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" session is running ");
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .alignment(Alignment::Center),
        area,
    );
}

/// Fixed width (columns) of the new-session agent picker overlay, matching the
/// running-session choice overlay's footprint so the two modals feel of a piece;
/// [`centered_rect`] shrinks it to fit on a tiny terminal.
const AGENT_PICK_WIDTH: u16 = 62;

/// Non-entry rows the agent picker always draws around its selectable list: a
/// title line, a blank spacer above the list, a blank spacer below it, and a
/// footer help line. Named so the overlay height (entries + this chrome + the two
/// border rows) carries no bare magic number.
const AGENT_PICK_CHROME_ROWS: u16 = 4;

/// The new-session agent picker overlay: a centered vertical list of the
/// discovered agents, preceded by a "default (no agent)" entry, with the
/// highlighted row reversed.
///
/// Drawn last (on top of the board) with a [`Clear`] so the board shows through
/// only outside the box. The selectable rows and the highlight live in [`App`], so
/// this is pure presentation. Row 0 is the synthetic default (no-agent) entry; the
/// rest map to `pending.agents` in order (mirroring `PendingAgent::selected_agent`).
fn render_agent_pick(frame: &mut Frame, app: &App) {
    let Some(pending) = &app.pending_agent else {
        return;
    };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "Start a new session — pick an agent:",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));
    // Row 0: the default (no-agent) entry; then one row per discovered agent.
    lines.push(agent_entry_line(
        "default (no agent)",
        None,
        pending.selected == 0,
    ));
    for (i, agent) in pending.agents.iter().enumerate() {
        lines.push(agent_entry_line(
            &agent.name,
            agent.description.as_deref(),
            pending.selected == i + 1,
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "↑/↓ choose · Enter start · Esc cancel",
        Style::default().add_modifier(Modifier::DIM),
    )));

    // Height: entry rows (default + each agent) + the fixed chrome + two borders.
    let entry_rows = pending.agents.len() as u16 + 1;
    let height = entry_rows
        .saturating_add(AGENT_PICK_CHROME_ROWS)
        .saturating_add(2);
    let area = centered_rect(frame.area(), AGENT_PICK_WIDTH, height);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" new session ");
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// One row of the agent picker: a `› ` marker + reversed, bold name when
/// selected (the same highlight glyph the list uses), else a padded name, with an
/// optional dim description trailing. Owns its text (`'static`) so it composes
/// into the picker `Paragraph`.
fn agent_entry_line(name: &str, description: Option<&str>, selected: bool) -> Line<'static> {
    let (marker, name_style) = if selected {
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
        Span::styled(name.to_string(), name_style),
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
    fn version_label_prefixes_v_and_carries_crate_version() {
        let label = version_label();
        assert!(label.starts_with('v'));
        assert!(label.contains(env!("CARGO_PKG_VERSION")));
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

    // --- new-session agent picker overlay ---------------------------------

    use crate::defined_agents::DefinedAgent;

    #[test]
    fn agent_entry_line_marks_the_selected_row_and_trails_the_description() {
        // A selected row leads with the highlight marker and reverses the name.
        let sel = agent_entry_line("planner", Some("plans work"), true);
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
            .expect("the name span is present");
        assert!(
            name.style.add_modifier.contains(Modifier::REVERSED),
            "the selected name is reversed"
        );

        // An unselected, description-less row is padded and not reversed.
        let unsel = agent_entry_line("planner", None, false);
        let text: String = unsel.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(
            text, "  planner",
            "unselected row is padded, no description"
        );
        let name = unsel
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "planner")
            .expect("the name span is present");
        assert!(
            !name.style.add_modifier.contains(Modifier::REVERSED),
            "an unselected name is not reversed"
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
            app.pending_agent.is_some(),
            "rendering must not disturb the open picker"
        );
    }
}
