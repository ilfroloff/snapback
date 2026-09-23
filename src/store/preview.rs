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
//! The `claude` separator also carries the BOUND agent handle in effect at that
//! turn (`● claude · @lead · sonnet-5 · 12:55`), read from two record types:
//! `agent-setting` (the interactive bind — a clean handle, authoritative) and
//! `agent-name` (the background job's name, a fallback trusted only when it names a
//! KNOWN defined agent, since that field also carries free-form job titles).
//! Attribution is POSITIONAL — the agent is threaded as streaming state (exactly
//! like the per-message day rollover), so a turn shows the agent set *before* it,
//! and a late record never retroactively labels earlier turns.
//!
//! It carries the ANSWERING MODEL the same way, and positional for the same reason
//! — but with no threading at all, because `message.model` sits on the very record
//! being rendered ([`record_model`]). A session that switches model mid-file
//! therefore labels each turn with its own, which real sessions do. `<synthetic>`,
//! the pseudo-model Claude Code stamps on records it injects itself, is suppressed
//! by [`model_label`], as is the total absence of a model (the common case).
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
//! The WHOLE transcript is rendered — there is no tail cap — and the caller caches
//! the result per session id, so markdown parsing never stalls the UI on a large
//! transcript. The cap that used to keep only the most-recent 600 rendered lines
//! existed to bound the DRAW, which re-wrapped everything it was handed on every
//! frame; the pane now draws a window of the rows its viewport can reach
//! (`tui::view::row_window`), so the cap bought nothing but a truncated transcript.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::Session;

/// Floor on a GFM table column's rendered width in GRID mode — and, because a
/// grid that cannot seat every column at its floor is abandoned for the stacked
/// record layout, the LAYOUT SWITCH between the two (see [`render_table`]).
///
/// Now that an over-wide cell WRAPS instead of being cut ([`wrap_spans`]), a
/// column narrower than this turns its text vertical — one or two chars per
/// line — which reads worse than no grid at all. Pinned at 10: a typical
/// transcript cell is a short phrase of ~20 columns, so 10 still seats one or
/// two words per line.
///
/// It is the switch point, not merely a floor, so retuning it moves ordinary
/// tables between the two layouts: at a 62-column preview (a 120-column terminal
/// split down the middle) a 5-column table of wide cells fits EXACTLY
/// (`5 * 10 + 4 * 3 = 62`) and a 6-column one falls back to records. A column
/// whose NATURAL width is already below the floor only ever costs its natural
/// width, so a table of short cells is never dumped into records for want of
/// room it would not have used.
const TABLE_MIN_COL_WIDTH: usize = 10;

/// Width of the DIM `─` rule that separates two stacked records in the
/// narrow-pane table fallback (see [`render_table_records`]).
///
/// The record layout has no grid to take a width from, so its one piece of chrome
/// needs a width of its own, and that width is FIXED — deliberately NOT clamped to
/// the pane. A record line is an ordinary logical line that the preview's own
/// `Wrap { trim: false }` wraps, and re-introducing a pane clamp here is the one
/// place this layout would start cutting again.
///
/// Fixed therefore means the rule OVERFLOWS a narrower pane and soft-wraps into
/// several rows of dashes: one row at 32 columns and up, two from 16 through 31,
/// four at 8. Those are squarely the panes the fallback serves — it fires only
/// once a floor-width grid no longer fits, i.e. below 36 columns for a 3-column
/// table of wide cells (`3 * 10 + 2 * 3`) and below 62 for a 5-column one — so a
/// multi-row separator is the NORMAL case here, not an edge one. That is accepted
/// rather than worked around because the rule is DECORATIVE: a wrapped separator
/// still reads as a separator, costs only rows, and loses no transcript text,
/// whereas the obvious repair — capping it to the pane — is exactly the cut this
/// layout exists to remove.
///
/// Pinned at 32 to bound both ends of that trade: wide enough to read as a rule
/// rather than a stray dash, and short enough that even at the narrowest panes the
/// wrap costs a few rows rather than a screenful (a 64-wide rule would cost eight
/// rows at width 8 where this one costs four).
const RECORD_RULE_WIDTH: usize = 32;

/// A clickable link inside the rendered preview, in CONTENT coordinates (before
/// the preview's soft-wrap is applied at draw time).
///
/// The preview renders a link's label UNDERLINED and DISCARDS its url from the
/// visible text (no OSC 8, no raw url — see [`parse_inline_collect`]). This
/// records where that label lives so the app's own mouse handling can recover the
/// url on a click: `content_row` indexes into the returned [`Text`]'s lines, and
/// `col_start..col_end` is the label's DISPLAY-column span on that line. Columns
/// depend on the render `width` (GFM tables shrink, wrap, and may change layout
/// entirely), so regions are cached TOGETHER with the `Text` under the same width
/// discipline (see [`App`]).
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

/// One turn's marker `Line`, addressed by content row in the SAME coordinate
/// space as [`LinkRegion::content_row`] (an index into the rendered [`Text`]'s
/// lines, rebased onto the whole transcript as each block is appended — there is
/// no tail cap left to rebase it a second time, see [`render_file_collect`]).
///
/// Captured so the preview pane's PINNED row can show the marker of whichever turn
/// is scrolled to the top of the viewport — in EVERY scroll state, including the
/// default bottom-anchored one (see [`crate::tui::view::render_preview`]) — reusing
/// a `Line` that is ALREADY rendered elsewhere in the transcript rather than
/// deriving a second, divergent copy of the marker text (no first-line synopsis,
/// no tool-only-turn fallback — the marker line verbatim, or nothing).
#[derive(Debug, Clone)]
pub struct MarkerLine {
    /// Line index into the rendered [`Text`] this marker sits on.
    pub content_row: usize,
    /// The turn's marker line verbatim — richest form first, every annotation
    /// being independently optional (see [`marker_line_with_time`]): e.g.
    /// `● claude · @lead · sonnet-5 · 12:55` or `▶ you · 14:23`.
    pub line: Line<'static>,
}

/// A rendered transcript preview: the styled [`Text`] plus the clickable
/// [`LinkRegion`]s and turn [`MarkerLine`]s discovered while building it. All
/// three are produced from one pass at a fixed `width`, so a region's columns —
/// and a marker's content row — always match the text as drawn.
#[derive(Debug, Default)]
pub struct RenderedPreview {
    /// The styled, markdown-rendered transcript.
    pub text: Text<'static>,
    /// Clickable link regions, in content coordinates (see [`LinkRegion`]).
    pub links: Vec<LinkRegion>,
    /// Every turn's marker line, in content-row (i.e. file) order (see
    /// [`MarkerLine`]).
    pub markers: Vec<MarkerLine>,
}

/// Render a session's transcript for the preview pane, fitting GFM tables to
/// `width` (the preview pane's inner content width, in columns). Returns the
/// styled text together with the clickable link regions found within it.
///
/// `known_agents` is the set of DEFINED agent names (`~/.claude/agents/*.md`); it
/// gates the noisy `agent-name` fallback so a free-form background-job title never
/// renders as a bogus handle (see [`render_record`]).
pub fn render(session: &Session, width: usize, known_agents: &HashSet<&str>) -> RenderedPreview {
    render_file_collect(&session.file, width, known_agents)
}

/// The optimistic trailing turns shown in the preview while a quick-reply send is
/// in flight, so the message you just sent — and the fact that claude is working on
/// it — appear immediately, before claude has written either to disk.
///
/// `echo_message` is `Some(text)` only while the sent user turn is NOT yet on disk
/// (the caller gates this on the session's turn count). The echoed `▶ you` turn is
/// styled and wrapped exactly like a real one — same marker, same markdown body
/// pass — so the instant claude appends the real turn the caller passes `None` and
/// the swap is seamless (never a doubled line). `working_label` (e.g. `⠙ cooking…`)
/// is the live spinner + phase word, always shown as claude's pending turn: a DIM
/// placeholder, like `[thinking]`, until the send completes.
///
/// Appended to the on-disk transcript by the view; the reply itself still renders
/// through the ordinary watcher → reload → [`render`] path once on disk. Pure (no
/// I/O, no `App`), so the shape is unit-testable without a terminal.
#[must_use]
pub fn pending_reply_turns(
    echo_message: Option<&str>,
    working_label: &str,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if let Some(message) = echo_message {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            YOU_MARKER.to_string(),
            you_style(),
        )));
        // Same body pass as a real user turn, so the echo wraps and styles
        // identically to the turn that will replace it.
        lines.extend(collapse_body_lines_collect(message, width).0);
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        CLAUDE_MARKER.to_string(),
        claude_style(),
    )));
    lines.push(Line::from(Span::styled(
        working_label.to_string(),
        marker_style(),
    )));
    lines
}

