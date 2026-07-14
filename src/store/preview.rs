//! Readable transcript preview rendering (markdown -> styled `Text`).
//!
//! Renders a human-readable transcript from a `Session` file into a
//! `ratatui::text::Text<'static>` for the preview pane. The transcript STRUCTURE
//! is styled by this module — the `you` / `claude` turn separators, the
//! `# summary` head, and the `[tool_use: NAME]` / `[tool_result]` / `[thinking]`
//! markers — while each message BODY is passed through a small, self-contained
//! markdown pass ([`markdown_body_lines`]) that styles headers, bold/italic,
//! inline code, fenced code blocks, blockquotes, ordered/unordered lists, and
//! GFM pipe tables.
//!
//! Ahead of the markdown pass, each message BODY runs through an allowlist-driven
//! control-wrapper collapse ([`collapse_control_wrappers`]). Claude Code injects a
//! fixed set of PAIRED pseudo-tags (`<command-name>`, `<system-reminder>`,
//! `<local-command-stdout>`, `<local-command-caveat>`, `<task-notification>`,
//! `<persisted-output>`, …) that
//! are noise in a transcript; only those KNOWN wrappers collapse to a single dim
//! marker (a slash-command turn renders as `▷ /name args`). Every other angle-
//! bracket token — open-only placeholders like `<session-id>`, generics like
//! `<String>`, and comparisons like `x < y > z` — is left byte-for-byte literal,
//! and a known opener with no close FAILS SOFT to literal text.
//!
//! All markdown rendering is deliberately isolated here (no external markdown
//! crate) so the color scheme stays RESTRAINED and dark-terminal-safe: it prefers
//! ratatui `Modifier`s (BOLD / ITALIC / DIM / UNDERLINED) plus a small palette of
//! NAMED ANSI colors, which adapt to the user's terminal theme (unlike hardcoded
//! RGB, which can vanish on a light background). Code — inline and fenced — is
//! DIM (and fenced code is indented), never syntax-highlighted with fixed colors.
//!
//! The most-recent `PREVIEW_LINES` rendered lines are kept (tail-cap), and the
//! caller caches the result per session id, so markdown parsing never stalls the
//! UI on a large transcript.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::Session;

/// Keep the LAST N rendered lines (most-recent turns).
pub const PREVIEW_LINES: usize = 600;

/// Upper bound on a rendered GFM table's total display width (columns +
/// separators). The PRIMARY table budget is the preview pane's inner content
/// width, plumbed in as `width` so a table shrinks-to-fit and never soft-wraps
/// (see [`render_table`]). This cap only bounds the OTHER direction: on a very
/// wide pane a table is not stretched past a comfortable reading width.
const TABLE_MAX_WIDTH: usize = 96;

/// A clickable link inside the rendered preview, in CONTENT coordinates (before
/// the preview's soft-wrap is applied at draw time).
///
/// The preview renders a link's label UNDERLINED and DISCARDS its url from the
/// visible text (no OSC 8, no raw url — see [`parse_inline_collect`]). This
/// records where that label lives so the app's own mouse handling can recover the
/// url on a click: `content_row` indexes into the returned [`Text`]'s lines, and
/// `col_start..col_end` is the label's DISPLAY-column span on that line. Columns
/// depend on the render `width` (GFM tables shrink/truncate), so regions are cached
/// TOGETHER with the `Text` under the same width discipline (see [`App`]).
///
/// [`App`]: crate::tui::app::App
#[derive(Debug, Clone, PartialEq)]
pub struct LinkRegion {
    /// Line index into the rendered [`Text`] the label sits on.
    pub content_row: usize,
    /// Display column where the label starts (inclusive).
    pub col_start: usize,
    /// Display column just past the label (exclusive).
    pub col_end: usize,
    /// The link target to open.
    pub url: String,
}

/// A rendered transcript preview: the styled [`Text`] plus the clickable
/// [`LinkRegion`]s discovered while building it. Both are produced from one pass
/// at a fixed `width`, so a region's columns always match the text as drawn.
#[derive(Debug, Default)]
pub struct RenderedPreview {
    /// The styled, markdown-rendered transcript.
    pub text: Text<'static>,
    /// Clickable link regions, in content coordinates (see [`LinkRegion`]).
    pub links: Vec<LinkRegion>,
}

/// Render a session's transcript for the preview pane, fitting GFM tables to
/// `width` (the preview pane's inner content width, in columns). Returns the
/// styled text together with the clickable link regions found within it.
pub fn render(session: &Session, width: usize) -> RenderedPreview {
    render_file_collect(&session.file, PREVIEW_LINES, width)
}

/// Render `path` into a [`RenderedPreview`]: the styled transcript plus the
/// clickable [`LinkRegion`]s, keeping the last `max_lines` visual lines.
///
/// Each record's block contributes its lines and its (block-relative) link
/// regions; both are rebased onto the growing transcript by the running line
/// offset so a region's `content_row` addresses the FINAL text. The tail-cap that
/// keeps only the most-recent `max_lines` rows rebases the regions the same way —
/// any link that scrolled off the top is dropped, never left pointing at a stale
/// row.
fn render_file_collect(path: &Path, max_lines: usize, width: usize) -> RenderedPreview {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => {
            return RenderedPreview {
                text: Text::from(format!("No such session file:\n{}", path.display())),
                links: Vec::new(),
            }
        }
    };
    let reader = BufReader::new(file);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut links: Vec<LinkRegion> = Vec::new();
    // Day of the previously ANNOTATED turn, threaded through the loop so a
    // per-message timestamp can switch to `MM-DD HH:MM` on a day rollover.
    let mut prev_day: Option<Date> = None;
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let record: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !record.is_object() {
            continue;
        }
        if let Some((block, block_links)) = render_record(&record, &mut prev_day, width) {
            let offset = lines.len();
            links.extend(rebased(block_links, offset));
            lines.extend(block);
        }
    }

    // Keep the tail (most-recent turns); split_off returns lines[start..].
    let start = lines.len().saturating_sub(max_lines);
    let tail = lines.split_off(start);
    // Rebase regions onto the kept tail, dropping any that fell off the top.
    let links = links
        .into_iter()
        .filter_map(|mut r| {
            r.content_row = r.content_row.checked_sub(start)?;
            Some(r)
        })
        .collect();
    RenderedPreview {
        text: Text::from(tail),
        links,
    }
}

/// Shift a batch of block-relative link regions down by `offset` rows so they
/// address the growing transcript.
fn rebased(links: Vec<LinkRegion>, offset: usize) -> Vec<LinkRegion> {
    links
        .into_iter()
        .map(|mut r| {
            r.content_row += offset;
            r
        })
        .collect()
}

/// Render a single record into a transcript block, or `None` to omit it.
///
/// `prev_day` carries the day of the previously ANNOTATED turn so the compact
/// per-message timestamp can roll over to `MM-DD HH:MM` when the day changes; it
/// is advanced only when this record actually renders a timestamped marker (a
/// skipped or timestamp-less turn never disturbs the rollover tracking).
fn render_record(
    record: &Value,
    prev_day: &mut Option<Date>,
    width: usize,
) -> Option<(Vec<Line<'static>>, Vec<LinkRegion>)> {
    match record.get("type").and_then(Value::as_str) {
        Some("summary") => {
            let s = record.get("summary").and_then(Value::as_str)?;
            // Keep the literal `# summary` head, now styled as a heading. No links.
            let lines = vec![marker_line_with_time(
                format!("# {s}"),
                summary_style(),
                record,
                prev_day,
            )];
            Some((lines, Vec::new()))
        }
        Some("user") => {
            if record
                .get("isSidechain")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return None;
            }
            let content = record.get("message").and_then(|m| m.get("content"))?;
            let text = user_text(content);
            if text.is_empty() {
                return None;
            }
            let mut lines = vec![
                Line::from(""),
                marker_line_with_time("\u{25b6} you".to_string(), you_style(), record, prev_day),
            ];
            // Body links are relative to the body; rebase them past the blank +
            // marker lines that lead every turn.
            let offset = lines.len();
            let (body, body_links) = collapse_body_lines_collect(&text, width);
            lines.extend(body);
            Some((lines, rebased(body_links, offset)))
        }
        Some("assistant") => {
            let content = record.get("message").and_then(|m| m.get("content"))?;
            let (body, body_links) = assistant_lines(content, width);
            if body.is_empty() {
                return None;
            }
            let mut lines = vec![
                Line::from(""),
                marker_line_with_time(
                    "\u{25cf} claude".to_string(),
                    claude_style(),
                    record,
                    prev_day,
                ),
            ];
            let offset = lines.len();
            lines.extend(body);
            Some((lines, rebased(body_links, offset)))
        }
        _ => None,
    }
}

/// Build a turn-marker line, appending a DIM per-message timestamp annotation
/// (e.g. ` · 14:23`) when THIS record carries a parseable RFC 3339 `timestamp`.
///
/// The marker span keeps its own (bold) style unchanged; only the trailing
/// annotation is DIM. FAIL-SOFT: a missing or unparseable timestamp renders the
/// marker exactly as before (no annotation) and leaves `prev_day` untouched. On
/// success `prev_day` advances to this record's day so the next annotated turn
/// can detect a rollover.
fn marker_line_with_time(
    marker: String,
    style: Style,
    record: &Value,
    prev_day: &mut Option<Date>,
) -> Line<'static> {
    let mut spans = vec![Span::styled(marker, style)];
    if let Some(ts) = record_timestamp(record) {
        let annotation = timestamp_annotation(ts, *prev_day);
        *prev_day = Some(ts.date());
        spans.push(Span::styled(
            format!(" \u{b7} {annotation}"),
            marker_style(),
        ));
    }
    Line::from(spans)
}

/// Parse a record's own `timestamp` field as RFC 3339 (the same parser the store
/// uses), or `None` when it is absent or unparseable.
fn record_timestamp(record: &Value) -> Option<OffsetDateTime> {
    let raw = record.get("timestamp").and_then(Value::as_str)?;
    OffsetDateTime::parse(raw, &Rfc3339).ok()
}

/// The compact per-message timestamp: `HH:MM` when the day matches `prev_day`,
/// else `MM-DD HH:MM`. The first annotated turn (`prev_day` is `None`) also
/// shows `MM-DD HH:MM`, since there is no prior day to compare against. Offset
/// fields are rendered as-is — no timezone conversion, matching
/// `view.rs::short_time`.
fn timestamp_annotation(ts: OffsetDateTime, prev_day: Option<Date>) -> String {
    if prev_day == Some(ts.date()) {
        format!("{:02}:{:02}", ts.hour(), ts.minute())
    } else {
        format!(
            "{:02}-{:02} {:02}:{:02}",
            u8::from(ts.month()),
            ts.day(),
            ts.hour(),
            ts.minute()
        )
    }
}

/// User `message.content` -> readable text (string, or text blocks joined with
/// newlines). Bash `utxt` (preview variant).
fn user_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Assistant `message.content` -> styled lines plus their link regions: text
/// blocks pass through the markdown pass; `tool_use` / `tool_result` / `thinking`
/// become DIM markers (kept verbatim, no links). Bash `atxt`, but the markers are
/// styled rather than inlined. Link regions are block-relative; the caller rebases
/// them onto the turn.
fn assistant_lines(content: &Value, width: usize) -> (Vec<Line<'static>>, Vec<LinkRegion>) {
    match content {
        Value::String(s) => collapse_body_lines_collect(s, width),
        Value::Array(blocks) => {
            let mut lines: Vec<Line<'static>> = Vec::new();
            let mut links: Vec<LinkRegion> = Vec::new();
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            let (text_lines, text_links) = collapse_body_lines_collect(t, width);
                            links.extend(rebased(text_links, lines.len()));
                            lines.extend(text_lines);
                        }
                    }
                    Some("tool_use") => {
                        let name = b.get("name").and_then(Value::as_str).unwrap_or("");
                        lines.push(marker_line(format!("  [tool_use: {name}]")));
                    }
                    Some("tool_result") => lines.push(marker_line("  [tool_result]".to_string())),
                    Some("thinking") => lines.push(marker_line("  [thinking]".to_string())),
                    _ => {}
                }
            }
            (lines, links)
        }
        _ => (Vec::new(), Vec::new()),
    }
}

// --- color scheme (restrained, dark-terminal-safe named ANSI + modifiers) -----

/// Body text: the terminal's default foreground, so it reads on any theme.
fn base_style() -> Style {
    Style::default()
}

/// The `# summary` head and markdown headers: a bold heading accent.
fn summary_style() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

/// `▶ you` turn separator.
fn you_style() -> Style {
    Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD)
}

/// `● claude` turn separator.
fn claude_style() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

/// Secondary markers (`[tool_use: ...]` / `[tool_result]` / `[thinking]`).
fn marker_style() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// Inline and fenced code: DIM so it reads as code without a fixed color.
fn code_style() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// Blockquotes: dim + italic, with a leading rule.
fn quote_style() -> Style {
    Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC)
}

/// Markdown header line style; the top level is additionally underlined.
fn header_style(level: usize) -> Style {
    let s = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    if level <= 1 {
        s.add_modifier(Modifier::UNDERLINED)
    } else {
        s
    }
}

/// GFM table borders/separators (the `│` column rules and the `─┼─` separator
/// row): DIM box-drawing, so they frame the table without a fixed RGB color —
/// dark-terminal-safe, matching `code_style`/`marker_style`.
fn table_border_style() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// A single DIM marker line.
fn marker_line(text: String) -> Line<'static> {
    Line::from(Span::styled(text, marker_style()))
}

// --- control-wrapper collapse -------------------------------------------------
//
// Claude Code wraps control content (slash-command turns, injected reminders,
// local command output, task notifications, persisted output) in a fixed set of
// PAIRED pseudo-tags. Rendered raw, they dump ugly `<tag>…</tag>` noise into the
// preview. This pre-pass runs BEFORE the markdown pass and collapses ONLY those
// KNOWN wrappers to the existing dim-marker convention; everything else is left
// literal.
//
// SAFETY — allowlist only. Two classes of angle-bracket tokens live in real data
// and only PAIRED control wrappers may be touched. Open-only template placeholders
// (`<session-id>`, `<skill-dir>`), generics/JSX (`<String>`, `<br>`, `<T>`), and
// comparisons (`x < y > z`) are legitimate content — collapsing them would be a
// data-loss bug. So a token is acted on ONLY when its name is in
// `CONTROL_WRAPPERS` AND it has a matching close tag; a known opener with no close
// FAILS SOFT to literal (never eats trailing content, never panics). Wrappers can
// span lines and nest different-named tags as payload (e.g. `<task-notification>`
// holds `<task-id>`/`<output-file>`), so the pass walks the WHOLE body string.

/// The ONLY paired pseudo-tag names the collapse acts on. One exact allowlist so
/// the pass can never touch a legitimate angle-bracket token (an open-only
/// placeholder, a generic, or a `<`/`>` comparison in prose).
const CONTROL_WRAPPERS: &[&str] = &[
    "command-name",
    "command-message",
    "command-args",
    "local-command-stdout",
    "local-command-stderr",
    "local-command-caveat",
    "system-reminder",
    "task-notification",
    "persisted-output",
];

/// Glyph for a collapsed slash-command turn (`▷`, U+25B7) — deliberately DISTINCT
/// from the `▶` (U+25B6) `you` turn marker so a command reads as its own thing.
const COMMAND_GLYPH: &str = "\u{25b7}";

/// Marker for a collapsed `local-command-stdout` / `local-command-stderr` wrapper.
/// The payload can be huge, so only its presence is surfaced (never inlined).
const MARKER_COMMAND_OUTPUT: &str = "[command output]";
/// Marker for a collapsed `local-command-caveat` wrapper. The caveat Claude Code
/// injects alongside `local-command-stdout` is semantically DISTINCT from the
/// command's output, so it gets its own label rather than folding into it.
const MARKER_COMMAND_CAVEAT: &str = "[command caveat]";
/// Marker for a collapsed `system-reminder` wrapper — stubbed, so an injected
/// reminder stays discoverable rather than hidden or dumped raw.
const MARKER_SYSTEM_REMINDER: &str = "[system-reminder]";
/// Marker for a collapsed `task-notification` wrapper (nested `task-id` /
/// `output-file` are consumed as payload, never shown).
const MARKER_TASK_NOTIFICATION: &str = "[task-notification]";
/// Marker for a collapsed `persisted-output` wrapper.
const MARKER_PERSISTED_OUTPUT: &str = "[persisted-output]";

/// A collapsed message body as an ordered sequence of literal prose and collapsed
/// control wrappers. Literal segments are routed through the markdown pass;
/// collapsed segments become a single marker/command line.
#[derive(Debug, PartialEq)]
enum Segment {
    /// Prose to render through [`markdown_body_lines`] unchanged.
    Literal(String),
    /// A collapsed slash-command turn -> `▷ /name args` (args omitted when empty).
    Command { name: Option<String>, args: String },
    /// A collapsed wrapper rendered as a fixed dim marker label.
    Marker(&'static str),
}

/// How an allowlisted wrapper renders. Command-turn tags carry their payload so
/// the trio (`command-name` + optional `command-args`; `command-message` is a mere
/// echo) can merge into one command line; every other wrapper maps to a fixed
/// marker label.
enum WrapperKind {
    CommandName,
    CommandArgs,
    CommandMessage,
    Marker(&'static str),
}

/// Map an allowlisted wrapper name to its render kind — the single source that
/// ties [`CONTROL_WRAPPERS`] to behavior. `None` means "not a control wrapper".
fn wrapper_kind(name: &str) -> Option<WrapperKind> {
    Some(match name {
        "command-name" => WrapperKind::CommandName,
        "command-args" => WrapperKind::CommandArgs,
        "command-message" => WrapperKind::CommandMessage,
        "local-command-stdout" | "local-command-stderr" => {
            WrapperKind::Marker(MARKER_COMMAND_OUTPUT)
        }
        "local-command-caveat" => WrapperKind::Marker(MARKER_COMMAND_CAVEAT),
        "system-reminder" => WrapperKind::Marker(MARKER_SYSTEM_REMINDER),
        "task-notification" => WrapperKind::Marker(MARKER_TASK_NOTIFICATION),
        "persisted-output" => WrapperKind::Marker(MARKER_PERSISTED_OUTPUT),
        _ => return None,
    })
}

/// If `rest` opens with a known control-wrapper tag `<name>` (name in
/// [`CONTROL_WRAPPERS`], immediately closed by `>` with no attributes), return the
/// allowlist `name` and the opener's byte length. A closing tag, an unlisted name,
/// or an attribute-bearing tag is not a control opener. Tag names are ASCII, so
/// the returned length always lands on a UTF-8 char boundary.
fn match_control_opener(rest: &str) -> Option<(&'static str, usize)> {
    let after_lt = rest.strip_prefix('<')?;
    CONTROL_WRAPPERS.iter().find_map(|&name| {
        after_lt
            .strip_prefix(name)
            .and_then(|tail| tail.strip_prefix('>'))
            .map(|_| (name, '<'.len_utf8() + name.len() + '>'.len_utf8()))
    })
}

/// Pure pre-pass: split `body` into literal and collapsed [`Segment`]s over the
/// [`CONTROL_WRAPPERS`] allowlist. Operates on the WHOLE body (wrappers span lines
/// and nest different-named tags as payload). FAIL-SOFT: a known opener with no
/// matching close is left literal (trailing content preserved, never panics); an
/// unlisted `<…>` token is left literal byte-for-byte. A body with no wrapper
/// yields exactly `[Literal(body)]`, so ordinary prose is untouched.
fn collapse_control_wrappers(body: &str) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    let mut literal = String::new();
    let mut i = 0;
    while i < body.len() {
        let rest = &body[i..];
        if let Some(consumed) = try_collapse_at(rest, &mut segments, &mut literal) {
            i += consumed;
            continue;
        }
        // Ordinary character (includes an unmatched `<` or an unlisted tag's `<`).
        let ch = rest.chars().next().expect("non-empty remainder has a char");
        literal.push(ch);
        i += ch.len_utf8();
    }
    flush_literal_segment(&mut literal, &mut segments);
    segments
}

/// Try to collapse a control wrapper at the START of `rest`. On a full match
/// (known opener + matching close) mutate `segments`/`literal` and return the byte
/// length consumed; return `None` otherwise (the caller then takes one literal
/// char), which also covers the FAIL-SOFT unclosed-opener case.
fn try_collapse_at(rest: &str, segments: &mut Vec<Segment>, literal: &mut String) -> Option<usize> {
    let (name, open_len) = match_control_opener(rest)?;
    let kind = wrapper_kind(name)?;
    let close = format!("</{name}>");
    let rel = rest[open_len..].find(&close)?;
    let payload = &rest[open_len..open_len + rel];
    match kind {
        WrapperKind::Marker(label) => {
            flush_literal_segment(literal, segments);
            segments.push(Segment::Marker(label));
        }
        WrapperKind::CommandName => add_command_field(segments, literal, Some(payload), None),
        WrapperKind::CommandArgs => add_command_field(segments, literal, None, Some(payload)),
        // A `command-message` is a mere echo: drop its payload but keep the command
        // group open so an adjacent name/args tag still merges into one line.
        WrapperKind::CommandMessage => add_command_field(segments, literal, None, None),
    }
    Some(open_len + rel + close.len())
}

/// Flush accumulated literal text as a `Segment::Literal` (an empty run is dropped).
fn flush_literal_segment(literal: &mut String, segments: &mut Vec<Segment>) {
    if !literal.is_empty() {
        segments.push(Segment::Literal(std::mem::take(literal)));
    }
}

/// Merge one slash-command tag into the current command group. The trio is
/// contiguous in real data but separated only by whitespace, so a blank pending
/// literal is absorbed and the field extends the trailing `Segment::Command`; any
/// non-blank literal (real prose) finalizes the run and starts a fresh group.
fn add_command_field(
    segments: &mut Vec<Segment>,
    literal: &mut String,
    name: Option<&str>,
    args: Option<&str>,
) {
    if literal.trim().is_empty() {
        literal.clear(); // absorb inter-tag / leading whitespace
    } else {
        flush_literal_segment(literal, segments);
    }
    if !matches!(segments.last(), Some(Segment::Command { .. })) {
        segments.push(Segment::Command {
            name: None,
            args: String::new(),
        });
    }
    if let Some(Segment::Command {
        name: cur_name,
        args: cur_args,
    }) = segments.last_mut()
    {
        if let Some(n) = name {
            *cur_name = Some(n.to_string());
        }
        if let Some(a) = args {
            *cur_args = a.to_string();
        }
    }
}

/// Text-only view over [`collapse_body_lines_collect`] (link regions discarded),
/// used by the transcript-shape tests. Runtime code calls the `_collect` variant
/// directly so it also gets the regions.
#[cfg(test)]
fn collapse_body_lines(body: &str, width: usize) -> Vec<Line<'static>> {
    collapse_body_lines_collect(body, width).0
}