/// Render `path` into a [`RenderedPreview`]: the styled transcript plus the
/// clickable [`LinkRegion`]s and the turn [`MarkerLine`]s, over the WHOLE file.
///
/// Each record's block contributes its lines, its (block-relative) link regions,
/// and — for a "user"/"assistant"/"summary" record — its (block-relative)
/// [`MarkerLine`]; all are rebased onto the growing transcript by the running line
/// offset so a region's `content_row` (and a marker's) addresses the FINAL text.
/// That running rebase is now the ONLY one: a tail cap used to drop everything
/// above the last 600 rendered lines and shift every surviving region up by the
/// same amount, so a long conversation's early turns simply were not in the preview
/// and a link above the cut was dropped. Nothing needs the cap any more — the pane
/// draws a window of the rows its viewport can reach rather than re-wrapping the
/// whole transcript per frame — so the transcript arrives whole and a region, like a
/// marker, keeps the row it was rendered on.
///
/// That the cap is gone is what makes the marker list TOTAL: every turn in the file
/// has a marker at a live row, so the banner's lookup can never miss because the
/// turn it wanted was cut off the top.
fn render_file_collect(path: &Path, width: usize, known_agents: &HashSet<&str>) -> RenderedPreview {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => {
            return RenderedPreview {
                text: Text::from(format!("No such session file:\n{}", path.display())),
                links: Vec::new(),
                markers: Vec::new(),
            }
        }
    };
    let reader = BufReader::new(file);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut links: Vec<LinkRegion> = Vec::new();
    let mut markers: Vec<MarkerLine> = Vec::new();
    // Day of the previously ANNOTATED turn, threaded through the loop so a
    // per-message timestamp can switch to `MM-DD HH:MM` on a day rollover.
    let mut prev_day: Option<Date> = None;
    // Bound agent in effect at this point in the file: `agent-setting` / `agent-name`
    // records set it as they stream past, and each assistant turn is labeled with it.
    // POSITIONAL — never a file-level hoist — so a late record cannot retroactively
    // label the turns that precede it.
    let mut agent = AgentState::default();
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
        if let Some((block, block_links, block_marker)) =
            render_record(&record, &mut agent, known_agents, &mut prev_day, width)
        {
            let offset = lines.len();
            links.extend(rebased(block_links, offset));
            if let Some(mut marker) = block_marker {
                marker.content_row += offset;
                markers.push(marker);
            }
            lines.extend(block);
        }
    }

    RenderedPreview {
        text: Text::from(lines),
        links,
        markers,
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
///
/// `agent` carries the bound agent in effect at this point in the file: an
/// `agent-setting` or (validated) `agent-name` record updates it — contributing no
/// lines — and each assistant turn is labeled with its [`effective`] value.
/// `known_agents` gates the `agent-name` fallback. State is threaded — never
/// hoisted — so attribution is positional; see the module doc.
///
/// The third tuple element is the block's own [`MarkerLine`] (block-relative
/// `content_row`, rebased by the caller like the links) for a "summary",
/// "user", or "assistant" record — the ONE line the pinned preview banner can
/// show when the user scrolls this turn to the top of the viewport
/// ([`crate::tui::view::render_preview`]) — `None` for a record that
/// contributes lines but is not itself a turn (there is none today, but the
/// shape stays honest for one that might).
///
/// [`effective`]: AgentState::effective
fn render_record(
    record: &Value,
    agent: &mut AgentState,
    known_agents: &HashSet<&str>,
    prev_day: &mut Option<Date>,
    width: usize,
) -> Option<(Vec<Line<'static>>, Vec<LinkRegion>, Option<MarkerLine>)> {
    match record.get("type").and_then(Value::as_str) {
        Some("summary") => {
            let s = record.get("summary").and_then(Value::as_str)?;
            // Keep the literal `# summary` head, now styled as a heading. No links.
            let marker =
                marker_line_with_time(format!("# {s}"), summary_style(), None, record, prev_day);
            let lines = vec![marker.clone()];
            Some((
                lines,
                Vec::new(),
                Some(MarkerLine {
                    content_row: 0,
                    line: marker,
                }),
            ))
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
            let marker =
                marker_line_with_time(YOU_MARKER.to_string(), you_style(), None, record, prev_day);
            let mut lines = vec![Line::from(""), marker.clone()];
            // Body links are relative to the body; rebase them past the blank +
            // marker lines that lead every turn. The marker itself sits at row 1
            // (the blank line at row 0 leads every turn).
            let offset = lines.len();
            let (body, body_links) = collapse_body_lines_collect(&text, width);
            lines.extend(body);
            Some((
                lines,
                rebased(body_links, offset),
                Some(MarkerLine {
                    content_row: 1,
                    line: marker,
                }),
            ))
        }
        Some("assistant") => {
            let content = record.get("message").and_then(|m| m.get("content"))?;
            let (body, body_links) = assistant_lines(content, width);
            if body.is_empty() {
                return None;
            }
            let marker = marker_line_with_time(
                CLAUDE_MARKER.to_string(),
                claude_style(),
                agent.effective(),
                record,
                prev_day,
            );
            let mut lines = vec![Line::from(""), marker.clone()];
            let offset = lines.len();
            lines.extend(body);
            Some((
                lines,
                rebased(body_links, offset),
                Some(MarkerLine {
                    content_row: 1,
                    line: marker,
                }),
            ))
        }
        Some("agent-setting") => {
            // Positional state, not a rendered line: record the interactive BIND in
            // effect from here on so later assistant turns can be labeled. Read
            // FAIL-SOFT — a missing / null / non-string `agentSetting` (or a blank
            // one) clears the bind rather than panicking. `"claude"` is stored
            // as-is (a later default re-emission must reset the bind); the
            // `● claude · @claude` suppression is `agent_handle`'s job at render.
            // Contributes no lines, so the link rebasing is untouched.
            agent.bound = trimmed_field(record, "agentSetting");
            None
        }
        Some("agent-name") => {
            // The background job's display NAME — a fallback source for the bound
            // agent, but the SAME field also carries free-form job titles (e.g.
            // "audit high severity errors"), so it is trusted ONLY when it names a
            // known DEFINED agent; a title clears the fallback rather than rendering
            // as a bogus `@handle`. `agent-setting` still wins over this (see
            // `AgentState::effective`). Read FAIL-SOFT; contributes no lines.
            agent.job = trimmed_field(record, "agentName")
                .filter(|name| known_agents.contains(name.as_str()));
            None
        }
        _ => None,
    }
}

/// A record's string `key`, trimmed, or `None` when it is absent / null /
/// non-string / blank. The one FAIL-SOFT reader both agent records share.
fn trimmed_field(record: &Value, key: &str) -> Option<String> {
    record
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The bound agent in effect at a point in the transcript, tracked from the two
/// records that can name it. `agent-setting` (the interactive bind) is
/// authoritative; `agent-name` (the background job's name) is a fallback trusted
/// only when it names a known agent, since that same field also carries free-form
/// job titles. Both are streaming positional state (like the day rollover), never
/// a file-level hoist.
#[derive(Default)]
struct AgentState {
    /// Latest `agentSetting` — a clean handle, stored verbatim (incl. `"claude"`).
    bound: Option<String>,
    /// Latest KNOWN `agentName` — validated against the defined-agent set; a
    /// free-form title leaves this `None`.
    job: Option<String>,
}

impl AgentState {
    /// The agent to label a turn with: the interactive bind wins; else the
    /// validated job name. (`agent_handle` still suppresses the default/blank.)
    fn effective(&self) -> Option<&str> {
        self.bound.as_deref().or(self.job.as_deref())
    }
}

/// The catch-all default agent. Claude Code writes `agentSetting: "claude"` when
/// no specialized agent is bound, and the `● claude` marker already names it, so
/// rendering `● claude · @claude` would be pure noise — [`agent_handle`] suppresses
/// it. (NO MAGIC VALUES: the default is named here, not spelled inline.)
const DEFAULT_AGENT: &str = "claude";

/// The DIM `@handle` to render for the bound agent, or `None` when there is nothing
/// worth showing: the name is absent, empty/whitespace-only, or the catch-all
/// [`DEFAULT_AGENT`] (already implied by the `● claude` marker). This is the SINGLE
/// place the render-suppression decision lives. PURE — see the unit test.
fn agent_handle(agent: Option<&str>) -> Option<String> {
    let name = agent?.trim();
    if name.is_empty() || name == DEFAULT_AGENT {
        return None;
    }
    Some(format!("@{name}"))
}

/// The vendor prefix every Claude Code model id carries (`claude-sonnet-5`). The
/// whole board is Claude, so repeating the vendor on every turn is noise — it is
/// the ONLY thing [`model_label`] strips. (NO MAGIC VALUES: named here, like
/// [`DEFAULT_AGENT`], never spelled inline.) Ids WITHOUT it (the bare `opus` /
/// `sonnet` aliases that also appear on disk) are rendered unchanged.
const MODEL_VENDOR_PREFIX: &str = "claude-";

/// The model label to render for a turn — the model that actually ANSWERED it —
/// or `None` when there is nothing worth showing. This is the SINGLE place that
/// suppression decision lives, mirroring [`agent_handle`]. PURE — see the unit test.
///
/// Suppressed: an absent / null / non-string `message.model` (all of which reach
/// here as `None` — see [`record_model`]) and a blank one; plus any `<…>`-wrapped
/// PSEUDO-model. `<synthetic>` is the one that matters: Claude Code stamps it on
/// records it injects ITSELF (a session-limit or auth notice), so it names no model
/// that answered anything, and it is the LAST assistant model in real sessions —
/// exactly where a naive "latest model" label would render it.
///
/// Otherwise the id is LOSSLESS beyond [`MODEL_VENDOR_PREFIX`]: `claude-sonnet-5`
/// reads `sonnet-5`, and a dated id keeps its date (`claude-haiku-4-5-20251001` ->
/// `haiku-4-5-20251001`) rather than being truncated into an ambiguous one.
///
/// # Two surfaces, one rule
///
/// `pub(crate)` rather than private because it now answers for BOTH places a model
/// id reaches the screen: the DIM preview marker here, and the quick-reply
/// completion status built from `modelUsage`
/// ([`crate::send::status_for_send`]). Those two channels exist to be COMPARED —
/// what the transcript recorded against what the send reported — so they must
/// shorten and suppress identically or a difference in spelling would read as a
/// difference in model. Hence one owner, exported, rather than a sibling copy.
#[must_use]
pub(crate) fn model_label(model: Option<&str>) -> Option<String> {
    let id = model?.trim();
    if id.is_empty() {
        return None;
    }
    // Any `<…>`-wrapped id is a PSEUDO-model, not a model that answered anything.
    // Matched by SHAPE rather than by listing `<synthetic>`, so a future sibling
    // pseudo-model is suppressed the day it appears instead of leaking to the board.
    if id.starts_with('<') && id.ends_with('>') {
        return None;
    }
    let label = id.strip_prefix(MODEL_VENDOR_PREFIX).unwrap_or(id);
    // A bare prefix and nothing else is malformed; render no annotation rather
    // than an empty ` · `.
    if label.is_empty() {
        return None;
    }
    Some(label.to_string())
}

/// One DIM ` · <text>` marker annotation. BOTH the bound-agent handle and the
/// per-message timestamp render through this ONE builder, so they share a single
/// ` · ` separator convention and DIM style and cannot drift apart (DRY).
fn annotation_span(text: &str) -> Span<'static> {
    Span::styled(format!(" \u{b7} {text}"), marker_style())
}

/// Build a turn-marker line, appending — in order — a DIM `@agent` handle (when a
/// non-default `agent` is in effect), a DIM answering-model label (when THIS record
/// carries a showable [`record_model`]), then a DIM per-message timestamp annotation
/// (e.g. ` · 14:23`, when THIS record carries a parseable RFC 3339 `timestamp`), so
/// a bound assistant turn reads `● claude · @lead · sonnet-5 · 12:55`.
///
/// The marker span keeps its own (bold) style unchanged; only the trailing
/// annotations are DIM. FAIL-SOFT: a missing or unparseable timestamp renders the
/// marker with no timestamp annotation and leaves `prev_day` untouched; a suppressed
/// or absent agent renders no handle; a suppressed or absent model renders no label.
/// On a timestamp success `prev_day` advances to this record's day so the next
/// annotated turn can detect a rollover.
fn marker_line_with_time(
    marker: String,
    style: Style,
    agent: Option<&str>,
    record: &Value,
    prev_day: &mut Option<Date>,
) -> Line<'static> {
    let mut spans = vec![Span::styled(marker, style)];
    if let Some(handle) = agent_handle(agent) {
        spans.push(annotation_span(&handle));
    }
    if let Some(label) = model_label(record_model(record)) {
        spans.push(annotation_span(&label));
    }
    if let Some(ts) = record_timestamp(record) {
        let annotation = timestamp_annotation(ts, *prev_day);
        *prev_day = Some(ts.date());
        spans.push(annotation_span(&annotation));
    }
    Line::from(spans)
}

/// A record's own `message.model` — the model that ANSWERED this turn, and the ONLY
/// on-disk evidence of it (no `model` RECORD type exists, so a session's model is
/// never persisted as a file-level fact). Read from the record being rendered, which
/// is what makes the label POSITIONAL for free: a session that changes model mid-file
/// labels each turn with ITS OWN model, and no late turn can relabel an earlier one.
/// In practice only `assistant` records carry the field.
///
/// FAIL-SOFT: absent / null / non-string all read as `None`, which is the NORMAL
/// case — a fifth of real sessions carry no model at all — never an error state.
fn record_model(record: &Value) -> Option<&str> {
    record
        .get("message")
        .and_then(|m| m.get("model"))
        .and_then(Value::as_str)
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

/// The `▶ you` user-turn marker (glyph + label). One source of truth for both the
/// real render ([`render_record`]) and the optimistic echo ([`pending_reply_turns`]),
/// so the two are byte-identical and the swap on disk-landing is seamless.
const YOU_MARKER: &str = "\u{25b6} you";
/// The `● claude` assistant-turn marker (glyph + label); see [`YOU_MARKER`].
const CLAUDE_MARKER: &str = "\u{25cf} claude";

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

/// The `Header: ` label of a stacked record in the narrow-pane table fallback:
/// DIM, so the label recedes and the VALUE beside it reads as the content. The
/// grid says which column a cell is in with position; without a grid the label
/// has to say it in words, and dimming is what stops those words drowning the
/// data. Same restraint (a `Modifier`, no fixed color) as the borders above.
fn record_label_style() -> Style {
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
/// verbatim, and GFM table cells are inline-parsed but their link columns are
/// still NOT recorded, so a table-cell link is never a wrong hit.
///
/// The reason is the LAYOUT, not truncation — nothing in a table is cut any more
/// (see [`render_table`]). A cell's label no longer sits at one known column:
/// grid mode wraps a cell down its column, so the label may start on any of the
/// row's visual lines at a per-line offset the padding shifts, and record mode
/// drops the grid entirely and re-emits the cell behind a `Header: ` prefix on a
/// line the pane then soft-wraps. Mapping either back to a `(content_row,
/// col_start..col_end)` region is a second, layout-aware pass; until it exists,
/// recording nothing keeps the promise that a click never opens the wrong url.
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
// timestamps and search-highlight keep working unchanged.
//
// Column widths are measured on each cell's MARKER-STRIPPED display text via
// `unicode-width` (see `cell_display_width`): `**x**` and `[a](b)` occupy their
// RENDERED column count (1 and 1), not their raw byte/char length, so inline
// styling inside cells cannot skew the grid. CJK/emoji "double-width" cells also
// measure at their true two columns.
//
// The whole table is fit to the preview pane's inner content `width`, and NOTHING
// IS EVER CUT to do it — the pane scrolls, so horizontal loss is paid as vertical
// cost instead. There are two layouts and `render_table` chooses between them:
//
// * GRID (the default) shrinks the columns to fit, then WRAPS an over-wide cell
//   down its own column (`wrap_spans`), so a row is as many visual lines as its
//   tallest cell needs. Columns never shrink past `TABLE_MIN_COL_WIDTH`, because
//   a narrower one would wrap its text into a vertical stack of single chars.
//   Because a row is no longer one line, the SAME `─┼─` rule that sits under the
//   header is also drawn between adjacent body rows — without it two multi-line
//   rows run together into one block with no visible boundary.
// * RECORDS take over when even that floor-width grid cannot fit the pane — a
//   narrow pane against many columns. Each row is stacked as `Header: value`
//   lines with no grid at all, and those lines are left LONG for the pane's own
//   `Wrap { trim: false }` to handle.
//
// The two differ on the no-wrap guarantee, and deliberately: every grid line is
// clamped to `width` so the grid can never soft-wrap and scatter, while a record
// line is MEANT to overflow and soft-wrap like any prose paragraph.

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

/// The marker-STRIPPED display text of a run of spans (their visible content
/// joined), so a stacked record's label reads `Beta` rather than `**Beta**`.
fn spans_display_text(spans: &[Span<'static>]) -> String {
    spans.iter().map(|s| s.content.as_ref()).collect()
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

/// Word-wrap parsed cell `spans` to at most `width` display columns, returning ONE
/// span run per visual line. The sibling of [`truncate_spans`], and its opposite:
/// where that one CUTS at the column budget, this one spends vertical space, so a
/// cell too wide for its column costs LINES rather than characters (see
/// [`render_table_grid`]).
///
/// Breaks at whitespace where it can, and HARD-BREAKS any token wider than the
/// whole column (a path, a url, an unbroken CJK run) because no word boundary can
/// help there. Only the whitespace a line actually BROKE on is dropped; every
/// other char survives into some line.
///
/// Every break lands on a GRAPHEME CLUSTER edge, never inside one, and that is
/// not cosmetic. `Line::width` sums `unicode-width` PER SPAN and that width is a
/// CONTEXTUAL fold, so a boundary drawn through a cluster — an emoji cut from its
/// VS16 or its skin-tone modifier, an `e` cut from its combining acute — changes
/// the summed width of text that did not change. That desyncs the preview's
/// cached wrapped-row map from the line actually painted, and the pane BOTH
/// windows its draw by that one map and hit-tests a click against it, so the
/// damage is a pane starting on the wrong line AND a click opening the wrong
/// link, not a mis-measured height. The cluster table is never hand-rolled
/// (`unicode-segmentation`), exactly as in [`crate::tui::view`]'s match runs.
///
/// Never panics: a `width` of 0 yields no lines at all, and a single cluster
/// wider than the whole column takes a line to itself rather than looping
/// forever. Grid mode keeps that last case unreachable — a column holding a
/// 2-column cluster has a natural width of at least 2, hence a floor of at least
/// 2 — but the helper does not rely on its caller for totality.
fn wrap_spans(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    if width == 0 {
        return Vec::new();
    }
    // One entry per grapheme cluster, carrying the style of the span it came from
    // and its display width, so a break can only ever land BETWEEN two entries.
    let mut cells: Vec<(&str, Style, usize)> = Vec::new();
    for span in spans {
        for g in span.content.as_ref().graphemes(true) {
            cells.push((g, span.style, display_width(g)));
        }
    }

    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut cur: Vec<(&str, Style, usize)> = Vec::new();
    let mut cur_w = 0usize;

    let mut i = 0usize;
    while i < cells.len() {
        // A run of whitespace is a break OPPORTUNITY: kept when the word after it
        // still fits this line, dropped when that word starts a new one.
        let gap_start = i;
        while i < cells.len() && is_blank_cluster(cells[i].0) {
            i += 1;
        }
        let gap = &cells[gap_start..i];
        let gap_w: usize = gap.iter().map(|c| c.2).sum();

        let word_start = i;
        while i < cells.len() && !is_blank_cluster(cells[i].0) {
            i += 1;
        }
        let word = &cells[word_start..i];
        let word_w: usize = word.iter().map(|c| c.2).sum();

        if word.is_empty() {
            // Trailing whitespace, so this is the last turn: keep only what still
            // fits, never open a line for it. `cur_w` is not advanced because
            // nothing reads it again.
            if !cur.is_empty() && cur_w + gap_w <= width {
                cur.extend_from_slice(gap);
            }
            break;
        }

        if !cur.is_empty() {
            if cur_w + gap_w + word_w > width {
                lines.push(coalesce_cells(&cur));
                cur.clear();
                cur_w = 0;
            } else {
                cur.extend_from_slice(gap);
                cur_w += gap_w;
            }
        }

        // Place the word, hard-breaking it across lines for as long as what is left
        // of it cannot fit one.
        let mut rest = word;
        loop {
            let rest_w: usize = rest.iter().map(|c| c.2).sum();
            if cur_w + rest_w <= width {
                cur.extend_from_slice(rest);
                cur_w += rest_w;
                break;
            }
            let mut take = 0usize;
            let mut take_w = 0usize;
            while take < rest.len() && cur_w + take_w + rest[take].2 <= width {
                take_w += rest[take].2;
                take += 1;
            }
            if take == 0 && cur.is_empty() {
                // One cluster wider than the whole column: give it a line of its
                // own. Without this the line would stay empty and never advance.
                take = 1;
            }
            cur.extend_from_slice(&rest[..take]);
            lines.push(coalesce_cells(&cur));
            cur.clear();
            cur_w = 0;
            rest = &rest[take..];
            if rest.is_empty() {
                break;
            }
        }
    }
    if !cur.is_empty() {
        lines.push(coalesce_cells(&cur));
    }
    lines
}

/// Is this grapheme cluster whitespace — i.e. a break opportunity for
/// [`wrap_spans`] rather than content?
fn is_blank_cluster(g: &str) -> bool {
    g.chars().all(char::is_whitespace)
}

/// Rebuild one wrapped line's grapheme cells back into spans, merging each run
/// that shares a style. The output is span-for-span what [`wrap_spans`] was
/// handed, minus the break points — so a cell's inline styling (DIM code, a bold
/// run, an underlined link label) survives the wrap intact.
fn coalesce_cells(cells: &[(&str, Style, usize)]) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    for (text, style, _) in cells {
        match out.last_mut() {
            Some(last) if last.style == *style => last.content.to_mut().push_str(text),
            _ => out.push(Span::styled((*text).to_string(), *style)),
        }
    }
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

/// Wrap one table cell to `width` display columns: inline-parse `raw` over `base`
/// (so `**bold**`/`` `code` ``/`[a](b)` style inside the cell), word-wrap the
/// result, then pad EVERY visual line out to exactly `width` per `align`. Returns
/// one span run per visual line — the cell's height in the grid. Width is
/// measured on the stripped display text, so styled and plain cells stay
/// column-aligned on every one of those lines.
fn wrap_cell_spans(raw: &str, width: usize, align: Align, base: Style) -> Vec<Vec<Span<'static>>> {
    wrap_spans(&parse_inline(raw, base), width)
        .into_iter()
        .map(|line| {
            let w = spans_display_width(&line);
            pad_cell_spans(line, w, width, align, base)
        })
        .collect()
}

/// Total display width of a `Line`'s spans (terminal columns).
fn line_display_width(line: &Line<'static>) -> usize {
    spans_display_width(&line.spans)
}

/// Final no-wrap guarantee for a GRID line: if one still exceeds `width` display
/// columns, clamp it to `width` with `…` so the row can never soft-wrap under
/// `Wrap { trim: false }` and scatter the grid.
///
/// This is now a BACKSTOP rather than the mechanism, and it should never fire: a
/// grid is only chosen when every column's floor fits the pane, so the widths
/// plus their separators are within budget by construction. It is kept because
/// the guarantee is load-bearing and cheap to hold, and drift in the arithmetic
/// above it would otherwise scatter the grid silently.
///
/// It must NOT be applied to [`render_table_records`]' lines: those are ordinary
/// logical lines that are SUPPOSED to overflow and soft-wrap, and clamping them
/// would reintroduce the very cut the record layout exists to remove.
fn clamp_line_to_width(line: Line<'static>, width: usize) -> Line<'static> {
    if line_display_width(&line) <= width {
        return line;
    }
    Line::from(truncate_spans(&line.spans, width, base_style()))
}

/// Shrink `widths` (widest column first) until `sum(widths) <= budget`, so the
/// table body fits the pane-derived column budget. No column ever drops below its
/// entry in `floors` — [`column_floors`], i.e. `min(natural, TABLE_MIN_COL_WIDTH)`
/// — because a column below that turns its wrapped text vertical.
///
/// Grid mode is only ENTERED when `sum(floors) <= budget` (see [`render_table`]),
/// so within it this always gets inside the budget and every grid line fits the
/// pane by construction; [`clamp_line_to_width`] is the backstop, not the
/// mechanism. A table too wide for the pane therefore lands on the budget
/// EXACTLY, filling the pane edge to edge.
///
/// It only ever DECREMENTS. A table narrower than the budget keeps its natural
/// width rather than being stretched across the pane: at natural width nothing
/// wraps, so stretching cannot save a line — it would only pad the columns apart.
fn fit_widths(mut widths: Vec<usize>, floors: &[usize], budget: usize) -> Vec<usize> {
    while widths.iter().sum::<usize>() > budget {
        let Some((idx, _)) = widths
            .iter()
            .enumerate()
            .filter(|(i, &w)| w > floors.get(*i).copied().unwrap_or(1))
            .max_by_key(|(_, &w)| w)
        else {
            break; // every column is already at its floor
        };
        widths[idx] -= 1;
    }
    widths
}

/// The per-column width floor a grid layout must honor: a column never needs more
/// than its NATURAL width, and never gets less than [`TABLE_MIN_COL_WIDTH`], so
/// the floor is the smaller of the two.
///
/// Summed (plus the structural separators) this is the narrowest grid the table
/// can be drawn as, and therefore the record-fallback threshold. Taking the `min`
/// per column rather than the flat `ncols * TABLE_MIN_COL_WIDTH` product is what
/// keeps a table of SHORT cells out of the record layout: five 3-to-5-column
/// cells need 19 columns of content, not 50, and demanding the product would dump
/// a table that fits comfortably.
fn column_floors(natural: &[usize]) -> Vec<usize> {
    natural
        .iter()
        .map(|&n| n.min(TABLE_MIN_COL_WIDTH))
        .collect()
}

/// Render a GFM pipe table beginning at `rows[0]` (the header), with `rows[1]`
/// the delimiter, fitting the whole table to `width` display columns (the preview
/// pane's inner content width) WITHOUT ever cutting a cell. Returns the styled
/// lines and the number of INPUT rows consumed (header + delimiter + body rows).
/// Body rows are consumed until a blank line or a non-table row (no `|` after
/// trim). Never panics on malformed input.
///
/// Cells ARE inline-parsed (`**bold**` / `` `code` `` / `[a](b)` style inside the
/// grid). Column widths are measured on each cell's marker-STRIPPED display text
/// (`**x**` is one column, not five), so styling can never skew alignment.
///
/// This is a CHOOSER over two layouts, and NEITHER of them ever cuts a cell:
///
/// * [`render_table_grid`] (the default) shrinks the columns to fit `width` and
///   WRAPS an over-wide cell down its column, so a row becomes as many visual
///   lines as its tallest cell needs — and draws a rule between adjacent rows so
///   those multi-line blocks stay tellable apart.
/// * [`render_table_records`] takes over when even a floor-width grid cannot fit
///   the pane (`sum(column_floors) + separators > width`) — more columns at their
///   floor than the pane has room for, which takes as few as TWO wide-celled ones
///   (`10 + 3 + 10 = 23`) against a narrower pane, not necessarily many. There is
///   no grid left to scatter, so each row is stacked as `Header: value` lines and
///   left for the pane's own soft wrap.
///
/// Neither layout records link regions for its cells; see
/// [`markdown_body_lines_collect`] for why.
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
    // min 1 so empty columns still render. It is what the column WANTS; the layout
    // below decides what it gets.
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
    // Reserve the 3-column `" │ "` / `"─┼─"` separators between columns. The ONLY
    // budget is the pane's inner content `width` — a table is never capped short
    // of it, so a wide one fills the pane edge to edge and follows a splitter
    // drag. No scrollbar column is subtracted: it overlays the block's right
    // border, not a content column.
    let sep_total = 3 * ncols.saturating_sub(1);

    // ONE number governs both the choice and the layout, so a grid is only ever
    // chosen when its floors are actually affordable inside it.
    let floors = column_floors(&natural);
    let min_grid_width = floors.iter().sum::<usize>() + sep_total;
    let lines = if min_grid_width <= width {
        render_table_grid(&header_cells, &body_rows, &aligns, natural, &floors, width)
    } else {
        render_table_records(&header_cells, &body_rows)
    };
    (lines, consumed)
}

/// The GRID layout: header row, `─┼─` separator, then one block of visual lines
/// per body row (see [`table_data_lines`]), with that SAME `─┼─` rule drawn
/// between each adjacent pair of body rows. Columns are shrunk to fit the pane
/// budget but never below their floor, and an over-wide cell WRAPS down its
/// column rather than being cut.
///
/// The row rules exist BECAUSE of that wrapping: a row spanning several visual
/// lines runs straight into the next one, and a 3-line row above a 2-line one
/// reads as a single five-line block. They are drawn UNCONDITIONALLY rather than
/// only for tables that wrapped, so the grid has one shape at every width — a
/// table must not change its chrome when a splitter drag happens to make a cell
/// fit. Reusing the header separator keeps the grid one vocabulary instead of
/// inventing a second kind of rule; the header stays distinguishable between two
/// identical rules because it alone is BOLD.
///
/// Every line here is guaranteed to fit `width`, so the grid can never soft-wrap
/// under `Wrap { trim: false }` and scatter. That now holds by construction —
/// grid mode is only entered when the floors fit — and [`clamp_line_to_width`]
/// stays as the backstop that keeps the guarantee true if the arithmetic above it
/// ever drifts.
fn render_table_grid(
    header_cells: &[String],
    body_rows: &[Vec<String>],
    aligns: &[Align],
    natural: Vec<usize>,
    floors: &[usize],
    width: usize,
) -> Vec<Line<'static>> {
    let sep_total = 3 * floors.len().saturating_sub(1);
    let budget = width.saturating_sub(sep_total);
    let widths = fit_widths(natural, floors, budget);

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.extend(table_data_lines(
        header_cells,
        &widths,
        aligns,
        base_style().add_modifier(Modifier::BOLD),
    ));
    lines.push(table_separator_line(&widths));
    for (i, row) in body_rows.iter().enumerate() {
        // A rule per row BOUNDARY — between adjacent body rows only. Since a
        // wrapped row spans several visual lines, consecutive rows otherwise run
        // together into one block with no way to tell where a row ended. Not
        // before the first row (the header separator above already marks that
        // edge) and not after the last (nothing follows it to separate from).
        if i > 0 {
            lines.push(table_separator_line(&widths));
        }
        lines.extend(table_data_lines(row, &widths, aligns, base_style()));
    }

    lines
        .into_iter()
        .map(|l| clamp_line_to_width(l, width))
        .collect()
}

/// The narrow-pane FALLBACK layout: with no grid that can seat every column at
/// its floor, drop the grid entirely and stack each body row as `Header: value`
/// lines, separated by a DIM [`RECORD_RULE_WIDTH`] rule.
///
/// Each line is an ORDINARY logical line and is deliberately NOT passed through
/// [`clamp_line_to_width`]: these lines are MEANT to overflow the pane and be
/// soft-wrapped by the preview's own `Wrap { trim: false }`, exactly as a prose
/// paragraph is. Clamping them would reintroduce the very cut this layout exists
/// to remove — and there is no grid left for a wrap to scatter, which is the
/// whole reason the fallback is safe.
///
/// An EMPTY cell contributes no line at all, so a sparse row does not become a
/// column of bare labels. A table with no body rows has nothing to stack, so its
/// headers are emitted on their own rather than the table vanishing.
fn render_table_records(header_cells: &[String], body_rows: &[Vec<String>]) -> Vec<Line<'static>> {
    if body_rows.is_empty() {
        return header_cells
            .iter()
            .filter(|h| !h.is_empty())
            .map(|h| Line::from(parse_inline(h, base_style().add_modifier(Modifier::BOLD))))
            .collect();
    }

    let rule = "\u{2500}".repeat(RECORD_RULE_WIDTH);
    let mut lines: Vec<Line<'static>> = Vec::new();
    for row in body_rows {
        let mut record: Vec<Line<'static>> = Vec::new();
        for (c, cell) in row.iter().enumerate() {
            if cell.is_empty() {
                continue;
            }
            // The label is the header's marker-STRIPPED text, so it reads `Beta`
            // rather than `**Beta**` — the same stripping the grid aligns on.
            let label = header_cells
                .get(c)
                .map(|h| spans_display_text(&parse_inline(h, Style::default())))
                .unwrap_or_default();
            let mut spans: Vec<Span<'static>> = Vec::new();
            if !label.is_empty() {
                spans.push(Span::styled(format!("{label}: "), record_label_style()));
            }
            spans.extend(parse_inline(cell, base_style()));
            record.push(Line::from(spans));
        }
        if record.is_empty() {
            continue; // an all-empty row earns no record and no rule
        }
        if !lines.is_empty() {
            lines.push(Line::from(Span::styled(rule.clone(), table_border_style())));
        }
        lines.append(&mut record);
    }
    lines
}

/// Build ONE header/body table row as the N visual lines its tallest cell needs:
/// each cell is inline-parsed, word-wrapped to its column `width` over
/// `cell_style` ([`wrap_cell_spans`]) and padded, then the row's lines are joined
/// by DIM `" │ "` column rules.
///
/// CONTINUATION lines carry the rules and the per-column padding exactly as the
/// first one does, and a cell that ran out of lines contributes a run of spaces —
/// so every visual line of the row shares one display width and one rule column,
/// and the grid never scatters. A row always occupies at least one line, even
/// when every cell in it is empty.
fn table_data_lines(
    cells: &[String],
    widths: &[usize],
    aligns: &[Align],
    cell_style: Style,
) -> Vec<Line<'static>> {
    let wrapped: Vec<Vec<Vec<Span<'static>>>> = widths
        .iter()
        .enumerate()
        .map(|(c, width)| wrap_cell_spans(&cells[c], *width, aligns[c], cell_style))
        .collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(0).max(1);

    (0..height)
        .map(|row| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (c, width) in widths.iter().enumerate() {
                if c > 0 {
                    spans.push(Span::styled(" \u{2502} ".to_string(), table_border_style()));
                }
                match wrapped[c].get(row) {
                    Some(line) => spans.extend(line.iter().cloned()),
                    // This cell is shorter than the row: hold its column open.
                    None => spans.push(Span::styled(" ".repeat(*width), cell_style)),
                }
            }
            Line::from(spans)
        })
        .collect()
}