/// Like [`collapse_body_lines`] but also returns the block-relative [`LinkRegion`]s
/// found in the literal (markdown) segments. Marker and command segments carry no
/// links. Regions from each literal segment are rebased onto the growing block.
fn collapse_body_lines_collect(body: &str, width: usize) -> (Vec<Line<'static>>, Vec<LinkRegion>) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut links: Vec<LinkRegion> = Vec::new();
    for seg in collapse_control_wrappers(body) {
        match seg {
            Segment::Literal(text) => {
                let (seg_lines, seg_links) = markdown_body_lines_collect(&text, width);
                links.extend(rebased(seg_links, lines.len()));
                lines.extend(seg_lines);
            }
            Segment::Marker(label) => lines.push(marker_line(label.to_string())),
            Segment::Command { name, args } => {
                if let Some(line) = command_line(name.as_deref(), &args) {
                    lines.push(line);
                }
            }
        }
    }
    (lines, links)
}

/// Render a collapsed slash-command turn as a single `▷ /name args` DIM marker.
/// The name is normalized (any leading slashes stripped, then exactly one
/// rendered) and the args are omitted when empty. A group with no usable name (a
/// bare `command-message` echo) renders nothing.
fn command_line(name: Option<&str>, args: &str) -> Option<Line<'static>> {
    let name = name?.trim().trim_start_matches('/');
    if name.is_empty() {
        return None;
    }
    let args = args.trim();
    let text = if args.is_empty() {
        format!("{COMMAND_GLYPH} /{name}")
    } else {
        format!("{COMMAND_GLYPH} /{name} {args}")
    };
    Some(marker_line(text))
}

// --- minimal markdown pass ----------------------------------------------------

/// Text-only view over [`markdown_body_lines_collect`] (link regions discarded),
/// used by the markdown/table tests. Runtime code calls the `_collect` variant
/// directly so it also gets the regions.
#[cfg(test)]
fn markdown_body_lines(body: &str, width: usize) -> Vec<Line<'static>> {
    markdown_body_lines_collect(body, width).0
}

/// Convert an inline run's [`InlineLink`]s into [`LinkRegion`]s on line
/// `content_row`, shifting their columns past the line's `prefix_width` (a list
/// bullet, blockquote rule, or ordered-item number) so the recorded columns match
/// where the label actually renders.
fn regions_from_inline(
    content_row: usize,
    prefix_width: usize,
    inline: Vec<InlineLink>,
) -> Vec<LinkRegion> {
    inline
        .into_iter()
        .map(|l| LinkRegion {
            content_row,
            col_start: prefix_width + l.col_start,
            col_end: prefix_width + l.col_end,
            url: l.url,
        })
        .collect()
}

/// Render a message BODY as styled lines PLUS the [`LinkRegion`]s for every
/// rendered link, in coordinates relative to the FIRST returned line.
///
/// Block constructs handled line-by-line: fenced code (``` / ~~~), ATX headers
/// (`#`..`######`), blockquotes (`>`), unordered (`-`/`*`/`+`) / ordered
/// (`1.`/`1)`) list items, and GFM pipe tables (a `|` header row followed by a
/// `:?-+:?` delimiter row). Everything else is a paragraph. Inline emphasis
/// (bold/italic), inline code, and links are parsed by [`parse_inline_collect`].
/// Deliberately minimal — it favors predictable, restrained styling over full
/// CommonMark.
///
/// Only the prose branches (paragraph, blockquote, unordered / ordered list item)
/// inline-parse and can carry links; headers push raw text, fenced code is
/// verbatim, and GFM table cells are inline-parsed but their shrink-to-fit
/// truncation/padding makes column mapping unreliable, so — per the v1 scope —
/// table-cell links are intentionally NOT recorded (never a wrong hit).
fn markdown_body_lines_collect(body: &str, width: usize) -> (Vec<Line<'static>>, Vec<LinkRegion>) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut links: Vec<LinkRegion> = Vec::new();
    let mut in_fence = false;

    // Collect the rows up front and walk them by index so a block can PEEK at the
    // next row (needed for GFM tables: a header row is only a table when the row
    // immediately below it is a delimiter row). The per-branch handling below is
    // otherwise identical to the previous line-by-line loop.
    let rows: Vec<&str> = body.split('\n').collect();
    let mut i = 0;
    while i < rows.len() {
        let raw = rows[i];
        let trimmed = raw.trim_start();

        // Fenced code toggle: the fence line itself is consumed, not shown.
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            i += 1;
            continue;
        }
        if in_fence {
            // Code line: DIM + indented, no inline parsing.
            lines.push(Line::from(Span::styled(format!("    {raw}"), code_style())));
            i += 1;
            continue;
        }

        // GFM pipe table: a header row containing `|` that is IMMEDIATELY followed
        // by a valid delimiter row (`:?-+:?` cells). The delimiter row is REQUIRED,
        // so a stray `|` in ordinary prose is never mistaken for a table. (`in_fence`
        // is already false here — the fenced-code branch above `continue`s.)
        // v1: table cells may hold links but their columns are not mapped (see the
        // doc comment), so no regions are recorded here.
        if trimmed.contains('|') && rows.get(i + 1).copied().is_some_and(is_table_delimiter) {
            let (table_lines, consumed) = render_table(&rows[i..], width);
            lines.extend(table_lines);
            i += consumed;
            continue;
        }

        if let Some((level, text)) = header(trimmed) {
            lines.push(Line::from(Span::styled(
                text.to_string(),
                header_style(level),
            )));
            i += 1;
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix('>') {
            let content = rest.strip_prefix(' ').unwrap_or(rest);
            let mut spans = vec![Span::styled("\u{258f} ".to_string(), quote_style())];
            let prefix_width = spans_display_width(&spans);
            let (inline_spans, inline_links) = parse_inline_collect(content, quote_style());
            links.extend(regions_from_inline(lines.len(), prefix_width, inline_links));
            spans.extend(inline_spans);
            lines.push(Line::from(spans));
            i += 1;
            continue;
        }

        let indent = raw.len() - trimmed.len();
        if let Some(rest) = unordered_item(trimmed) {
            let mut spans = vec![
                Span::raw(" ".repeat(indent)),
                Span::styled("\u{2022} ".to_string(), base_style()),
            ];
            let prefix_width = spans_display_width(&spans);
            let (inline_spans, inline_links) = parse_inline_collect(rest, base_style());
            links.extend(regions_from_inline(lines.len(), prefix_width, inline_links));
            spans.extend(inline_spans);
            lines.push(Line::from(spans));
            i += 1;
            continue;
        }
        if let Some((num, rest)) = ordered_item(trimmed) {
            let mut spans = vec![
                Span::raw(" ".repeat(indent)),
                Span::styled(format!("{num}. "), base_style()),
            ];
            let prefix_width = spans_display_width(&spans);
            let (inline_spans, inline_links) = parse_inline_collect(rest, base_style());
            links.extend(regions_from_inline(lines.len(), prefix_width, inline_links));
            spans.extend(inline_spans);
            lines.push(Line::from(spans));
            i += 1;
            continue;
        }

        let (inline_spans, inline_links) = parse_inline_collect(raw, base_style());
        links.extend(regions_from_inline(lines.len(), 0, inline_links));
        lines.push(Line::from(inline_spans));
        i += 1;
    }

    (lines, links)
}