/// Build a DIM box-drawing separator row: `─` fill per column, `─┼─` at the
/// column junctions so each `┼` lines up with the `│` above it.
///
/// ONE rule serves both grid boundaries — under the header and between adjacent
/// body rows — so the two never drift apart into different-looking chrome.
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

    /// A comfortably wide preview pane for the fixtures whose subject is NOT
    /// shrink-to-fit, so an ordinary transcript table renders at its natural
    /// width. A plain test literal, deliberately tied to no production constant:
    /// nothing caps a table short of its pane, so this number binds these
    /// fixtures alone and a test that needs another pane just passes one.
    ///
    /// It is a comfortable width, NOT a no-wrap guarantee — a cell wider than
    /// this still wraps and a grid still shrinks to it, which is exactly what
    /// `overwide_multibyte_cell_wraps_on_grapheme_cluster_boundaries` pins.
    const WIDE: usize = 96;

    fn fixture(folder: &str, file: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("store")
            .join(folder)
            .join(file)
    }

    /// A fixture under `tests/fixtures/preview/` — deliberately OUTSIDE the `store/`
    /// discovery root, so a preview-only format edge case is handed straight to the
    /// renderer and can never disturb the exact discovered/session counts `store`'s
    /// own tests pin.
    fn preview_fixture(file: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("preview")
            .join(file)
    }

    /// Text-only convenience over [`render_file_collect`] for the transcript-shape
    /// tests that assert markers/structure rather than link regions. No known
    /// agents, so the `agent-name` fallback stays inert; see [`render_file_known`]
    /// for tests that exercise it.
    fn render_file(path: &Path, width: usize) -> Text<'static> {
        render_file_collect(path, width, &HashSet::new()).text
    }

    /// Like [`render_file`] but with an explicit set of known DEFINED agents, so a
    /// test can exercise the validated `agent-name` fallback.
    fn render_file_known(path: &Path, width: usize, known: &[&str]) -> Text<'static> {
        let known: HashSet<&str> = known.iter().copied().collect();
        render_file_collect(path, width, &known).text
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

    /// EVERY line led by `needle`, in transcript order — for the tests that assert a
    /// PER-TURN annotation across a multi-turn session, where checking only the first
    /// turn would pass against a hoisted (file-level) value.
    fn lines_led_by<'a>(text: &'a Text, needle: &str) -> Vec<&'a Line<'a>> {
        text.lines
            .iter()
            .filter(|l| l.spans.first().map(|s| s.content.as_ref()) == Some(needle))
            .collect()
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

    /// The optimistic reply tail echoes the sent message under a `▶ you` turn plus
    /// a pending `● claude` placeholder while a quick-reply is in flight, then drops
    /// the echo — leaving only the placeholder — once the caller signals the real
    /// turn has landed (`echo_message = None`). Styled exactly like real turns, no
    /// embedded ANSI.
    #[test]
    fn pending_reply_turns_echoes_then_yields_to_the_real_turn() {
        // Before the real turn lands: echo the message + a "sending…" placeholder.
        let with_echo = pending_reply_turns(Some("hello there"), "\u{280b} sending\u{2026}", WIDE);
        let text = flatten(&Text::from(with_echo));
        assert!(text.contains(YOU_MARKER), "echoes a `you` turn:\n{text}");
        assert!(text.contains("hello there"), "echoes the message:\n{text}");
        assert!(
            text.contains(CLAUDE_MARKER) && text.contains("sending"),
            "has a pending claude placeholder:\n{text}"
        );

        // After it lands: no echoed `you` turn, only the pending claude placeholder,
        // so the real turn (rendered by the reload) is never doubled.
        let no_echo = pending_reply_turns(None, "\u{280b} cooking\u{2026}", WIDE);
        let text = flatten(&Text::from(no_echo));
        assert!(
            !text.contains(YOU_MARKER),
            "no echoed `you` turn once landed:\n{text}"
        );
        assert!(
            text.contains(CLAUDE_MARKER) && text.contains("cooking"),
            "keeps the placeholder until the send finishes:\n{text}"
        );
        // Styling is via ratatui Style, never embedded ANSI (TERMINAL-SAFE STYLING).
        assert!(!text.contains('\u{1b}'), "reply tail must not embed ANSI");
    }

    #[test]
    fn render_keeps_turn_separators_and_tool_markers() {
        let text = render_file(
            &fixture("-Users-me-project-alpha", "sess-normal-1.jsonl"),
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

    /// Every marker [`render_file_collect`] collects addresses the row its OWN line
    /// was rendered on — the `summary` head included, whose block is a SINGLE line
    /// and so carries its marker at block row 0, where a `user` or `assistant`
    /// turn's leading blank line puts theirs at row 1.
    ///
    /// [`RenderedPreview::markers`] has exactly one reader — the pinned preview
    /// banner, through `App::preview_marker_at` — and a marker rebased onto the
    /// wrong row still renders a perfectly ordinary transcript, so nothing in the
    /// drawn text can catch it. That is why the row is pinned HERE, at the producer,
    /// rather than left to a pane test: the banner's own suite resolves WHICH turn
    /// owns a scroll offset, and takes these rows as given.
    ///
    /// INDEPENDENT on both sides. The summary's expected text is read back out of
    /// the FIXTURE's own first record, so re-wording the fixture moves the
    /// expectation with it instead of stranding a hardcoded string; and the rows are
    /// re-derived by walking the rendered `Text` FORWARD for every marker-led line,
    /// never by asking the list under test where its markers are.
    #[test]
    fn collected_markers_address_their_rendered_rows_summary_head_included() {
        let path = fixture("-Users-me-project-alpha", "sess-normal-1.jsonl");
        let raw = std::fs::read_to_string(&path).expect("read the fixture");
        let record: Value = serde_json::from_str(raw.lines().next().expect("a first record"))
            .expect("the first record is JSON");
        assert_eq!(
            record.get("type").and_then(Value::as_str),
            Some("summary"),
            "this fixture must LEAD with a summary record, or the arm under test is \
             never reached"
        );
        // The head renders the fixture's own summary verbatim behind a `# `, so the
        // expectation comes from the INPUT rather than from anything the renderer
        // produced.
        let expected_head = format!(
            "# {}",
            record
                .get("summary")
                .and_then(Value::as_str)
                .expect("a summary record names its summary")
        );

        let rendered = render_file_collect(&path, WIDE, &HashSet::new());
        let text_of =
            |line: &Line<'_>| -> String { line.spans.iter().map(|s| s.content.as_ref()).collect() };

        // Walk the rendered transcript forward for every marker-led line — the `# `
        // head as well as the two turn glyphs. A markdown body header has its hashes
        // STRIPPED by `markdown_body_lines`, so nothing inside a turn can pose as the
        // summary head here.
        let expected: Vec<(usize, String)> = rendered
            .text
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| {
                line.spans.first().is_some_and(|span| {
                    let head = span.content.as_ref();
                    head == YOU_MARKER || head == CLAUDE_MARKER || head == expected_head
                })
            })
            .map(|(row, line)| (row, text_of(line)))
            .collect();
        let actual: Vec<(usize, String)> = rendered
            .markers
            .iter()
            .map(|marker| (marker.content_row, text_of(&marker.line)))
            .collect();

        // The fixture must carry ordinary turns beside the summary, or the rebase
        // past the head's one-line block is never exercised.
        assert!(
            expected.len() > 1,
            "the oracle must find the summary AND real turns: {expected:?}"
        );
        assert_eq!(
            actual, expected,
            "every collected marker must address the row its line was rendered on, \
             in transcript order"
        );
        // The arm this exists for: the head is the FIRST marker and sits at row 0 —
        // its block contributes exactly one line, so there is no leading blank row to
        // rebase past, and the banner has a turn to name at the very top of a
        // summary-led transcript.
        assert_eq!(
            actual.first(),
            Some(&(0, expected_head)),
            "the summary head must be a pinnable marker at the transcript's first row"
        );
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
        // A well-formed 2-column table: header + separator + two body rows with
        // a rule between them, all padded to the same total display width (the
        // alignment invariant).
        let body = "| A | B |\n| --- | --- |\n| 1 | 22 |\n| 333 | 4 |";
        let lines = markdown_body_lines(body, WIDE);
        assert_eq!(
            lines.len(),
            5,
            "header + separator + 2 body rows + 1 row rule"
        );

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
        // panic; the table still renders header + separator + two body rows with
        // a rule between them.
        let body = "| A | B | C |\n| --- | --- | --- |\n| 1 |\n| 2 | 3 | 4 | 5 | 6 |";
        let lines = markdown_body_lines(body, WIDE);
        assert_eq!(
            lines.len(),
            5,
            "header + separator + 2 body rows + 1 row rule"
        );
        // Every line stays the same width despite the ragged input.
        let widths: Vec<usize> = lines.iter().map(|l| line_text(l).chars().count()).collect();
        assert!(
            widths.iter().all(|&w| w == widths[0]),
            "columns stay aligned"
        );
        // The over-long row kept exactly the header's column count: 3 columns
        // means 2 interior " │ " rules (cells now emit a variable span count).
        // It is the LAST line: header, separator, row 1, the row rule, row 2.
        let rules = lines[4]
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
    fn overwide_multibyte_cell_wraps_on_grapheme_cluster_boundaries() {
        // A single-column cell far wider than the width budget, built from 2-byte
        // chars, must WRAP onto continuation lines (never be cut) on cluster
        // boundaries, and never panic.
        let wide = "é".repeat(200);
        let body = format!("| head |\n| --- |\n| {wide} |");
        let lines = markdown_body_lines(&body, WIDE);
        // 200 columns of content in a 96-column column: header + separator + the
        // body row's three visual lines (96 + 96 + 8).
        assert_eq!(lines.len(), 5, "header + separator + 3 wrapped body lines");
        let body_text: String = lines[2..].iter().map(line_text).collect();
        assert!(
            !body_text.contains('\u{2026}'),
            "the cell wraps instead of being cut: {body_text}"
        );
        assert_eq!(
            body_text.matches('é').count(),
            200,
            "every char of the over-wide cell survived the wrap"
        );
        for line in &lines {
            assert!(
                display_width(&line_text(line)) <= WIDE,
                "every grid line still fits the width budget"
            );
        }
    }

    // --- table wrapping: grid mode ----------------------------------------

    /// The display column a line's first `ch` sits at, measured in DISPLAY columns
    /// rather than chars so a double-width cell cannot fake a match. Asked with
    /// `│` of a data line and with `┼` of a separator, which carries the junction
    /// in that same column instead.
    fn col_of(s: &str, ch: char) -> Option<usize> {
        let idx = s.find(ch)?;
        Some(display_width(&s[..idx]))
    }

    #[test]
    fn narrow_grid_wraps_cells_instead_of_cutting_them() {
        // Two prose columns at a width that forces both well below their natural
        // width: every word must still be present, and nothing may be replaced by
        // an ellipsis.
        let body = "| Alpha | Beta |\n| --- | --- |\n\
                    | the quick brown fox | jumps over the lazy dog |";
        let lines = markdown_body_lines(body, 30);
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            !joined.contains('\u{2026}'),
            "no cell is cut at a narrow width: {joined}"
        );
        for word in ["quick", "brown", "jumps", "lazy", "dog"] {
            assert!(
                joined.contains(word),
                "{word:?} survived the wrap: {joined}"
            );
        }
    }

    #[test]
    fn a_wrapped_grid_row_keeps_its_rules_and_one_display_width() {
        // A tall row (its cells wrap) plus a short one. The tall row becomes N
        // visual lines; every one of them keeps the `│` at the same column and
        // measures the same total width, so the grid never scatters.
        let body = "| Alpha | Beta |\n| --- | --- |\n\
                    | the quick brown fox | jumps over the lazy dog |\n| x | y |";
        let lines = markdown_body_lines(body, 30);
        assert!(
            lines.len() > 4,
            "the tall row spans several visual lines, got {} lines",
            lines.len()
        );

        let texts: Vec<String> = lines.iter().map(line_text).collect();
        let widths: Vec<usize> = texts.iter().map(|t| display_width(t)).collect();
        assert!(
            widths.iter().all(|&w| w == widths[0]),
            "all grid lines share one display width: {widths:?}"
        );

        // Every DATA line (a `─┼─` separator carries `┼` at that column instead)
        // puts its `│` at one and the same display column.
        let cols: Vec<Option<usize>> = texts
            .iter()
            .filter(|t| !t.contains('\u{253c}'))
            .map(|t| col_of(t, '\u{2502}'))
            .collect();
        assert!(
            cols[0].is_some() && cols.iter().all(|c| *c == cols[0]),
            "continuation lines keep the column rule in place: {cols:?}"
        );
    }

    #[test]
    fn a_rule_separates_each_pair_of_grid_body_rows_and_lines_up_with_them() {
        // Wrapping lets one row span several visual lines, so with nothing drawn
        // between rows a 3-line row above a 2-line one reads as a single
        // five-line block — rows and columns stop looking like separate units.
        // A rule per row BOUNDARY is what says where a row ends, and it reuses
        // the header separator's `─┼─` vocabulary so the whole grid reads as one
        // piece of chrome rather than two. That leaves the header framed by two
        // identical rules; the header stays legible because it alone is BOLD.
        //
        // Stated over a fixture whose first two rows WRAP, since a multi-line row
        // is the case the rules exist for. Each row owns a unique word, so the
        // blocks between rules can be checked to hold exactly one row each —
        // proving a rule lands at a row boundary and never mid-row.
        let body = "| Alpha | Beta |\n| --- | --- |\n\
                    | the quick brown fox | jumps over lazy dogs |\n\
                    | second wrapping row | with further wrapped words |\n\
                    | tail | end |";
        let lines = markdown_body_lines(body, 30);
        let texts: Vec<String> = lines.iter().map(line_text).collect();

        // Split on the rules; a `┼` junction is carried by separator lines alone.
        let mut blocks: Vec<Vec<&String>> = vec![Vec::new()];
        for t in &texts {
            if t.contains('\u{253c}') {
                blocks.push(Vec::new());
            } else {
                blocks.last_mut().expect("seeded with one block").push(t);
            }
        }
        // Header block + 3 body rows = 4 blocks, so 3 rules: the header separator
        // plus ONE boundary per adjacent PAIR of body rows. No extra rule before
        // the first body row (the header separator already marks that edge) and
        // none after the last — either would leave an EMPTY block behind.
        assert_eq!(
            blocks.len(),
            4,
            "header + 3 rows, one rule between each: {texts:#?}"
        );
        assert!(
            blocks.iter().all(|b| !b.is_empty()),
            "every rule sits BETWEEN blocks, never doubled, leading or trailing: {texts:#?}"
        );
        for (block, word) in blocks.iter().zip(["Alpha", "quick", "second", "tail"]) {
            let joined = block.iter().map(|t| t.as_str()).collect::<String>();
            assert!(
                joined.contains(word),
                "{word:?} is alone in its block: {joined:?}"
            );
        }
        // The rows really did wrap, or this pins nothing about the case that
        // motivated the rules in the first place.
        assert!(blocks[1].len() > 1, "row 1 wrapped: {:?}", blocks[1]);
        assert!(blocks[2].len() > 1, "row 2 wrapped: {:?}", blocks[2]);

        // A boundary is only legible as part of the grid if its `┼` sits in the
        // very column the `│` above and below it do.
        let (rules, data): (Vec<&String>, Vec<&String>) =
            texts.iter().partition(|t| t.contains('\u{253c}'));
        let data_cols: Vec<Option<usize>> = data.iter().map(|t| col_of(t, '\u{2502}')).collect();
        assert!(
            data_cols[0].is_some() && data_cols.iter().all(|c| *c == data_cols[0]),
            "every data line puts `│` at one column: {data_cols:?}"
        );
        let junctions: Vec<Option<usize>> = rules.iter().map(|t| col_of(t, '\u{253c}')).collect();
        assert!(
            junctions.iter().all(|c| *c == data_cols[0]),
            "every rule puts `┼` at that same column {data_cols:?}: {junctions:?}"
        );
    }

    #[test]
    fn a_token_longer_than_its_column_hard_breaks_without_loss() {
        // A 60-char unbroken token (a path or url) in a column far narrower than
        // it: word wrap cannot help, so it must HARD-BREAK rather than be cut.
        // The header cell shares no letter with the token, so counting `a` over
        // the WHOLE joined output counts the token's chars ALONE: 60 then means
        // the 60-char token survived whole, and pins no DUPLICATION as well as
        // no loss.
        let token = "a".repeat(60);
        let body = format!("| Url | N |\n| --- | --- |\n| {token} | 1 |");
        let lines = markdown_body_lines(&body, 20);
        let joined: String = lines.iter().map(line_text).collect();
        assert!(
            !joined.contains('\u{2026}'),
            "hard-broken, not cut: {joined}"
        );
        assert_eq!(
            joined.matches('a').count(),
            60,
            "every char of the over-long token survived: {joined}"
        );
    }

    #[test]
    fn a_wrapped_cell_never_splits_a_grapheme_cluster() {
        // "é" spelled as `e` + U+0301 (a TWO-scalar cluster) beside a 2-wide emoji.
        // A wrapper walking chars would strand the combining mark at the head of a
        // continuation line, changing the summed width of text that did not change
        // and desyncing the cached wrapped-row count from the painted line.
        let cell = "e\u{0301}\u{1f600}".repeat(12);
        let body = format!("| C |\n| --- |\n| {cell} |");
        let lines = markdown_body_lines(&body, 14);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        let joined: String = texts.concat();

        assert_eq!(
            joined.matches('\u{1f600}').count(),
            12,
            "every emoji survived: {joined:?}"
        );
        assert_eq!(
            joined.matches('\u{0301}').count(),
            12,
            "every combining mark survived: {joined:?}"
        );
        for t in texts.iter().skip(2) {
            assert!(
                !t.starts_with('\u{0301}'),
                "a continuation line must not open on a stranded combining mark: {t:?}"
            );
        }
        let widths: Vec<usize> = texts.iter().map(|t| display_width(t)).collect();
        assert!(
            widths.iter().all(|&w| w == widths[0]),
            "cluster-safe wrapping keeps every line one width: {widths:?}"
        );
    }

    #[test]
    fn a_wide_pane_is_filled_exactly_and_a_narrow_table_is_not_stretched() {
        // The pane is the ONLY budget a grid answers to, and it answers in one
        // direction. Both halves of that are pinned here at a pane far wider than
        // any test above reaches, because the two are one decision: `fit_widths`
        // only ever DECREMENTS, so a table wider than the pane comes down to it
        // exactly, and a table narrower than it is left alone.
        const PANE: usize = 200;

        // Natural width 150 + 3 + 150 = 303, far past the pane: both columns
        // shrink until the row measures the pane EDGE TO EDGE. A 197-column
        // content budget split over two columns plus the 3-column rule is 200 —
        // no empty gutter beside the table, and no more wrapped rows than the
        // pane actually forces.
        let cell = "w".repeat(150);
        let overwide = format!("| Alpha | Beta |\n| --- | --- |\n| {cell} | {cell} |");
        let lines = markdown_body_lines(&overwide, PANE);
        // 150 columns of unbroken content in a ~98-column column hard-breaks onto
        // two visual lines: header + separator + those 2. Pinned so the width
        // check below is answering for real lines, not passing over an empty vec.
        assert_eq!(lines.len(), 4, "header + separator + 2 wrapped body lines");
        let widths: Vec<usize> = lines.iter().map(|l| display_width(&line_text(l))).collect();
        assert!(
            widths.iter().all(|&w| w == PANE),
            "an over-wide grid fills the pane exactly: {widths:?}"
        );

        // The other direction: natural width 5 + 3 + 4 = 12 stays 12. Stretching
        // it could not save a line — nothing wraps at natural width — and would
        // only open a canyon between two columns.
        let narrow = "| Alpha | Beta |\n| --- | --- |\n| x | y |";
        let narrow_lines = markdown_body_lines(narrow, PANE);
        // Nothing wraps at natural width, so the row stays one visual line:
        // header + separator + that 1.
        assert_eq!(narrow_lines.len(), 3, "header + separator + 1 body row");
        let narrow_widths: Vec<usize> = narrow_lines
            .iter()
            .map(|l| display_width(&line_text(l)))
            .collect();
        assert!(
            narrow_widths.iter().all(|&w| w == 12),
            "a narrow grid keeps its content width, never padded out to the pane: \
             {narrow_widths:?}"
        );
    }

    // --- table wrapping: the record-mode threshold -------------------------

    #[test]
    fn a_grid_that_cannot_reach_its_floor_falls_back_to_stacked_records() {
        // 5 columns of WIDE content at 60: every column wants more than the floor,
        // so `min_grid_width` is 5*10 + 4*3 = 62 > 60 and the grid is abandoned.
        let body = "| Alpha | Beta | Gamma | Delta | Epsilon |\n\
                    | --- | --- | --- | --- | --- |\n\
                    | first value | second value | third value | \
                    fourth value | fifth value |";
        let lines = markdown_body_lines(body, 60);
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            !joined.contains('\u{2502}') && !joined.contains('\u{253c}'),
            "no grid chrome in record mode: {joined}"
        );
        assert!(
            !joined.contains('\u{2026}'),
            "nothing is cut in record mode: {joined}"
        );
        assert!(
            joined.contains("Gamma: third value"),
            "a record reads `Header: value`: {joined}"
        );
    }

    #[test]
    fn a_narrow_content_grid_is_not_dumped_into_records() {
        // The other side of the threshold, and the reason it is a per-column
        // `min(natural, floor)` SUM rather than the naive `cols * floor` product:
        // 5 short columns want 3+4+5+3+4 = 19 content columns plus 12 of separators
        // = 31, which fits a 60-column pane with room to spare. The product
        // (5*10 + 12 = 62) would dump this common table into a record dump.
        let body = "| abc | abcd | abcde | xyz | wxyz |\n\
                    | --- | --- | --- | --- | --- |\n\
                    | 1 | 2 | 3 | 4 | 5 |";
        let lines = markdown_body_lines(body, 60);
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            joined.contains('\u{2502}') && joined.contains('\u{253c}'),
            "a short-celled 5-column table stays a grid at 60: {joined}"
        );
    }

    #[test]
    fn the_record_fallback_switch_points_are_pinned_at_a_62_column_pane() {
        // Columns whose natural width all EXCEED the floor, so each contributes a
        // full `TABLE_MIN_COL_WIDTH`. At a 62-column preview (a 120-column terminal
        // split in half) the switch lands between 5 and 6 columns:
        //   4 cols -> 4*10 + 3*3 = 49 <= 62  grid
        //   5 cols -> 5*10 + 4*3 = 62 <= 62  grid (it fits EXACTLY)
        //   6 cols -> 6*10 + 5*3 = 75 >  62  record
        // and one column narrower is all it takes to tip 5 columns over.
        fn table_of(ncols: usize) -> String {
            let head = (0..ncols).map(|_| "wide header").collect::<Vec<_>>();
            let delim = (0..ncols).map(|_| "---").collect::<Vec<_>>();
            let body = (0..ncols).map(|_| "wide value!!").collect::<Vec<_>>();
            format!(
                "| {} |\n| {} |\n| {} |",
                head.join(" | "),
                delim.join(" | "),
                body.join(" | ")
            )
        }
        fn is_grid(body: &str, width: usize) -> bool {
            markdown_body_lines(body, width)
                .iter()
                .any(|l| line_text(l).contains('\u{2502}'))
        }

        assert!(is_grid(&table_of(4), 62), "4 columns fit 62 as a grid");
        assert!(
            is_grid(&table_of(5), 62),
            "5 columns fit 62 EXACTLY, so the grid is kept"
        );
        assert!(
            !is_grid(&table_of(5), 61),
            "one column narrower and 5 columns tip into records"
        );
        assert!(
            !is_grid(&table_of(6), 62),
            "6 columns cannot reach the floor at 62"
        );
    }

    #[test]
    fn a_record_omits_its_empty_cells_and_labels_the_rest() {
        // A sparse row must not become a column of bare labels, so an EMPTY cell
        // contributes no line at all. Records are separated by a DIM `─` rule.
        let body = "| Alpha | Beta | Gamma | Delta | Epsilon |\n\
                    | --- | --- | --- | --- | --- |\n\
                    | first value | | third value | | fifth value |\n\
                    | another one | second here | | fourth here | last value |";
        let lines = markdown_body_lines(body, 40);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        let joined = texts.join("\n");
        assert!(
            !joined.contains("Beta: \n") && !joined.contains("Delta: \n"),
            "an empty cell emits no line at all: {joined}"
        );
        assert_eq!(
            texts.iter().filter(|t| t.starts_with("Beta:")).count(),
            1,
            "only the row that HAS a Beta value labels one: {joined}"
        );
        assert!(
            texts.iter().any(|t| t.starts_with('\u{2500}')),
            "a dim rule separates two records: {joined}"
        );
    }

    #[test]
    fn a_header_only_table_in_record_mode_still_shows_its_headers() {
        // A header + delimiter with no body rows has nothing to STACK, so the
        // record layout would emit nothing at all and the table would vanish from
        // the transcript — a content loss in the one layout that exists because
        // nothing may be lost. With no records to label, the headers ARE what is
        // left to show.
        // 6 columns of 4-to-7 natural width: 30 + 5 * 3 = 45 > 40, so this routes
        // to record mode rather than proving the grid's behaviour by accident.
        let body = "| Alpha | Beta | Gamma | Delta | Epsilon | Zeta |\n\
                    | --- | --- | --- | --- | --- | --- |";
        let lines = markdown_body_lines(body, 40);
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            !joined.contains('\u{2502}') && !joined.contains('\u{253c}'),
            "6 columns at 40 route to record mode, so no grid chrome: {joined}"
        );
        for head in ["Alpha", "Beta", "Gamma", "Delta", "Epsilon", "Zeta"] {
            assert!(
                joined.contains(head),
                "{head:?} survived a body-less table: {joined}"
            );
        }
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

        let rendered = render_file_collect(&file, WIDE, &HashSet::new());
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

    /// How many rendered lines the tail cap used to keep. Named here ONLY so the
    /// test below can prove a transcript reaches past it; nothing in the renderer
    /// knows this number any more.
    const FORMER_TAIL_CAP: usize = 600;

    /// A transcript longer than the cap that used to truncate it renders WHOLE — its
    /// oldest turns included, with their links still pointing at the rows they were
    /// rendered on.
    ///
    /// This is the deletion's user-visible payoff, and both halves were real losses.
    /// The cap kept only the most-recent 600 rendered lines, so a long conversation
    /// simply had no beginning in the preview: scrolling to the top showed the middle
    /// of a turn with nothing above it, and the board's content search could admit a
    /// row on words the pane could never show. A link above the cut was DROPPED
    /// outright by the same pass, since its row no longer existed.
    #[test]
    fn a_transcript_past_the_former_tail_cap_keeps_its_oldest_turns_and_their_links() {
        let dir = unique_temp_dir("no-tail-cap");
        let file = dir.join("sess.jsonl");
        // Each turn renders as a blank line, a marker line and one body line, so
        // this clears the old cap several times over.
        let turns = FORMER_TAIL_CAP;
        let mut jsonl = String::new();
        for turn in 0..turns {
            let body = if turn == 0 {
                "the oldest turn says [origin](https://example.com/origin) here".to_string()
            } else {
                format!("turn {turn} body")
            };
            jsonl.push_str(&format!(
                r#"{{"type":"user","sessionId":"s","cwd":"/x","timestamp":"2026-07-01T10:00:00.000Z","message":{{"role":"user","content":"{body}"}}}}"#
            ));
            jsonl.push('\n');
        }
        std::fs::write(&file, jsonl).expect("write temp jsonl");

        let rendered = render_file_collect(&file, WIDE, &HashSet::new());
        assert!(
            rendered.text.lines.len() > FORMER_TAIL_CAP,
            "the fixture must render past the old cap, or this proves nothing \
             (lines={})",
            rendered.text.lines.len()
        );
        let flat = flatten(&rendered.text);
        assert!(
            flat.contains("the oldest turn says"),
            "the first turn must survive into the preview"
        );
        assert!(
            flat.contains(&format!("turn {} body", turns - 1)),
            "and so must the last"
        );

        // The oldest turn's link keeps the row it was rendered on, rather than being
        // dropped for sitting above a cut.
        let region = rendered
            .links
            .first()
            .expect("the oldest turn's link must survive");
        assert_eq!(region.url, "https://example.com/origin");
        assert!(
            region.content_row < FORMER_TAIL_CAP,
            "the surviving link must really sit above the old cut (row={})",
            region.content_row
        );
        let row = &rendered.text.lines[region.content_row];
        let text: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(&text[region.col_start..region.col_end], "origin");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn table_cells_are_inline_parsed_and_stay_aligned() {
        // Cells contain inline code and bold; the markers must be STRIPPED from
        // the display text, and columns must stay aligned because width is
        // measured on the stripped text (`**bold**` is 4 columns, not 8).
        let body = "| A | B |\n| --- | --- |\n| `code` | **bold** |\n| x | y |";
        let lines = markdown_body_lines(body, WIDE);
        assert_eq!(
            lines.len(),
            5,
            "header + separator + 2 body rows + 1 row rule"
        );

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

    /// The 3-column fixture both narrow-width tests below are stated over. Each
    /// column's natural width is 7 (`1111111`), so `min_grid_width` is
    /// `7 + 7 + 7 + 2 * 3 = 27`: at 27 columns or more it is a GRID, below that the
    /// stacked record layout.
    const NARROW_TABLE: &str = "| alpha | beta | gamma |\n| --- | --- | --- |\n\
                                | 1111111 | 2222222 | 3333333 |\n\
                                | 4444444 | 5555555 | 6666666 |";

    #[test]
    fn narrow_width_grid_table_never_exceeds_the_pane_width() {
        // In GRID mode every produced line must fit within `width` — guaranteeing
        // it can never soft-wrap under `Wrap { trim: false }` and scatter the grid.
        // Re-scoped to grid mode: below `min_grid_width` (27) the same table routes
        // to record mode, where a line is ALLOWED to exceed the width (see
        // `record_mode_lines_may_exceed_the_pane_width`).
        for width in [27usize, 30, 48, 96] {
            let lines = markdown_body_lines(NARROW_TABLE, width);
            let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
            assert!(
                joined.contains('\u{2502}'),
                "width {width} must still be a grid, or this proves nothing: {joined}"
            );
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

    #[test]
    fn record_mode_lines_may_exceed_the_pane_width() {
        // The companion to the grid guarantee above, and the INVERSE of it. Record
        // lines are ordinary logical lines that ratatui's `Wrap { trim: false }`
        // wraps, so they are deliberately NOT clamped — clamping them would
        // reintroduce exactly the cut the record layout exists to remove.
        for width in [8usize, 16, 24] {
            let lines = markdown_body_lines(NARROW_TABLE, width);
            let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
            assert!(
                !joined.contains('\u{2502}') && !joined.contains('\u{253c}'),
                "width {width} routes to record mode, so no grid chrome: {joined}"
            );
            assert!(
                !joined.contains('\u{2026}'),
                "nothing is cut at width {width}: {joined}"
            );
            assert!(
                lines.iter().any(|l| display_width(&line_text(l)) > width),
                "a record line is allowed to exceed width {width}: {joined}"
            );
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
        let text = render_file(&file, WIDE);
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

    // --- bound agent handle -----------------------------------------------

    #[test]
    fn agent_handle_suppresses_default_and_blank_names() {
        // A real agent yields an `@handle`.
        assert_eq!(agent_handle(Some("lead")).as_deref(), Some("@lead"));
        // The catch-all default is already named by the `● claude` marker.
        assert_eq!(agent_handle(Some("claude")), None);
        // Blank / whitespace-only / absent all suppress.
        assert_eq!(agent_handle(Some("")), None);
        assert_eq!(agent_handle(Some("   ")), None);
        assert_eq!(agent_handle(None), None);
        // A padded name is trimmed before the handle is built.
        assert_eq!(agent_handle(Some("  lead  ")).as_deref(), Some("@lead"));
    }

    #[test]
    fn claude_marker_carries_the_bound_agent_handle() {
        // The worktree fixture binds `lead` (line 2) ahead of its assistant turn.
        let text = render_file(
            &fixture(
                "-Users-me-acme-web-worktrees-feature-x",
                "sess-worktree-1.jsonl",
            ),
            WIDE,
        );
        let claude = line_led_by(&text, "\u{25cf} claude").expect("a claude marker line");
        // Exact span ORDER: `● claude` (bold) · @lead (dim) · timestamp (dim).
        assert_eq!(claude.spans.len(), 3, "marker + handle + timestamp");
        assert_eq!(claude.spans[0].content.as_ref(), "\u{25cf} claude");
        assert!(
            claude.spans[0].style.add_modifier.contains(Modifier::BOLD),
            "the marker stays bold"
        );
        assert_eq!(claude.spans[1].content.as_ref(), " \u{b7} @lead");
        assert!(
            claude.spans[1].style.add_modifier.contains(Modifier::DIM),
            "the handle is dim"
        );
        // The timestamp annotation still follows the handle (same day as the user
        // turn, so `HH:MM`).
        assert_eq!(claude.spans[2].content.as_ref(), " \u{b7} 07:00");
        assert!(
            claude.spans[2].style.add_modifier.contains(Modifier::DIM),
            "the timestamp is dim"
        );
    }

    #[test]
    fn a_late_agent_setting_does_not_label_earlier_turns() {
        // The `eec8fc7c` store shape: the ONLY `agent-setting` sits AFTER the
        // assistant turn it would (wrongly) attribute. Positional threading must
        // leave that earlier turn BARE — this fails loudly if someone "fixes" the
        // design by hoisting the file's agent.
        let dir = unique_temp_dir("late-agent");
        let file = dir.join("sess.jsonl");
        let jsonl = concat!(
            r#"{"type":"assistant","sessionId":"s","cwd":"/x","timestamp":"2026-07-04T07:00:00.000Z","message":{"role":"assistant","content":"early turn"}}"#,
            "\n",
            r#"{"type":"agent-setting","agentSetting":"lead","sessionId":"s"}"#,
            "\n",
        );
        std::fs::write(&file, jsonl).expect("write temp jsonl");

        let text = render_file(&file, WIDE);
        let claude = line_led_by(&text, "\u{25cf} claude").expect("a claude marker line");
        // Marker + timestamp only — NO handle span between them.
        assert_eq!(
            claude.spans.len(),
            2,
            "an agent set after the turn must not label it: {:?}",
            claude.spans
        );
        assert!(
            !claude.spans.iter().any(|s| s.content.contains('@')),
            "no handle on a turn that precedes its agent-setting"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_malformed_agent_setting_never_panics_and_renders_no_handle() {
        // Missing field, null, a number, and an empty string must all FAIL SOFT to
        // no binding — never a panic, never a stray handle.
        let dir = unique_temp_dir("bad-agent");
        let file = dir.join("sess.jsonl");
        let jsonl = concat!(
            r#"{"type":"agent-setting","sessionId":"s"}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"s","cwd":"/x","message":{"role":"assistant","content":"a"}}"#,
            "\n",
            r#"{"type":"agent-setting","agentSetting":null,"sessionId":"s"}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"s","cwd":"/x","message":{"role":"assistant","content":"b"}}"#,
            "\n",
            r#"{"type":"agent-setting","agentSetting":42,"sessionId":"s"}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"s","cwd":"/x","message":{"role":"assistant","content":"c"}}"#,
            "\n",
            r#"{"type":"agent-setting","agentSetting":"","sessionId":"s"}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"s","cwd":"/x","message":{"role":"assistant","content":"d"}}"#,
            "\n",
        );
        std::fs::write(&file, jsonl).expect("write temp jsonl");

        let text = render_file(&file, WIDE);
        let markers: Vec<&Line> = text
            .lines
            .iter()
            .filter(|l| l.spans.first().map(|s| s.content.as_ref()) == Some("\u{25cf} claude"))
            .collect();
        assert_eq!(markers.len(), 4, "all four assistant turns render");
        for m in markers {
            assert!(
                !m.spans.iter().any(|s| s.content.contains('@')),
                "a malformed agent-setting must render no handle: {:?}",
                m.spans
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_default_agent_renders_a_bare_claude_marker() {
        // `agentSetting: "claude"` is the catch-all default; the `● claude` marker
        // already names it, so it must never render `● claude · @claude`.
        let dir = unique_temp_dir("default-agent");
        let file = dir.join("sess.jsonl");
        let jsonl = concat!(
            r#"{"type":"agent-setting","agentSetting":"claude","sessionId":"s"}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"s","cwd":"/x","message":{"role":"assistant","content":"hi"}}"#,
            "\n",
        );
        std::fs::write(&file, jsonl).expect("write temp jsonl");

        let text = render_file(&file, WIDE);
        let claude = line_led_by(&text, "\u{25cf} claude").expect("a claude marker line");
        assert_eq!(
            claude.spans.len(),
            1,
            "the default agent adds no handle span: {:?}",
            claude.spans
        );
        assert!(
            !flatten(&text).contains("@claude"),
            "the default agent must never render `@claude`"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- agent-name fallback (validated) ----------------------------------

    #[test]
    fn agent_state_precedence_prefers_the_interactive_bind() {
        let mut s = AgentState::default();
        assert_eq!(s.effective(), None, "nothing set => no agent");
        s.job = Some("technical-brainstormer".to_string());
        assert_eq!(
            s.effective(),
            Some("technical-brainstormer"),
            "the validated job name is used when there is no interactive bind"
        );
        s.bound = Some("lead".to_string());
        assert_eq!(
            s.effective(),
            Some("lead"),
            "an interactive agent-setting bind wins over the job name"
        );
        s.bound = None;
        assert_eq!(
            s.effective(),
            Some("technical-brainstormer"),
            "clearing the bind falls back to the job name"
        );
    }

    #[test]
    fn a_known_agent_name_labels_the_turn_when_there_is_no_agent_setting() {
        // The background-agent shape: no `agent-setting`, only `agent-name`. When it
        // names a KNOWN agent the turn is labeled from it.
        let dir = unique_temp_dir("known-agent-name");
        let file = dir.join("sess.jsonl");
        let jsonl = concat!(
            r#"{"type":"agent-name","agentName":"lead","sessionId":"s"}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"s","cwd":"/x","message":{"role":"assistant","content":"hi"}}"#,
            "\n",
        );
        std::fs::write(&file, jsonl).expect("write temp jsonl");

        let text = render_file_known(&file, WIDE, &["lead", "technical-brainstormer"]);
        let claude = line_led_by(&text, "\u{25cf} claude").expect("a claude marker line");
        assert_eq!(claude.spans.len(), 2, "marker + handle: {:?}", claude.spans);
        assert_eq!(claude.spans[1].content.as_ref(), " \u{b7} @lead");
        assert!(claude.spans[1].style.add_modifier.contains(Modifier::DIM));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unknown_agent_name_is_a_title_and_renders_bare() {
        // `agent-name` also carries free-form job titles; one that is NOT a known
        // agent must never render as a handle. THIS is the guard that keeps
        // `● claude · @Drop initial commit bcdc05e` off the screen.
        let dir = unique_temp_dir("title-agent-name");
        let file = dir.join("sess.jsonl");
        let jsonl = concat!(
            r#"{"type":"agent-name","agentName":"Drop initial commit bcdc05e","sessionId":"s"}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"s","cwd":"/x","message":{"role":"assistant","content":"hi"}}"#,
            "\n",
        );
        std::fs::write(&file, jsonl).expect("write temp jsonl");

        let text = render_file_known(&file, WIDE, &["lead"]);
        let claude = line_led_by(&text, "\u{25cf} claude").expect("a claude marker line");
        assert_eq!(
            claude.spans.len(),
            1,
            "a free-form title must add no handle span: {:?}",
            claude.spans
        );
        assert!(!flatten(&text).contains('@'), "no handle anywhere");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn agent_setting_wins_over_a_concurrent_agent_name() {
        // A session carrying BOTH: `agent-setting` (the interactive bind) is
        // authoritative even when `agent-name` also names a known agent.
        let dir = unique_temp_dir("both-agents");
        let file = dir.join("sess.jsonl");
        let jsonl = concat!(
            r#"{"type":"agent-setting","agentSetting":"lead","sessionId":"s"}"#,
            "\n",
            r#"{"type":"agent-name","agentName":"technical-brainstormer","sessionId":"s"}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"s","cwd":"/x","message":{"role":"assistant","content":"hi"}}"#,
            "\n",
        );
        std::fs::write(&file, jsonl).expect("write temp jsonl");

        let text = render_file_known(&file, WIDE, &["lead", "technical-brainstormer"]);
        let claude = line_led_by(&text, "\u{25cf} claude").expect("a claude marker line");
        assert_eq!(claude.spans[1].content.as_ref(), " \u{b7} @lead");
        assert!(
            !flatten(&text).contains("technical-brainstormer"),
            "the agent-setting bind wins; the job name must not appear"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_malformed_agent_name_never_panics_and_renders_no_handle() {
        // Missing field, null, a number, and an empty string must all FAIL SOFT.
        let dir = unique_temp_dir("bad-agent-name");
        let file = dir.join("sess.jsonl");
        let jsonl = concat!(
            r#"{"type":"agent-name","sessionId":"s"}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"s","cwd":"/x","message":{"role":"assistant","content":"a"}}"#,
            "\n",
            r#"{"type":"agent-name","agentName":null,"sessionId":"s"}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"s","cwd":"/x","message":{"role":"assistant","content":"b"}}"#,
            "\n",
            r#"{"type":"agent-name","agentName":42,"sessionId":"s"}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"s","cwd":"/x","message":{"role":"assistant","content":"c"}}"#,
            "\n",
            r#"{"type":"agent-name","agentName":"","sessionId":"s"}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"s","cwd":"/x","message":{"role":"assistant","content":"d"}}"#,
            "\n",
        );
        std::fs::write(&file, jsonl).expect("write temp jsonl");

        let text = render_file_known(&file, WIDE, &["lead", ""]);
        let markers: Vec<&Line> = text
            .lines
            .iter()
            .filter(|l| l.spans.first().map(|s| s.content.as_ref()) == Some("\u{25cf} claude"))
            .collect();
        assert_eq!(markers.len(), 4, "all four turns render");
        for m in markers {
            assert!(
                !m.spans.iter().any(|s| s.content.contains('@')),
                "a malformed agent-name renders no handle: {:?}",
                m.spans
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- answering model label --------------------------------------------

    #[test]
    fn model_label_strips_the_vendor_prefix_and_suppresses_pseudo_models() {
        // A normal id: the vendor prefix goes, nothing else does.
        assert_eq!(
            model_label(Some("claude-sonnet-5")).as_deref(),
            Some("sonnet-5")
        );
        // A DATED id keeps its date — collapsing it would render two different
        // releases identically, which is worse than a long label.
        assert_eq!(
            model_label(Some("claude-haiku-4-5-20251001")).as_deref(),
            Some("haiku-4-5-20251001")
        );
        // An id with NO vendor prefix (the bare aliases that also appear on disk)
        // is rendered unchanged.
        assert_eq!(model_label(Some("opus")).as_deref(), Some("opus"));
        // `<synthetic>` names no model that answered — it rides records Claude Code
        // injects itself — so it is suppressed rather than labeled.
        assert_eq!(model_label(Some("<synthetic>")), None);
        // ANY `<…>`-wrapped pseudo-model, not just the one we have seen on disk.
        assert_eq!(model_label(Some("<unknown>")), None);
        // Absent / empty / whitespace-only all suppress.
        assert_eq!(model_label(None), None);
        assert_eq!(model_label(Some("")), None);
        assert_eq!(model_label(Some("   ")), None);
        // A padded id is trimmed before the prefix is stripped.
        assert_eq!(
            model_label(Some("  claude-opus-5  ")).as_deref(),
            Some("opus-5")
        );
        // A bare prefix and nothing else renders no label, never an empty ` · `.
        assert_eq!(model_label(Some("claude-")), None);
    }

    #[test]
    fn record_model_is_fail_soft_over_absent_null_and_non_string() {
        let present = serde_json::json!({"message": {"model": "claude-opus-5"}});
        assert_eq!(record_model(&present), Some("claude-opus-5"));
        // An absent `message`, an absent `model`, a null, a non-string, and a
        // non-object `message` must ALL read as no model — never a panic.
        for raw in [
            serde_json::json!({}),
            serde_json::json!({"message": {}}),
            serde_json::json!({"message": {"model": null}}),
            serde_json::json!({"message": {"model": 42}}),
            serde_json::json!({"message": "not an object"}),
        ] {
            assert_eq!(record_model(&raw), None, "{raw} must read as no model");
        }
    }

    #[test]
    fn the_claude_marker_carries_the_answering_model_between_handle_and_time() {
        let text = render_file(&preview_fixture("sess-model-synthetic-tail-1.jsonl"), WIDE);
        let claude = lines_led_by(&text, "\u{25cf} claude");
        let first = claude.first().expect("a claude marker line");
        // Exact span ORDER: `● claude` (bold) · @lead · opus-4-8 · timestamp (dim).
        assert_eq!(
            first.spans.len(),
            4,
            "marker + handle + model + timestamp: {:?}",
            first.spans
        );
        assert_eq!(first.spans[0].content.as_ref(), "\u{25cf} claude");
        assert_eq!(first.spans[1].content.as_ref(), " \u{b7} @lead");
        assert_eq!(first.spans[2].content.as_ref(), " \u{b7} opus-4-8");
        assert!(
            first.spans[2].style.add_modifier.contains(Modifier::DIM),
            "the model label is dim"
        );
        assert_eq!(first.spans[3].content.as_ref(), " \u{b7} 10:00");
        // Styled with ratatui only — never embedded ANSI.
        assert!(
            !flatten(&text).contains('\u{1b}'),
            "the model label must not embed ANSI"
        );
    }

    #[test]
    fn a_synthetic_model_turn_renders_bare() {
        // The `<synthetic>` pseudo-model is the LAST assistant model in this
        // fixture — exactly where a naive "latest model" label would render it.
        let text = render_file(&preview_fixture("sess-model-synthetic-tail-1.jsonl"), WIDE);
        let claude = lines_led_by(&text, "\u{25cf} claude");
        assert_eq!(claude.len(), 2, "both assistant turns render");
        let last = claude[1];
        // Marker + handle + timestamp only — NO model span between them.
        assert_eq!(
            last.spans.len(),
            3,
            "a pseudo-model adds no label span: {:?}",
            last.spans
        );
        assert_eq!(last.spans[1].content.as_ref(), " \u{b7} @lead");
        assert_eq!(last.spans[2].content.as_ref(), " \u{b7} 10:02");
        assert!(
            !flatten(&text).contains("synthetic"),
            "the pseudo-model must never reach the screen"
        );
    }

    #[test]
    fn a_two_model_session_labels_each_turn_with_its_own_model() {
        // THE anti-hoist test: this session genuinely changes model mid-file (real
        // sessions do), so a file-level "the session's model" would be false for one
        // of these two turns whichever value it picked.
        let text = render_file(&preview_fixture("sess-model-switch-1.jsonl"), WIDE);
        let claude = lines_led_by(&text, "\u{25cf} claude");
        assert_eq!(claude.len(), 2, "both assistant turns render");
        assert_eq!(claude[0].spans[1].content.as_ref(), " \u{b7} opus-4-8");
        assert_eq!(claude[1].spans[1].content.as_ref(), " \u{b7} sonnet-5");
        // The label rides the ASSISTANT record, so a user turn never carries one.
        for you in lines_led_by(&text, "\u{25b6} you") {
            assert_eq!(
                you.spans.len(),
                2,
                "a user turn is marker + timestamp only: {:?}",
                you.spans
            );
        }
    }

    #[test]
    fn a_turn_with_no_model_renders_exactly_as_it_did_before() {
        // 21% of real sessions carry no `message.model` at all — the NORMAL case,
        // which must render byte-identically to the pre-label marker line.
        let text = render_file(&preview_fixture("sess-model-absent-1.jsonl"), WIDE);
        let claude = lines_led_by(&text, "\u{25cf} claude");
        assert_eq!(claude.len(), 2, "both assistant turns render");
        // Marker + timestamp, exactly as before this label existed.
        assert_eq!(
            claude[0]
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<Vec<_>>(),
            vec!["\u{25cf} claude", " \u{b7} 10:00"],
        );
        assert_eq!(
            claude[1]
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<Vec<_>>(),
            vec!["\u{25cf} claude", " \u{b7} 10:00"],
        );
    }

    #[test]
    fn a_malformed_model_never_panics_and_renders_no_label() {
        // A missing `message`, a null, a number, an empty string, and a `<…>`
        // pseudo-model must all FAIL SOFT to no label. No timestamps here, so a
        // bare marker line is exactly ONE span and a stray annotation is loud.
        let dir = unique_temp_dir("bad-model");
        let file = dir.join("sess.jsonl");
        let jsonl = concat!(
            r#"{"type":"assistant","sessionId":"s","cwd":"/x","message":{"role":"assistant","content":"a"}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"s","cwd":"/x","message":{"role":"assistant","model":null,"content":"b"}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"s","cwd":"/x","message":{"role":"assistant","model":42,"content":"c"}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"s","cwd":"/x","message":{"role":"assistant","model":"","content":"d"}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"s","cwd":"/x","message":{"role":"assistant","model":"<synthetic>","content":"e"}}"#,
            "\n",
        );
        std::fs::write(&file, jsonl).expect("write temp jsonl");

        let text = render_file(&file, WIDE);
        let markers = lines_led_by(&text, "\u{25cf} claude");
        assert_eq!(markers.len(), 5, "all five assistant turns render");
        for m in markers {
            assert_eq!(
                m.spans.len(),
                1,
                "a malformed model renders no label: {:?}",
                m.spans
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

        let text = render_file(&file, WIDE);
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