/// An ATX header: 1..=6 leading `#` followed by a space (or end of line).
/// Returns `(level, text)`. `#word` (no space) is NOT a header.
fn header(line: &str) -> Option<(usize, &str)> {
    if !line.starts_with('#') {
        return None;
    }
    let hashes = line.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &line[hashes..];
    if rest.is_empty() {
        return Some((hashes, ""));
    }
    let text = rest.strip_prefix(' ')?;
    Some((hashes, text.trim_end()))
}

/// An unordered list item marker (`- ` / `* ` / `+ `) -> the item text.
fn unordered_item(line: &str) -> Option<&str> {
    ["- ", "* ", "+ "]
        .into_iter()
        .find_map(|m| line.strip_prefix(m))
}

/// An ordered list item (`N. ` or `N) `) -> `(number, text)`.
fn ordered_item(line: &str) -> Option<(u64, &str)> {
    let digits: String = line.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let after = &line[digits.len()..];
    let rest = after
        .strip_prefix(". ")
        .or_else(|| after.strip_prefix(") "))?;
    let num = digits.parse().ok()?;
    Some((num, rest))
}

// --- GFM pipe tables ----------------------------------------------------------
//
// A hand-rolled, restrained renderer for GitHub-flavored-markdown pipe tables:
// a header row of `|`-separated cells, a REQUIRED delimiter row (`:?-+:?` cells
// that also carry per-column alignment), then zero or more body rows. The result
// is monospace-aligned, styled `Line`s (bold header, DIM box-drawing separators)
// appended to the preview like any other block — so preview scroll, per-message
// timestamps, search-highlight, and the 600-line tail-cap keep working unchanged.
//
// Column widths are measured on each cell's MARKER-STRIPPED display text via
// `unicode-width` (see `cell_display_width`): `**x**` and `[a](b)` occupy their
// RENDERED column count (1 and 1), not their raw byte/char length, so inline
// styling inside cells cannot skew the grid. CJK/emoji "double-width" cells also
// measure at their true two columns. The whole table is fit to the preview pane's
// inner content `width` (shrink-to-fit + `…` truncation), and a final per-line
// clamp guarantees no row can exceed `width` and soft-wrap.

/// Per-column text alignment, read from the delimiter row's colons.
#[derive(Clone, Copy)]
enum Align {
    Left,
    Center,
    Right,
}

/// Is `line` a valid GFM table delimiter row? After stripping optional leading/
/// trailing pipes and surrounding whitespace, EVERY cell (split on `|`) must match
/// `:?-+:?` — one or more hyphens with an optional leading and/or trailing colon.
/// A line with no hyphen, or any non-conforming cell, is not a delimiter.
fn is_table_delimiter(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.contains('-') {
        return false;
    }
    let inner = trimmed.trim_start_matches('|').trim_end_matches('|');
    let mut saw_cell = false;
    for cell in inner.split('|') {
        saw_cell = true;
        let cell = cell.trim();
        // Strip an optional leading and trailing colon, then require `-+`.
        let body = cell.strip_prefix(':').unwrap_or(cell);
        let body = body.strip_suffix(':').unwrap_or(body);
        if body.is_empty() || !body.bytes().all(|b| b == b'-') {
            return false;
        }
    }
    saw_cell
}

/// Read a delimiter cell's alignment from its colons: `:--` left, `:-:` center,
/// `--:` right, `---`/no colon default (rendered as left).
fn cell_align(delim_cell: &str) -> Align {
    let cell = delim_cell.trim();
    match (cell.starts_with(':'), cell.ends_with(':')) {
        (true, true) => Align::Center,
        (false, true) => Align::Right,
        _ => Align::Left,
    }
}

/// Split one table row into TRIMMED cell strings. Optional leading/trailing pipes
/// and surrounding whitespace are stripped, and `\|` is treated as a LITERAL pipe
/// within a cell (not a column separator). Never panics.
fn split_table_row(line: &str) -> Vec<String> {
    let mut cells: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut chars = line.trim().chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // `\|` -> literal pipe; any other backslash is kept verbatim.
            '\\' if chars.peek() == Some(&'|') => {
                cur.push('|');
                chars.next();
            }
            '|' => {
                cells.push(std::mem::take(&mut cur).trim().to_string());
            }
            _ => cur.push(c),
        }
    }
    cells.push(cur.trim().to_string());
    // Drop the single empty cell produced by an optional leading/trailing pipe
    // (but keep genuinely-empty interior cells).
    if cells.first().is_some_and(String::is_empty) {
        cells.remove(0);
    }
    if cells.last().is_some_and(String::is_empty) {
        cells.pop();
    }
    cells
}

/// Pad/truncate `cells` to exactly `ncols`: extra cells beyond the header count
/// are dropped, short rows are padded with empty cells. Ragged rows never panic.
fn fit_row(mut cells: Vec<String>, ncols: usize) -> Vec<String> {
    cells.truncate(ncols);
    cells.resize(ncols, String::new());
    cells
}

/// Display width (terminal columns) of `s` via `unicode-width`, so `**x**` after
/// marker-stripping and CJK/emoji cells measure at their rendered column count.
fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Total display width of a run of spans (their visible content joined).
fn spans_display_width(spans: &[Span<'static>]) -> usize {
    spans
        .iter()
        .map(|s| display_width(s.content.as_ref()))
        .sum()
}

/// Display width of a table cell's VISIBLE text: parse inline markers, then
/// measure the stripped result. This is the width columns are aligned to, so a
/// styled cell (`**x**`, `` `x` ``, `[x](y)`) lines up with a plain one.
fn cell_display_width(raw: &str) -> usize {
    spans_display_width(&parse_inline(raw, Style::default()))
}

/// Truncate parsed cell `spans` to at most `width` display columns, appending a
/// `…` (U+2026, styled `ellipsis`). One column is reserved for the ellipsis; a
/// multi-column glyph that would straddle the limit is dropped whole (never split
/// mid-scalar), so the result is always `<= width` columns and never panics.
fn truncate_spans(spans: &[Span<'static>], width: usize, ellipsis: Style) -> Vec<Span<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let budget = width - 1; // reserve one column for the ellipsis
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    'outer: for span in spans {
        let mut piece = String::new();
        for ch in span.content.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + cw > budget {
                if !piece.is_empty() {
                    out.push(Span::styled(std::mem::take(&mut piece), span.style));
                }
                break 'outer;
            }
            piece.push(ch);
            used += cw;
        }
        if !piece.is_empty() {
            out.push(Span::styled(piece, span.style));
        }
    }
    out.push(Span::styled("\u{2026}".to_string(), ellipsis));
    out
}

/// Pad fitted cell `spans` (display width `fitted_w`, already `<= width`) out to
/// exactly `width` columns per `align`, with `base`-styled space runs: left pads
/// on the right, right pads on the left, center splits the padding.
fn pad_cell_spans(
    mut spans: Vec<Span<'static>>,
    fitted_w: usize,
    width: usize,
    align: Align,
    base: Style,
) -> Vec<Span<'static>> {
    let pad = width.saturating_sub(fitted_w);
    if pad == 0 {
        return spans;
    }
    let (left, right) = match align {
        Align::Left => (0, pad),
        Align::Right => (pad, 0),
        Align::Center => (pad / 2, pad - pad / 2),
    };
    let mut out = Vec::with_capacity(spans.len() + 2);
    if left > 0 {
        out.push(Span::styled(" ".repeat(left), base));
    }
    out.append(&mut spans);
    if right > 0 {
        out.push(Span::styled(" ".repeat(right), base));
    }
    out
}

/// Render one table cell to EXACTLY `width` display columns: inline-parse `raw`
/// over `base` (so `**bold**`/`` `code` ``/`[a](b)` style inside the cell), then
/// either pad (when it fits) or truncate-with-`…` and pad. Width is measured on
/// the stripped display text, so styled and plain cells stay column-aligned.
fn render_cell_spans(raw: &str, width: usize, align: Align, base: Style) -> Vec<Span<'static>> {
    let spans = parse_inline(raw, base);
    let w = spans_display_width(&spans);
    let (fitted, fitted_w) = if w <= width {
        (spans, w)
    } else {
        let truncated = truncate_spans(&spans, width, base);
        let tw = spans_display_width(&truncated);
        (truncated, tw)
    };
    pad_cell_spans(fitted, fitted_w, width, align, base)
}

/// Total display width of a `Line`'s spans (terminal columns).
fn line_display_width(line: &Line<'static>) -> usize {
    spans_display_width(&line.spans)
}

/// Final no-wrap guarantee: if a built table `line` still exceeds `width` display
/// columns (only possible when a very narrow pane with many columns pushes even
/// the 1-column-floored layout plus its structural separators over budget), clamp
/// it to `width` with `…`, so the row can never soft-wrap under `Wrap { trim:
/// false }` and scatter the grid.
fn clamp_line_to_width(line: Line<'static>, width: usize) -> Line<'static> {
    if line_display_width(&line) <= width {
        return line;
    }
    Line::from(truncate_spans(&line.spans, width, base_style()))
}

/// Shrink `widths` (widest column first) until `sum(widths) <= budget`, so the
/// table body fits the pane-derived column budget. Columns never drop below 1
/// column; if even 1-column columns overflow the budget the returned widths may
/// exceed it — [`clamp_line_to_width`] then guards the final render against wrap.
fn fit_widths(mut widths: Vec<usize>, budget: usize) -> Vec<usize> {
    while widths.iter().sum::<usize>() > budget {
        let Some((idx, _)) = widths
            .iter()
            .enumerate()
            .filter(|(_, &w)| w > 1)
            .max_by_key(|(_, &w)| w)
        else {
            break; // every column is already at its 1-char floor
        };
        widths[idx] -= 1;
    }
    widths
}

/// Render a GFM pipe table beginning at `rows[0]` (the header), with `rows[1]`
/// the delimiter, fitting the whole table to `width` display columns (the preview
/// pane's inner content width). Returns the styled lines and the number of INPUT
/// rows consumed (header + delimiter + body rows). Body rows are consumed until a
/// blank line or a non-table row (no `|` after trim). Never panics on malformed
/// input.
///
/// Cells ARE inline-parsed (`**bold**` / `` `code` `` / `[a](b)` style inside the
/// grid). Column widths are measured on each cell's marker-STRIPPED display text
/// (`**x**` is one column, not five), so styling can never skew alignment. The
/// table shrinks-to-fit `width` and truncates over-wide cells with `…`; a final
/// [`clamp_line_to_width`] pass guarantees no row exceeds `width` and soft-wraps.
fn render_table(rows: &[&str], width: usize) -> (Vec<Line<'static>>, usize) {
    let headers = split_table_row(rows[0]);
    let ncols = headers.len().max(1);

    let delim_cells = split_table_row(rows[1]);
    let aligns: Vec<Align> = (0..ncols)
        .map(|c| delim_cells.get(c).map_or(Align::Left, |d| cell_align(d)))
        .collect();

    let header_cells = fit_row(headers, ncols);

    // Consume body rows until a blank line or a non-table (`|`-less) row; that
    // terminator is NOT consumed, so the outer loop renders it normally.
    let mut body_rows: Vec<Vec<String>> = Vec::new();
    let mut consumed = 2;
    for &row in &rows[2..] {
        if row.trim().is_empty() || !row.contains('|') {
            break;
        }
        body_rows.push(fit_row(split_table_row(row), ncols));
        consumed += 1;
    }

    // Natural column width = widest cell's STRIPPED display width (header + body),
    // min 1 so empty columns still render. Then shrink to fit the pane budget.
    let natural: Vec<usize> = (0..ncols)
        .map(|c| {
            let header_w = cell_display_width(&header_cells[c]);
            let body_w = body_rows
                .iter()
                .map(|r| cell_display_width(&r[c]))
                .max()
                .unwrap_or(0);
            header_w.max(body_w).max(1)
        })
        .collect();
    // Reserve the 3-column `" │ "` / `"─┼─"` separators between columns. The
    // PRIMARY budget is the pane's inner content `width`; `TABLE_MAX_WIDTH` only
    // caps the other direction (a very wide pane). No scrollbar column is
    // subtracted: it overlays the block's right border, not a content column.
    let sep_total = 3 * ncols.saturating_sub(1);
    let budget = width.min(TABLE_MAX_WIDTH).saturating_sub(sep_total);
    let widths = fit_widths(natural, budget);

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(table_data_line(
        &header_cells,
        &widths,
        &aligns,
        base_style().add_modifier(Modifier::BOLD),
    ));
    lines.push(table_separator_line(&widths));
    for row in &body_rows {
        lines.push(table_data_line(row, &widths, &aligns, base_style()));
    }

    // Guarantee no row can wrap under `Wrap { trim: false }`.
    let lines = lines
        .into_iter()
        .map(|l| clamp_line_to_width(l, width))
        .collect();
    (lines, consumed)
}

/// Build a header/body table row: each cell inline-parsed then truncated + padded
/// to its column `width` over `cell_style`, joined by DIM `" │ "` column rules.
fn table_data_line(
    cells: &[String],
    widths: &[usize],
    aligns: &[Align],
    cell_style: Style,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (c, width) in widths.iter().enumerate() {
        if c > 0 {
            spans.push(Span::styled(" \u{2502} ".to_string(), table_border_style()));
        }
        spans.extend(render_cell_spans(&cells[c], *width, aligns[c], cell_style));
    }
    Line::from(spans)
}

/// Build the DIM box-drawing separator row under the header: `─` fill per column,
/// `─┼─` at the column junctions so each `┼` lines up with the `│` above it.
fn table_separator_line(widths: &[usize]) -> Line<'static> {
    let mut rule = String::new();
    for (c, width) in widths.iter().enumerate() {
        if c > 0 {
            rule.push_str("\u{2500}\u{253c}\u{2500}");
        }
        for _ in 0..*width {
            rule.push('\u{2500}');
        }
    }
    Line::from(Span::styled(rule, table_border_style()))
}

/// If `rest` opens with `delim` and has a later closing `delim`, return the
/// content between them and the bytes consumed (both delimiters + content).
/// Rejects an empty span (e.g. `****`) so the delimiter falls back to literal.
fn match_delim<'a>(rest: &'a str, delim: &str) -> Option<(&'a str, usize)> {
    let after = rest.strip_prefix(delim)?;
    let close = after.find(delim)?;
    if close == 0 {
        return None;
    }
    Some((&after[..close], delim.len() * 2 + close))
}

/// If `rest` opens a `[label](url)` inline link, return `(label, url, consumed)`
/// where `consumed` is the byte length of the whole `[..](..)` run. Requires a
/// `]` closing the label immediately followed by `(` and a later `)`; neither
/// label nor url may contain its own closing bracket (no nesting). Any
/// unclosed/malformed form (e.g. `[text](`) returns `None` so the `[` falls back
/// to literal text, mirroring the unclosed-delimiter behavior.
fn match_link(rest: &str) -> Option<(&str, &str, usize)> {
    let after_open = rest.strip_prefix('[')?;
    let label_end = after_open.find(']')?;
    let label = &after_open[..label_end];
    let after_paren = after_open[label_end + 1..].strip_prefix('(')?;
    let url_end = after_paren.find(')')?;
    let url = &after_paren[..url_end];
    // '[' + label + ']' + '(' + url + ')'
    let consumed = 1 + label_end + 1 + 1 + url_end + 1;
    Some((label, url, consumed))
}

/// If `rest` begins with a bare `http://` / `https://` autolink, return the URL
/// slice and its byte length. The URL runs to the first ASCII whitespace or angle
/// bracket; a trailing run of sentence punctuation (`.,;:!?`) is excluded so a URL
/// ending a sentence renders cleanly. A bare scheme with no host is not a link.
fn match_autolink(rest: &str) -> Option<(&str, usize)> {
    if !(rest.starts_with("http://") || rest.starts_with("https://")) {
        return None;
    }
    let end = rest
        .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>'))
        .unwrap_or(rest.len());
    let url = rest[..end].trim_end_matches(['.', ',', ';', ':', '!', '?']);
    if url == "http://" || url == "https://" {
        return None;
    }
    Some((url, url.len()))
}

/// One rendered link within a single inline run, in DISPLAY columns RELATIVE to
/// the start of that run (before any block prefix like a list bullet is added).
/// [`markdown_body_lines_collect`] rebases these onto the finished line to build
/// a [`LinkRegion`], so mouse click-to-open can find the url behind an underlined
/// label without ever emitting OSC 8 or showing the url.
#[derive(Debug, Clone, PartialEq)]
struct InlineLink {
    /// Display column where the visible label starts (inclusive).
    col_start: usize,
    /// Display column just past the visible label (exclusive).
    col_end: usize,
    /// The link target, retained here even though it is never rendered.
    url: String,
}

/// Parse inline markdown (`` `code` ``, `**bold**`/`__bold__`,
/// `*italic*`/`_italic_`, `[text](url)` links, and bare `http(s)://` autolinks)
/// into styled spans over `base`.
///
/// Thin wrapper over [`parse_inline_collect`] that discards the link-region
/// metadata — the single scan implementation lives there, so the styled output
/// and the recorded link columns can never diverge. Callers that need the link
/// regions (the prose branches of [`markdown_body_lines_collect`]) call
/// `parse_inline_collect` directly; table cells and every other caller use this.
fn parse_inline(text: &str, base: Style) -> Vec<Span<'static>> {
    parse_inline_collect(text, base).0
}

/// Parse inline markdown into styled spans AND the display-column span of every
/// rendered link, in one left-to-right scan.
///
/// Inline code wins first (no emphasis inside it), then links/autolinks, then
/// bold, then italic (recursing so `**a `b`**` styles the code inside the bold).
/// A link renders its VISIBLE label UNDERLINED (the url is not shown, keeping the
/// line at the label's display width — no OSC 8 or embedded escapes); an empty
/// label falls back to showing the url. An unclosed delimiter or malformed link is
/// emitted as literal text. Always returns at least one span so a blank line still
/// occupies a row.
///
/// Alongside the spans it records an [`InlineLink`] for each link/autolink at the
/// DISPLAY column it occupies (measured with `unicode-width`, so multi-byte / wide
/// labels map to the right cells). A bold/italic run recurses and its nested links
/// are shifted by the run's own starting column, so `**[a](u)**` still yields a
/// correctly-placed region.
fn parse_inline_collect(text: &str, base: Style) -> (Vec<Span<'static>>, Vec<InlineLink>) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut links: Vec<InlineLink> = Vec::new();
    let mut plain = String::new();
    // Display column of the NEXT span to emit (== display width of spans so far).
    let mut col = 0usize;
    let mut i = 0;

    while i < text.len() {
        let rest = &text[i..];

        // Inline code: `...`
        if let Some(after) = rest.strip_prefix('`') {
            if let Some(close) = after.find('`') {
                col += flush_plain(&mut plain, &mut spans, base);
                let content = after[..close].to_string();
                col += display_width(&content);
                spans.push(Span::styled(content, code_style()));
                i += 1 + close + 1;
                continue;
            }
        }
        // Inline link: [label](url) -> UNDERLINED label (url not shown), region recorded.
        if let Some((label, url, consumed)) = match_link(rest) {
            col += flush_plain(&mut plain, &mut spans, base);
            let shown = if label.is_empty() { url } else { label };
            let width = display_width(shown);
            links.push(InlineLink {
                col_start: col,
                col_end: col + width,
                url: url.to_string(),
            });
            spans.push(Span::styled(
                shown.to_string(),
                base.add_modifier(Modifier::UNDERLINED),
            ));
            col += width;
            i += consumed;
            continue;
        }
        // Bare autolink: http(s)://... -> UNDERLINED, region recorded.
        if let Some((url, consumed)) = match_autolink(rest) {
            col += flush_plain(&mut plain, &mut spans, base);
            let width = display_width(url);
            links.push(InlineLink {
                col_start: col,
                col_end: col + width,
                url: url.to_string(),
            });
            spans.push(Span::styled(
                url.to_string(),
                base.add_modifier(Modifier::UNDERLINED),
            ));
            col += width;
            i += consumed;
            continue;
        }
        // Bold: **...** or __...__
        if let Some((content, consumed)) =
            match_delim(rest, "**").or_else(|| match_delim(rest, "__"))
        {
            col += flush_plain(&mut plain, &mut spans, base);
            col += extend_with_nested(
                &mut spans,
                &mut links,
                col,
                content,
                base.add_modifier(Modifier::BOLD),
            );
            i += consumed;
            continue;
        }
        // Italic: *...* or _..._
        if let Some((content, consumed)) = match_delim(rest, "*").or_else(|| match_delim(rest, "_"))
        {
            col += flush_plain(&mut plain, &mut spans, base);
            col += extend_with_nested(
                &mut spans,
                &mut links,
                col,
                content,
                base.add_modifier(Modifier::ITALIC),
            );
            i += consumed;
            continue;
        }

        // Ordinary character.
        let ch = rest.chars().next().unwrap();
        plain.push(ch);
        i += ch.len_utf8();
    }

    col += flush_plain(&mut plain, &mut spans, base);
    let _ = col; // final column not needed past the last flush
    if spans.is_empty() {
        // Keep blank lines as a (styled) empty span so height counting sees a row.
        spans.push(Span::styled(String::new(), base));
    }
    (spans, links)
}

/// Recurse into a bold/italic `content` run at display column `base_col`, append
/// its spans, rebase its nested links by `base_col`, and return the run's display
/// width so the caller can advance its column cursor. Keeps the emphasis recursion
/// and the link-column bookkeeping in exactly one place.
fn extend_with_nested(
    spans: &mut Vec<Span<'static>>,
    links: &mut Vec<InlineLink>,
    base_col: usize,
    content: &str,
    style: Style,
) -> usize {
    let (sub_spans, sub_links) = parse_inline_collect(content, style);
    for mut link in sub_links {
        link.col_start += base_col;
        link.col_end += base_col;
        links.push(link);
    }
    let width = spans_display_width(&sub_spans);
    spans.extend(sub_spans);
    width
}

/// Flush any accumulated plain text as a `base`-styled span, returning the display
/// width flushed (`0` when empty) so the inline scan can advance its column cursor.
fn flush_plain(plain: &mut String, spans: &mut Vec<Span<'static>>, base: Style) -> usize {
    if plain.is_empty() {
        return 0;
    }
    let width = display_width(plain);
    spans.push(Span::styled(std::mem::take(plain), base));
    width
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A generous preview width for tests that are NOT exercising shrink-to-fit,
    /// so tables render at their natural width without wrapping or truncation.
    const WIDE: usize = TABLE_MAX_WIDTH;

    fn fixture(folder: &str, file: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("store")
            .join(folder)
            .join(file)
    }

    /// Text-only convenience over [`render_file_collect`] for the transcript-shape
    /// tests that assert markers/structure rather than link regions.
    fn render_file(path: &Path, max_lines: usize, width: usize) -> Text<'static> {
        render_file_collect(path, max_lines, width).text
    }

    /// Flatten a `Text` back to a plain string (span contents joined, lines by
    /// `\n`) so structural markers can be asserted independent of styling.
    fn flatten(text: &Text) -> String {
        text.lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Find the first line whose LEADING span content equals `needle` (the turn
    /// marker), ignoring any trailing DIM timestamp annotation span.
    fn line_led_by<'a>(text: &'a Text, needle: &str) -> Option<&'a Line<'a>> {
        text.lines
            .iter()
            .find(|l| l.spans.first().map(|s| s.content.as_ref()) == Some(needle))
    }

    fn unique_temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "snapback-preview-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn render_keeps_turn_separators_and_tool_markers() {
        let text = render_file(
            &fixture("-Users-me-project-alpha", "sess-normal-1.jsonl"),
            PREVIEW_LINES,
            WIDE,
        );
        let plain = flatten(&text);
        assert!(
            plain.contains("\u{25b6} you"),
            "missing user separator:\n{plain}"
        );
        assert!(
            plain.contains("\u{25cf} claude"),
            "missing claude separator:\n{plain}"
        );
        assert!(
            plain.contains("[tool_use: Read]"),
            "missing tool marker:\n{plain}"
        );
        // Styling is via ratatui Style, never embedded ANSI escapes.
        assert!(!plain.contains('\u{1b}'), "preview must not embed ANSI");
    }

    #[test]
    fn turn_separators_are_styled_bold() {
        let text = render_file(
            &fixture("-Users-me-project-alpha", "sess-normal-1.jsonl"),
            PREVIEW_LINES,
            WIDE,
        );
        for sep in ["\u{25b6} you", "\u{25cf} claude"] {
            let line = line_led_by(&text, sep).unwrap_or_else(|| panic!("no {sep} line"));
            assert!(
                line.spans[0].style.add_modifier.contains(Modifier::BOLD),
                "{sep} separator must be bold"
            );
        }
    }

    #[test]
    fn markdown_body_styles_headers_emphasis_and_code() {
        let body =
            "# Title\n\nA **bold** and *italic* and `code` word.\n\n```\nfn main() {}\n```\n\n- item";
        let lines = markdown_body_lines(body, WIDE);

        // Header: hashes stripped, styled bold.
        let header = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.as_ref() == "Title"))
            .expect("header line");
        assert!(
            header.spans[0].style.add_modifier.contains(Modifier::BOLD),
            "header must be bold"
        );

        // Emphasis + inline code on the paragraph line.
        let para = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.as_ref() == "bold"))
            .expect("paragraph line");
        let find = |needle: &str| {
            para.spans
                .iter()
                .find(|s| s.content.as_ref() == needle)
                .unwrap_or_else(|| panic!("no span {needle}"))
                .style
        };
        assert!(find("bold").add_modifier.contains(Modifier::BOLD));
        assert!(find("italic").add_modifier.contains(Modifier::ITALIC));
        assert!(
            find("code").add_modifier.contains(Modifier::DIM),
            "inline code is dim"
        );

        // Fenced code: indented + dim, not run through inline parsing.
        let code = lines
            .iter()
            .find(|l| {
                l.spans
                    .iter()
                    .any(|s| s.content.as_ref().contains("fn main"))
            })
            .expect("code line");
        assert!(
            code.spans[0].content.as_ref().starts_with("    "),
            "fenced code is indented"
        );
        assert!(code.spans[0].style.add_modifier.contains(Modifier::DIM));

        // Unordered list bullet.
        assert!(
            lines
                .iter()
                .any(|l| l.spans.iter().any(|s| s.content.as_ref() == "\u{2022} ")),
            "unordered item must render a bullet"
        );
    }

    #[test]
    fn inline_parser_leaves_unclosed_delimiters_literal() {
        // No closing `**` / `` ` `` => emitted verbatim, never panics.
        let spans = parse_inline("a **b and `c", base_style());
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "a **b and `c");
    }

    #[test]
    fn blank_body_line_keeps_a_row() {
        // An empty paragraph line yields one (empty) span so height counting and
        // the wrapped-row math still see a visual row.
        let lines = markdown_body_lines("", WIDE);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans.len(), 1);
        assert_eq!(lines[0].spans[0].content.as_ref(), "");
    }

    // --- GFM pipe tables ---------------------------------------------------

    /// Join a single `Line`'s span contents back into its plain text.
    fn line_text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn table_renders_header_separator_and_aligned_body_rows() {
        // A well-formed 2-column table: header + separator + two body rows, all
        // padded to the same total display width (the alignment invariant).
        let body = "| A | B |\n| --- | --- |\n| 1 | 22 |\n| 333 | 4 |";
        let lines = markdown_body_lines(body, WIDE);
        assert_eq!(lines.len(), 4, "header + separator + 2 body rows");

        // Column A width = max(len "A","1","333") = 3; column B = max("B","22","4") = 2.
        // Every rendered line is the same width: 3 + 3 (" │ ") + 2 = 8.
        let widths: Vec<usize> = lines.iter().map(|l| line_text(l).chars().count()).collect();
        assert!(
            widths.iter().all(|&w| w == widths[0]),
            "all table lines share one width (aligned columns): {widths:?}"
        );
        assert_eq!(widths[0], 8, "3 (col A) + 3 (' │ ') + 2 (col B)");

        // Header is bold; the separator is a DIM box-drawing rule with a junction.
        assert!(lines[0].spans[0]
            .style
            .add_modifier
            .contains(Modifier::BOLD));
        let sep = line_text(&lines[1]);
        assert!(
            sep.contains('\u{253c}'),
            "separator has a ┼ junction: {sep}"
        );
        assert!(lines[1].spans[0].style.add_modifier.contains(Modifier::DIM));
        // First body cell "1" left-padded to width 3 (cells now render as
        // inline-parsed spans, so the content + its pad may be separate spans).
        assert!(
            line_text(&lines[2]).starts_with("1  "),
            "first cell left-padded to width 3: {}",
            line_text(&lines[2])
        );
    }

    #[test]
    fn table_alignment_markers_place_padding_left_center_and_right() {
        // Header cells are width 4; single-char body cells expose padding side.
        let body = "| Left | Cent | Rght |\n| :--- | :--: | ---: |\n| x | y | z |";
        let lines = markdown_body_lines(body, WIDE);
        // Each cell is width 4: left pads on the right, center splits the pad,
        // right pads on the left, joined by the DIM " │ " column rules. Assert
        // the whole flattened row so the padding sides are pinned exactly.
        let expected = ["x   ", " \u{2502} ", " y  ", " \u{2502} ", "   z"].concat();
        assert_eq!(line_text(&lines[2]), expected, "left/center/right padding");
    }

    #[test]
    fn pipe_in_prose_without_a_delimiter_row_is_not_a_table() {
        // A `|` in ordinary text with no delimiter row underneath stays a paragraph.
        let body = "This | that and the other.\nJust a normal paragraph.";
        let lines = markdown_body_lines(body, WIDE);
        assert_eq!(lines.len(), 2, "two ordinary paragraph lines, no table");
        let first = line_text(&lines[0]);
        assert_eq!(first, "This | that and the other.");
        // No box-drawing was emitted -> nothing was treated as a table.
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(!joined.contains('\u{2502}') && !joined.contains('\u{253c}'));
    }

    #[test]
    fn ragged_table_degrades_without_panicking() {
        // A short row (padded) and an over-long row (extra cells dropped) must not
        // panic; the table still renders header + separator + two body rows.
        let body = "| A | B | C |\n| --- | --- | --- |\n| 1 |\n| 2 | 3 | 4 | 5 | 6 |";
        let lines = markdown_body_lines(body, WIDE);
        assert_eq!(lines.len(), 4, "header + separator + 2 body rows");
        // Every line stays the same width despite the ragged input.
        let widths: Vec<usize> = lines.iter().map(|l| line_text(l).chars().count()).collect();
        assert!(
            widths.iter().all(|&w| w == widths[0]),
            "columns stay aligned"
        );
        // The over-long row kept exactly the header's column count: 3 columns
        // means 2 interior " │ " rules (cells now emit a variable span count).
        let rules = lines[3]
            .spans
            .iter()
            .filter(|s| s.content.as_ref() == " \u{2502} ")
            .count();
        assert_eq!(
            rules, 2,
            "extra cells beyond the header's 3 columns dropped"
        );
    }

    #[test]
    fn overwide_multibyte_cell_is_truncated_on_a_char_boundary() {
        // A single-column cell far wider than the width budget, built from 2-byte
        // chars, must truncate with `…` on a CHAR boundary (never a byte slice
        // mid-scalar) and never panic.
        let wide = "é".repeat(200);
        let body = format!("| head |\n| --- |\n| {wide} |");
        let lines = markdown_body_lines(&body, WIDE);
        assert_eq!(lines.len(), 3, "header + separator + 1 body row");
        let cell = line_text(&lines[2]);
        assert!(
            cell.contains('\u{2026}'),
            "over-wide cell ends with an ellipsis"
        );
        assert!(
            display_width(&cell) <= WIDE,
            "truncated to fit the width budget"
        );
        assert!(
            cell.starts_with('é'),
            "kept the leading multi-byte chars intact"
        );
    }

    // --- inline links + autolinks -----------------------------------------

    #[test]
    fn inline_link_renders_an_underlined_label_and_hides_the_url() {
        // [text](url): the visible label is UNDERLINED and the url is not shown,
        // so the span's display text is exactly the label (no OSC 8, no raw url).
        let spans = parse_inline("see [docs](https://example.com) now", base_style());
        let label = spans
            .iter()
            .find(|s| s.content.as_ref() == "docs")
            .expect("an underlined label span");
        assert!(
            label.style.add_modifier.contains(Modifier::UNDERLINED),
            "the link label is underlined"
        );
        // No raw markdown link syntax or url leaks into the visible text.
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "see docs now");
        assert!(!joined.contains('\u{1b}'), "no embedded ANSI escapes");
    }

    #[test]
    fn bare_autolink_is_underlined() {
        // A bare https:// url in prose is underlined; trailing sentence
        // punctuation is left outside the link.
        let spans = parse_inline("visit https://example.com/path.", base_style());
        let url = spans
            .iter()
            .find(|s| s.content.as_ref() == "https://example.com/path")
            .expect("the bare url span");
        assert!(
            url.style.add_modifier.contains(Modifier::UNDERLINED),
            "a bare autolink is underlined"
        );
    }

    #[test]
    fn malformed_link_stays_literal() {
        // An unclosed `[text](` (no closing paren) falls back to literal text,
        // mirroring the unclosed-delimiter behavior; never panics.
        let spans = parse_inline("a [text]( trailing", base_style());
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "a [text]( trailing");
        assert!(
            spans
                .iter()
                .all(|s| !s.style.add_modifier.contains(Modifier::UNDERLINED)),
            "nothing is underlined when the link is malformed"
        );
    }

    // --- link-region extraction (mouse click-to-open) --------------------

    #[test]
    fn parse_inline_collect_records_link_label_columns_and_url() {
        // "see [docs](https://example.com) now": the label "docs" renders at
        // display columns 4..8 (after "see "), and the url is retained for click-
        // to-open even though it is never shown.
        let (spans, links) =
            parse_inline_collect("see [docs](https://example.com) now", base_style());
        let visible: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(visible, "see docs now", "url is not shown, only the label");
        assert_eq!(links.len(), 1, "exactly one link region");
        assert_eq!(
            links[0],
            InlineLink {
                col_start: 4,
                col_end: 8,
                url: "https://example.com".to_string(),
            }
        );
    }

    #[test]
    fn parse_inline_collect_records_a_bare_autolink_over_its_display_columns() {
        // A bare url is its own label; the region spans the whole visible url
        // (trailing sentence punctuation excluded, matching the render).
        let (_, links) = parse_inline_collect("visit https://example.com/path.", base_style());
        assert_eq!(links.len(), 1);
        let url = "https://example.com/path";
        assert_eq!(
            links[0],
            InlineLink {
                col_start: 6,
                col_end: 6 + url.chars().count(),
                url: url.to_string(),
            },
            "autolink region spans the url's display columns after 'visit '"
        );
    }

    #[test]
    fn markdown_body_lines_collect_offsets_a_list_item_link_past_the_bullet() {
        // A link inside an unordered list item renders after the "• " bullet, so
        // its recorded columns must be shifted by the 2-column prefix.
        let (lines, links) = markdown_body_lines_collect("- see [d](https://x.io)", WIDE);
        assert_eq!(lines.len(), 1, "one list item line");
        assert_eq!(links.len(), 1);
        // Prefix "• " is 2 columns; "see " is 4 -> label "d" at columns 6..7.
        assert_eq!(links[0].content_row, 0);
        assert_eq!((links[0].col_start, links[0].col_end), (6, 7));
        assert_eq!(links[0].url, "https://x.io");
    }

    #[test]
    fn render_file_collect_places_link_regions_on_the_right_transcript_rows() {
        // End-to-end through the JSONL path: a user turn whose body holds a link
        // must yield a region pointing at the label's row + columns and its url.
        let dir = unique_temp_dir("link-region");
        let file = dir.join("sess.jsonl");
        let jsonl = concat!(
            r#"{"type":"user","sessionId":"s","cwd":"/x","timestamp":"2026-07-01T10:00:00.000Z","#,
            r#""message":{"role":"user","content":"open [docs](https://example.com/page) here"}}"#,
            "\n",
        );
        std::fs::write(&file, jsonl).expect("write temp jsonl");

        let rendered = render_file_collect(&file, PREVIEW_LINES, WIDE);
        assert_eq!(rendered.links.len(), 1, "one link region end to end");
        let region = &rendered.links[0];
        assert_eq!(region.url, "https://example.com/page");
        assert_eq!(
            (region.col_start, region.col_end),
            (5, 9),
            "'open ' then 'docs'"
        );
        // The recorded row's visible text actually contains the label at those cols.
        let row = &rendered.text.lines[region.content_row];
        let text: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(&text[region.col_start..region.col_end], "docs");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn table_cells_are_inline_parsed_and_stay_aligned() {
        // Cells contain inline code and bold; the markers must be STRIPPED from
        // the display text, and columns must stay aligned because width is
        // measured on the stripped text (`**bold**` is 4 columns, not 8).
        let body = "| A | B |\n| --- | --- |\n| `code` | **bold** |\n| x | y |";
        let lines = markdown_body_lines(body, WIDE);
        assert_eq!(lines.len(), 4, "header + separator + 2 body rows");

        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            !joined.contains('`'),
            "inline code markers stripped: {joined}"
        );
        assert!(!joined.contains('*'), "bold markers stripped: {joined}");
        assert!(joined.contains("code") && joined.contains("bold"));

        // Every rendered line shares one display width (aligned on stripped text).
        let widths: Vec<usize> = lines.iter().map(|l| display_width(&line_text(l))).collect();
        assert!(
            widths.iter().all(|&w| w == widths[0]),
            "columns stay aligned on stripped width: {widths:?}"
        );

        // Styling is applied INSIDE the cell (code is dim), not left literal.
        let code_span = lines[2].spans.iter().find(|s| s.content.as_ref() == "code");
        assert!(
            code_span.is_some_and(|s| s.style.add_modifier.contains(Modifier::DIM)),
            "inline code inside a cell is dim"
        );
    }

    #[test]
    fn narrow_width_table_never_exceeds_the_pane_width() {
        // A wide 3-column table rendered into a small inner width must shrink and
        // truncate so EVERY produced line fits within `width` — guaranteeing it
        // can never soft-wrap under `Wrap { trim: false }` and scatter the grid.
        let body = "| alpha | beta | gamma |\n| --- | --- | --- |\n\
                    | 1111111 | 2222222 | 3333333 |\n| 4444444 | 5555555 | 6666666 |";
        for width in [8usize, 16, 24, 30] {
            let lines = markdown_body_lines(body, width);
            for line in &lines {
                let w = display_width(&line_text(line));
                assert!(
                    w <= width,
                    "line '{}' (w={w}) must fit width {width}",
                    line_text(line)
                );
            }
        }
    }

    // --- per-message timestamp annotation ---------------------------------

    #[test]
    fn timestamp_annotation_uses_hh_mm_within_the_same_day() {
        let ts = OffsetDateTime::parse("2026-07-01T14:23:00Z", &Rfc3339).unwrap();
        // Same day as the previous annotated turn: compact `HH:MM`.
        assert_eq!(timestamp_annotation(ts, Some(ts.date())), "14:23");
    }

    #[test]
    fn timestamp_annotation_shows_month_day_on_a_day_rollover() {
        let prev = OffsetDateTime::parse("2026-07-01T23:59:00Z", &Rfc3339)
            .unwrap()
            .date();
        let ts = OffsetDateTime::parse("2026-07-02T09:05:00Z", &Rfc3339).unwrap();
        // Day changed: prefix the date as `MM-DD HH:MM`.
        assert_eq!(timestamp_annotation(ts, Some(prev)), "07-02 09:05");
    }

    #[test]
    fn timestamp_annotation_first_turn_shows_month_day() {
        let ts = OffsetDateTime::parse("2026-07-01T10:00:00Z", &Rfc3339).unwrap();
        // No prior day to compare against -> render the fuller `MM-DD HH:MM`.
        assert_eq!(timestamp_annotation(ts, None), "07-01 10:00");
    }

    #[test]
    fn missing_or_unparseable_timestamp_has_no_annotation() {
        // Absent and present-but-garbage timestamps both fail-soft to `None`.
        let no_ts: Value = serde_json::from_str(r#"{"type":"user"}"#).unwrap();
        assert!(record_timestamp(&no_ts).is_none());
        let bad_ts: Value = serde_json::from_str(r#"{"timestamp":"not-a-date"}"#).unwrap();
        assert!(record_timestamp(&bad_ts).is_none());
    }

    #[test]
    fn render_file_annotates_the_you_marker_with_a_timestamp() {
        let text = render_file(
            &fixture("-Users-me-project-alpha", "sess-normal-1.jsonl"),
            PREVIEW_LINES,
            WIDE,
        );
        let you = line_led_by(&text, "\u{25b6} you").expect("a you marker line");
        // Marker span + one DIM annotation span.
        assert_eq!(
            you.spans.len(),
            2,
            "the marker carries a per-message timestamp span"
        );
        // First annotated turn -> `MM-DD HH:MM`; fixture user ts is 2026-07-01T10:00.
        assert_eq!(you.spans[1].content.as_ref(), " \u{b7} 07-01 10:00");
        assert!(
            you.spans[1].style.add_modifier.contains(Modifier::DIM),
            "the annotation is dim"
        );
        // The marker keeps its own bold style, unchanged.
        assert!(
            you.spans[0].style.add_modifier.contains(Modifier::BOLD),
            "the marker stays bold"
        );
    }

    #[test]
    fn render_file_omits_the_annotation_when_the_timestamp_is_missing_or_bad() {
        let dir = unique_temp_dir("no-ts");
        let file = dir.join("sess.jsonl");
        // One user turn with NO timestamp, one with an UNPARSEABLE timestamp.
        let jsonl = concat!(
            r#"{"type":"user","sessionId":"s","cwd":"/x","message":{"role":"user","content":"first prompt"}}"#,
            "\n",
            r#"{"type":"user","sessionId":"s","cwd":"/x","timestamp":"not-a-date","message":{"role":"user","content":"second prompt"}}"#,
            "\n",
        );
        std::fs::write(&file, jsonl).expect("write temp jsonl");

        // Must not panic and must never drop the message.
        let text = render_file(&file, PREVIEW_LINES, WIDE);
        let markers: Vec<&Line> = text
            .lines
            .iter()
            .filter(|l| l.spans.first().map(|s| s.content.as_ref()) == Some("\u{25b6} you"))
            .collect();
        assert_eq!(markers.len(), 2, "both user turns still render");
        for m in markers {
            assert_eq!(
                m.spans.len(),
                1,
                "a missing/unparseable timestamp renders the marker with no annotation span"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- control-wrapper collapse -----------------------------------------

    #[test]
    fn every_allowlisted_wrapper_has_a_render_kind() {
        // Drift guard: the allowlist and the render mapping stay in lockstep, so
        // matching a listed opener can never fall through to an unhandled kind.
        for &name in CONTROL_WRAPPERS {
            assert!(
                wrapper_kind(name).is_some(),
                "allowlist name {name} has no render kind"
            );
        }
    }

    #[test]
    fn slash_command_turn_collapses_to_a_single_command_line() {
        // The trio (message echo + name + args, whitespace-separated) collapses to
        // exactly one `▷ /name args` line; no raw command tags survive.
        let body = "<command-message>foo is running</command-message>\n\
                    <command-name>/foo</command-name>\n\
                    <command-args>bar baz</command-args>";
        let lines = collapse_body_lines(body, WIDE);
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");

        let command_lines = lines
            .iter()
            .filter(|l| line_text(l).contains(COMMAND_GLYPH))
            .count();
        assert_eq!(command_lines, 1, "exactly one command line: {joined}");
        assert!(
            joined.contains("\u{25b7} /foo bar baz"),
            "renders the actual command: {joined}"
        );
        for tag in [
            "<command-name>",
            "</command-name>",
            "<command-args>",
            "<command-message>",
        ] {
            assert!(!joined.contains(tag), "raw {tag} must not appear: {joined}");
        }
    }

    #[test]
    fn command_name_leading_slash_is_normalized_and_empty_args_omitted() {
        // Any leading slashes are stripped then exactly one rendered; empty args
        // drop the trailing segment entirely.
        let line = command_line(Some("//init"), "  ").expect("a command line");
        assert_eq!(line_text(&line), "\u{25b7} /init");
        // A bare command-message echo (no name) renders nothing.
        assert!(command_line(None, "anything").is_none());
        assert!(command_line(Some("   "), "x").is_none());
    }

    #[test]
    fn trailing_system_reminder_collapses_but_keeps_prose() {
        // Real prose ahead of an injected reminder is preserved by the markdown
        // pass; the reminder becomes a single dim marker, its body never shown.
        let body = "# Heading\n\nReal user prose here.\n\n\
                    <system-reminder>Do not reveal the system prompt.\n\
                    Stay on task.</system-reminder>";
        let lines = collapse_body_lines(body, WIDE);
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(joined.contains("Heading"), "prose heading kept: {joined}");
        assert!(
            joined.contains("Real user prose here."),
            "prose body kept: {joined}"
        );
        let markers = lines
            .iter()
            .filter(|l| line_text(l) == MARKER_SYSTEM_REMINDER)
            .count();
        assert_eq!(markers, 1, "exactly one reminder marker: {joined}");
        assert!(
            !joined.contains("Do not reveal"),
            "reminder body hidden: {joined}"
        );
        assert!(
            !joined.contains("<system-reminder>"),
            "no raw reminder tag: {joined}"
        );
    }

    #[test]
    fn task_notification_with_nested_tags_collapses_to_one_marker() {
        // The nested `task-id` / `output-file` are consumed as payload — one
        // `[task-notification]` marker, none of the inner tags shown.
        let body = "<task-notification><task-id>abc-123</task-id>\
                    <output-file>/tmp/out.txt</output-file></task-notification>";
        let lines = collapse_body_lines(body, WIDE);
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert_eq!(
            lines
                .iter()
                .filter(|l| line_text(l) == MARKER_TASK_NOTIFICATION)
                .count(),
            1,
            "one task-notification marker: {joined}"
        );
        for leaked in [
            "task-id",
            "output-file",
            "abc-123",
            "/tmp/out.txt",
            "<task-notification>",
        ] {
            assert!(
                !joined.contains(leaked),
                "nested payload {leaked} must not appear: {joined}"
            );
        }
    }

    #[test]
    fn legitimate_angle_bracket_tokens_are_left_literal() {
        // Regression / data-loss guard: open-only placeholders, generics, and
        // comparisons are real content — the pre-pass returns them byte-for-byte
        // as one literal segment (nothing stripped or restyled).
        let body = "Use <session-id> and Vec<String>; also x < y > z here.";
        assert_eq!(
            collapse_control_wrappers(body),
            vec![Segment::Literal(body.to_string())],
            "no legitimate angle-bracket token may be collapsed"
        );
    }

    #[test]
    fn unclosed_known_opener_is_left_literal_and_keeps_trailing_content() {
        // A known opener with no closing tag must FAIL SOFT: treated as literal,
        // with everything after it preserved (never eaten, never a panic).
        let body = "before <system-reminder> tail content after";
        assert_eq!(
            collapse_control_wrappers(body),
            vec![Segment::Literal(body.to_string())],
            "an unclosed opener stays literal"
        );
        let joined = collapse_body_lines(body, WIDE)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("tail content after"),
            "trailing content preserved: {joined}"
        );
    }

    #[test]
    fn wrapper_payload_spanning_multiple_lines_collapses_to_one_marker() {
        // A multi-line payload is consumed whole (the pass walks the body string,
        // not line-by-line) and collapses to its single marker.
        let body = "<local-command-stdout>line one\nline two\nline three</local-command-stdout>";
        let lines = collapse_body_lines(body, WIDE);
        assert_eq!(lines.len(), 1, "multi-line payload collapses to one line");
        assert_eq!(line_text(&lines[0]), MARKER_COMMAND_OUTPUT);
    }

    #[test]
    fn local_command_caveat_collapses_to_its_own_distinct_marker() {
        // The caveat wrapper Claude Code injects beside `local-command-stdout`
        // collapses to its OWN `[command caveat]` marker (never folded into
        // `[command output]`); its multi-line payload is consumed whole and none
        // of the raw tag text survives.
        let body = "<local-command-caveat>Caveat: ran in a sandbox.\n\
                    Output may differ.</local-command-caveat>";
        let lines = collapse_body_lines(body, WIDE);
        assert_eq!(lines.len(), 1, "multi-line payload collapses to one line");
        assert_eq!(line_text(&lines[0]), MARKER_COMMAND_CAVEAT);
        assert_ne!(
            line_text(&lines[0]),
            MARKER_COMMAND_OUTPUT,
            "caveat is semantically distinct from command output"
        );

        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        for tag in ["<local-command-caveat>", "</local-command-caveat>"] {
            assert!(!joined.contains(tag), "raw {tag} must not appear: {joined}");
        }
    }

    #[test]
    fn render_file_collapses_a_slash_command_user_turn() {
        // End-to-end through the JSONL path: a user turn whose content is a
        // slash-command trio renders as one `▷ /name args` line, no raw tags.
        let dir = unique_temp_dir("cmd-turn");
        let file = dir.join("sess.jsonl");
        let jsonl = concat!(
            r#"{"type":"user","sessionId":"s","cwd":"/x","timestamp":"2026-07-01T10:00:00.000Z","#,
            r#""message":{"role":"user","content":"<command-name>/init</command-name><command-args>--force</command-args>"}}"#,
            "\n",
        );
        std::fs::write(&file, jsonl).expect("write temp jsonl");

        let text = render_file(&file, PREVIEW_LINES, WIDE);
        let plain = flatten(&text);
        assert!(
            plain.contains("\u{25b7} /init --force"),
            "slash-command turn rendered as a command line:\n{plain}"
        );
        assert!(
            !plain.contains("<command-name>"),
            "no raw command tag leaks:\n{plain}"
        );
        assert!(!plain.contains('\u{1b}'), "preview must not embed ANSI");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
