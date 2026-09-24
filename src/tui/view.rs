//! View rendering.
//!
//! Draws the two-pane layout: a session list on the left, a readable transcript
//! preview on the right — divided by the app's `PaneLayout`, five stops from a
//! full-width preview to a full-width list — plus a header/help line and a search
//! input line. The
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

use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Widget, Wrap,
};
use ratatui::Frame;
use time::OffsetDateTime;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::agents::{self, AgentActivity, ReportedAgent};
use crate::resume::ModelPick;
use crate::search::SearchMode;
use crate::store::preview::{self, FoldRegion, LinkRegion};
use crate::store::FailedTask;

use super::app::{
    resolve_list_width, App, ComposeDefault, InterruptRoute, Modal, ModalAction, ModalChoice,
    ModalLayout, NewSessionDraft, PaneLayout, Row, Scope, MODEL_DEFAULT_LABEL,
    MODEL_NEW_SESSION_SCOPE,
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
    // Borrowed MUTABLY for the same reason the list is: a `List` modal's scroll
    // window is resolved against the clamped box and written back (`Modal::scroll`).
    if let Some(modal) = app.modal.as_mut() {
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

/// How a compose box's `model:` label draws the model its launch will run on: a
/// `Ctrl-L` pick (with its effort), and the default shown while none is picked —
/// `session (<model>)`, `default (<value>)` or `default` — alike. One constant for
/// all of them, so they cannot drift to different colours; a pick and the default
/// are told apart by their words, not by colour. The `(new sessions only)` scope
/// after a draft's settings value is NOT in it (see [`compose_model_label`]). The
/// model picker draws a row's set effort in it too ([`modal_effort_span`]), so a
/// level reads the same in the picker as in the label it lands in. A named ANSI
/// colour only (TERMINAL-SAFE STYLING).
const MODEL_LABEL_STYLE: Style = Style::new().fg(Color::Magenta);

/// What joins a model and its effort — `opus · high` — in a compose's `model:`
/// label and on a model-picker row. The middot with ONE space either side is the
/// preview's turn marker separator (`● claude · Opus 5.5 · xhigh`), so a picked
/// effort reads exactly like the effort a turn records; it is deliberately tighter
/// than [`HEADER_SEPARATOR`], which parts whole header SEGMENTS rather than the two
/// halves of one value.
const MODEL_EFFORT_SEPARATOR: &str = " · ";

/// What a compose's `model:` label calls a REPLY's default — the model its session
/// last answered with, which claude normally restores with no `--model`:
/// `model: session (Opus 5.5)`. Terse, because the label shares a border with the
/// box it sits on; the picker's first row spells the same state out in full
/// (`session's model (Opus 5.5)`, [`super::app::MODEL_SESSION_ROW_LABEL`]).
const MODEL_SESSION_LABEL: &str = "session";

/// What a HIGHLIGHTED model-picker row shows while its effort is UNSET: no
/// `--effort` is sent, so claude applies the model's own level (the user's
/// settings, else its built-in default). Shown only on the highlighted row, so the
/// cycle's unset stop is visible where `←`/`→` act without cluttering every other
/// row. It names no level on purpose — which one the settings would apply is not
/// something the board reads.
const EFFORT_UNSET_LABEL: &str = "default effort";

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

/// The marker a background session wears when it lost the agent BINDING its own
/// fork lineage root still carries — the anthropics/claude-code#80811 signature
/// (see [`lineage::lost_agent_bindings`](crate::store::lineage::lost_agent_bindings)).
///
/// # What it asserts, and what it deliberately does not
///
/// EXACTLY this: the transcript is `sessionKind: bg`, it carries an `agent-name`,
/// it carries no `agent-setting`, and the oldest member of its lineage does. That
/// is an observation about two files on disk, nothing more.
///
/// It is NOT a recovery instruction and carries NO remediation payload. A prior
/// attempt at this badge shipped one — a `SendMessage` action that agent-bound
/// sessions structurally cannot execute — and the wording is kept a plain state
/// ("unbound") rather than an imperative so it cannot drift back into advice. No
/// key is bound to it; it is a badge.
///
/// # Width
///
/// Deliberately terse — eleven columns, the same order as its neighbours
/// `[hidden]` and `(+N)` — because a folded, badged row wears BOTH trailing
/// markers at once and a narrow pane must still fit the pair beside the
/// timestamp. A longer phrase ("agent unbound") does not fit an 80-column board's
/// worst case, and a badge that gets clipped to `[agent unboun` asserts nothing.
/// The precise claim lives in this doc comment, not in the eleven columns.
///
/// Carries its own leading gap, exactly as [`HIDDEN_ROW_MARKER`] and
/// [`LINEAGE_MARKER_GAP`] do, so the width reserved is the width drawn.
const AGENT_UNBOUND_MARKER: &str = "  [unbound]";

/// The marker a session row wears while a background task it launched stands
/// FAILED and the user has not written into the session since (see
/// [`Session::failed_task`](crate::store::Session::failed_task)).
///
/// # What it asserts
///
/// EXACTLY this: claude delivered a `failed` task notification into this
/// transcript, and no later record in it is the user's own prompt, typed or a
/// quick reply. The precise account is claude's own `<summary>`, which the
/// preview banner quotes when the notice carries one ([`failed_task_banner`]);
/// the row only says where to look.
///
/// "task", not "agent", because the notification is claude's `task-notification`
/// and it reports background SHELL commands as well as agents — a real store
/// holds a `Background command "…" failed with exit code 126` beside the agent
/// stalls. A plain state rather than an imperative, like [`AGENT_UNBOUND_MARKER`],
/// so it cannot drift into advice. It is a marker only: no key is bound to it and
/// it gates nothing.
///
/// # Width
///
/// Two words, because one (`[failed]`) reads as a verdict on the SESSION, which
/// did not fail. Like every trailing marker it is reserved BEFORE the label and
/// dropped whole rather than clipped, so the extra columns cost label, never a
/// half-drawn `[task fai`.
///
/// Carries its own leading gap, exactly as [`AGENT_UNBOUND_MARKER`] does, so the
/// width reserved is the width drawn.
const FAILED_TASK_MARKER: &str = "  [task failed]";

/// What the preview banner says ahead of claude's own words when the selected
/// session carries a failed background task (see [`failed_task_banner`]).
///
/// Lower-case board voice, matching the reported-agent status (`bg done`) the same
/// pinned row can fall back to, and worded for the task rather than the agent for
/// the reason [`FAILED_TASK_MARKER`] gives.
const FAILED_TASK_BANNER_LEAD: &str = "background task failed";

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

/// Rows the PINNED banner (the sticky header) reserves at the top of the preview's
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

/// A `Ctrl-L` pick as a compose label names it: the model alone, or
/// `<model> · <effort>` when the pick carries an effort — the effort as the bare
/// level, the way a turn marker shows the effort a turn ran at. Pure, so the
/// wording is asserted without a terminal.
#[must_use]
fn model_pick_label(pick: &ModelPick) -> String {
    match pick.effort {
        Some(level) => format!("{}{MODEL_EFFORT_SEPARATOR}{level}", pick.model),
        None => pick.model.clone(),
    }
}

/// The `model: …` label a compose box carries on its bottom border: what THIS
/// reply or draft will run on, as a line of spans. `pick` is the compose's own
/// `Ctrl-L` pick and `default` what it runs on without one
/// ([`App::compose_default`]).
///
/// * A pick reads `model: <alias>` or `model: <alias> · <effort>`
///   ([`model_pick_label`]).
/// * A reply's default reads `model: session (<model>)` — the model its session
///   last answered with, which claude normally restores — or `model: default` when
///   claude would not restore it (an environment override, or no answering model
///   on record).
/// * A draft's default reads `model: default (<value>) (new sessions only)` with the
///   settings' model, or bare `model: default` with none.
///
/// The value — the part that NAMES a model, `default` included — is drawn in
/// [`MODEL_LABEL_STYLE`]; the `model: ` prefix and the `(new sessions only)` scope
/// ([`MODEL_NEW_SESSION_SCOPE`]) are separate UNSTYLED spans, leading space
/// included, so they take the box's own border colour and only the value carries the
/// model's. Padded with a space either side, the way the box's top title is.
///
/// It is STATE for the compose's whole lifetime, so it renders on the compose box
/// that owns it and never on `App::status` (AGENTS.md STATUS-LINE OWNERSHIP). Pure,
/// so the wording AND the styling are asserted without a terminal.
#[must_use]
fn compose_model_label(pick: Option<&ModelPick>, default: &ComposeDefault) -> Line<'static> {
    let mut spans = vec![Span::raw(" model: ")];
    match (pick, default) {
        (Some(pick), _) => spans.push(Span::styled(model_pick_label(pick), MODEL_LABEL_STYLE)),
        (None, ComposeDefault::SessionModel(label)) => spans.push(Span::styled(
            format!("{MODEL_SESSION_LABEL} ({label})"),
            MODEL_LABEL_STYLE,
        )),
        (None, ComposeDefault::Settings(value)) => {
            spans.push(Span::styled(
                format!("{MODEL_DEFAULT_LABEL} ({value})"),
                MODEL_LABEL_STYLE,
            ));
            spans.push(Span::raw(format!(" {MODEL_NEW_SESSION_SCOPE}")));
        }
        (
            None,
            ComposeDefault::RestoreOverridden
            | ComposeDefault::NoSessionModel
            | ComposeDefault::BuiltIn,
        ) => spans.push(Span::styled(MODEL_DEFAULT_LABEL, MODEL_LABEL_STYLE)),
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

/// The body: grouped list on the left, preview on the right, divided by the
/// app's [`PaneLayout`] — one of five stops `Shift-←` / `Shift-→` walk.
///
/// A SPLIT stop is a share of the body, turned into columns HERE, against THIS
/// frame's `area.width`, by [`resolve_list_width`] (mirroring how `render_preview`
/// re-clamps `preview_scroll` rather than trusting a stale value) — so no stored
/// width exists for a terminal resize to leave degenerate. The two single-pane
/// stops give the other pane nothing at all: its rect is left EMPTY, which never
/// matches a hit-test, so a wheel anywhere over the body reaches the pane that
/// is actually there.
fn render_body(frame: &mut Frame, app: &mut App, area: Rect) {
    match app.pane_layout() {
        PaneLayout::PreviewOnly => {
            // 0:1 — the preview owns the whole body and the list is not drawn.
            // The selection still moves (`↑`/`↓` act regardless), so the preview
            // names the row it is showing in its title instead (`preview_title`).
            app.list_rect = Rect::default();
            app.preview_rect = area;
            render_preview(frame, app, area);
        }
        PaneLayout::ListOnly => {
            // 1:0 — the list owns the whole body and the preview is not drawn.
            app.list_rect = area;
            app.preview_rect = Rect::default();
            render_list(frame, app, area);
            // `render_preview` is the only consumer of BOTH preview one-shots, and
            // it does not run here — so this frame is where a request armed with no
            // pane on screen has to die. Left armed, a match jump would fire on the
            // frame the pane comes BACK on, overriding the newest-turn anchor the
            // layout setter just set, for a query the user typed before they
            // re-opened the pane. Dropping it HERE (rather than refusing to arm it)
            // covers every route into the flag, including the explicit
            // `Shift`-arrow step. The reading-position anchor is dropped for the
            // same reason, though the setter never leaves one pending into this
            // layout today.
            let _ = app.take_preview_match_jump();
            let _ = app.take_preview_anchor();
        }
        split @ (PaneLayout::PreviewWide | PaneLayout::Even | PaneLayout::ListWide) => {
            let list_cols = resolve_list_width(split, area.width);
            let [list_area, preview_area] =
                Layout::horizontal([Constraint::Length(list_cols), Constraint::Fill(1)])
                    .areas(area);
            // Persist the pane rects so a mouse wheel or a click (a fold node or
            // a link) can be hit-tested against a pane.
            app.list_rect = list_area;
            app.preview_rect = preview_area;
            render_list(frame, app, list_area);
            render_preview(frame, app, preview_area);
        }
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
    let highlights: HashMap<usize, HashSet<usize>> = if app.query().is_empty() {
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
                    // A failed background task is a fact about THIS file, so a
                    // child row draws it too — and a child is exactly where an
                    // inherited one shows: a background fork copies its parent's
                    // transcript, flag included, until its own first prompt.
                    // Dropped only if the row has no room, exactly as the turn
                    // count above is, and decided FIRST because it is the marker
                    // that wants the user.
                    let used: usize = spans.iter().map(Span::width).sum();
                    if session.failed_task.is_some()
                        && used + FAILED_TASK_MARKER.chars().count() <= content_width
                    {
                        spans.push(failed_task_marker_span());
                    }
                    // A downgraded member is not always the head: the fork is
                    // usually NEWER than its root and so heads the lineage, but a
                    // third member can push it into a child row. The badge is a
                    // fact about the SESSION, not about its position in the fold,
                    // so it draws here too — dropped only if the row has no room,
                    // exactly as the turn count above is.
                    let used: usize = spans.iter().map(Span::width).sum();
                    if app.lost_agent_bindings.contains(&session.session_id)
                        && used + AGENT_UNBOUND_MARKER.chars().count() <= content_width
                    {
                        spans.push(unbound_marker_span());
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
                let fold_width = marker.as_ref().map_or(0, |m| m.chars().count());
                let used: usize = spans.iter().map(Span::width).sum();
                // The failed-task marker: a field of the session this row already
                // holds, so no lookup at all. DROPPED, never clipped, when the row
                // cannot fit it even with no label: a half-drawn `[task fai`
                // asserts nothing, and a marker that overruns pushes the row's
                // content off the edge. Same discipline as `fit_child_msgs` on the
                // child row. Decided BEFORE the #80811 badge because it is the one
                // that wants the user, so a pane with room for only one keeps it.
                let failed = session.failed_task.is_some()
                    && used + fold_width + FAILED_TASK_MARKER.chars().count() <= content_width;
                let failed_width = if failed {
                    FAILED_TASK_MARKER.chars().count()
                } else {
                    0
                };
                // The #80811 badge: a single lookup against the set derived once
                // per reload, using the id this row already holds. Never a scan.
                // Dropped whole on the same terms, against what is left.
                let unbound = app.lost_agent_bindings.contains(&session.session_id)
                    && used + fold_width + failed_width + AGENT_UNBOUND_MARKER.chars().count()
                        <= content_width;
                // EVERY trailing marker is reserved in ONE budget, so the label
                // is what gives way and no marker can be shoved off the edge —
                // the same discipline `(+N)` alone already followed.
                let reserved = fold_width
                    + failed_width
                    + if unbound {
                        AGENT_UNBOUND_MARKER.chars().count()
                    } else {
                        0
                    };
                let label = if reserved > 0 {
                    // The SAME `used` the reservation above was decided against —
                    // nothing has pushed to `spans` in between, so re-summing it
                    // could only ever produce that identical width.
                    fit_label(&session.label, content_width, used, reserved)
                } else {
                    session.label.clone()
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
                if failed {
                    spans.push(failed_task_marker_span());
                }
                if unbound {
                    spans.push(unbound_marker_span());
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

/// The styled [`AGENT_UNBOUND_MARKER`] span, built in ONE place so the head row
/// and a child row can never draw the same fact in two different styles.
///
/// `Yellow` — the board's established CAUTION color (the group header and the
/// `NeedsInput` badge already speak it), so the badge reads as "something here is
/// off" in the vocabulary the user already has. Deliberately NOT `DIM`: dim is
/// this row's FOOTNOTE weight, worn by the timestamp, the `(+N)` marker and
/// `[hidden]`, and a real defect is not a footnote. Equally deliberately not
/// `BOLD` — the bold-yellow weight belongs to the group header and the agent
/// badge, and a trailing marker must not outrank the label it sits beside.
///
/// TERMINAL-SAFE STYLING: a NAMED ANSI color, never RGB and never an embedded
/// escape (AGENTS.md).
fn unbound_marker_span() -> Span<'static> {
    Span::styled(AGENT_UNBOUND_MARKER, Style::default().fg(Color::Yellow))
}

/// The color that says "a background task failed" — the row marker and the
/// banner sentence alike, so the two surfaces read as one fact.
///
/// `Red`: the board's one accent for "this wants you" (the `NeedsInput` glyph,
/// [`BADGE_NEEDS_INPUT_COLOR`], already speaks it), and a failure nobody has
/// looked at yet is exactly that. A NAMED ANSI color, never RGB (TERMINAL-SAFE
/// STYLING).
const FAILED_TASK_COLOR: Color = Color::Red;

/// The styled [`FAILED_TASK_MARKER`] span, built in ONE place so the head row
/// and a child row can never draw the same fact in two different styles.
///
/// Deliberately NOT `DIM` — dim is this row's footnote weight, and an unanswered
/// failure is not a footnote — and deliberately not `BOLD`, so a trailing marker
/// does not outrank the label it sits beside (the same weighing as
/// [`unbound_marker_span`]).
fn failed_task_marker_span() -> Span<'static> {
    Span::styled(FAILED_TASK_MARKER, Style::default().fg(FAILED_TASK_COLOR))
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
/// ([`pulse_color`]), while the cursor swaps its `REVERSED` modifier on and off
/// ([`render_search`]). The name is the cursor's literal reading and the dot's
/// ON/OFF phase; anything animated later phases off this too.
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

/// The separator between the banner's status phrase and its age (`live busy · 46m`):
/// the same middot the draft card and the board header use between facts, so the
/// age reads as a second fact about the session and not as part of the qualifier.
const BANNER_AGE_SEPARATOR: &str = " \u{b7} ";

/// WHETHER the preview reserves its pinned banner row — and the FALLBACK line for
/// it — or `None` when the pane has no session transcript to pin a row above: nothing
/// is selected, a new-session draft card owns the pane, or a quick reply to the
/// selected session is in flight.
///
/// This answers the RESERVATION, which is the question both callers actually need.
/// What the row finally SHOWS is resolved in [`render_preview`], after the scroll
/// offset is known, by ONE precedence: a FAILED background task the session still
/// carries ([`failed_task_banner_line`]) outranks everything else and stands ALONE on
/// the row; otherwise the marker of the turn at the top of the viewport
/// ([`marker_at_top`]), followed — for a LIVE agent only — by its status and age
/// ([`marker_with_live_status`]). The line returned here reaches the screen only when
/// both miss — no standing failure, and a transcript with no marker at all — so it is
/// the session's reported status, with its age when one is known
/// ([`reported_status`]), when claude reports one, and an EMPTY row otherwise. The
/// remap is `Option::map`, so `is_some()` — the only thing the geometry depends on —
/// is preserved exactly.
///
/// Keyed on the SELECTION alone — never on the reported-agent set, on liveness, or
/// on whether the cached render holds markers. Why each of those is wrong is owned
/// by `docs/agents/PATTERNS.md` §5 (the `has_banner` rule); do not restate it here.
///
/// Read-only over state that already exists — the selected id (`App::selected`), the
/// draft and in-flight send, and, for the fallback, the existing `App::reported_agent`
/// accessor and the stamp its map arrived with — so there is no new `App` state, no
/// I/O, and no second interpretation of the `state`/`status` value set.
///
/// # The age (`live busy · 46m`)
///
/// When the record carries a `startedAt`, the phrase gains how long ago claude says
/// the session started ([`agents::elapsed_phrase`]), so a wedged child is legible
/// rather than indistinguishable from a healthy one. [`agents::friendly_status`]
/// still owns the phrase before the separator, which is composed beside it and never
/// re-derived here. Three rules hold it in place:
///
/// * **Its "now" is the POLL's, never a clock read here.** It subtracts the record's
///   `startedAt` from `App::reported_at_ms`, the wall-clock instant the poller
///   stamped this same map with. The age is therefore true "as of the last poll", and
///   this render path reads no clock. A few seconds of staleness cannot move an `m`/`h`
///   answer.
/// * **It reads the POLLED `--all` map, deliberately.** An age is a DISPLAY fact, so
///   the snapshot that draws badges is the right source. The `pid` a `Ctrl-K` signal
///   targets comes from the one-shot probe (`App::live_agent_now`) and never from
///   here: the pinned row asks only whether the polled record CARRIES one
///   ([`reports_live_process`]), never which. Two sources, two questions, and
///   neither may borrow the other's.
/// * **It lives in typed state and draws HERE, never on `App::status`** (STATUS-LINE
///   OWNERSHIP). An age is true over an interval, and the status line carries only
///   the outcome or refusal of a keypress.
///
/// It measures from the session's reported START (`startedAt`), not from when its
/// current turn or qualifier began: it is the session's age as claude reports it,
/// never a turn's. The two process shapes measured behind a `kind:"interactive"`
/// record (`docs/agents/DOMAIN.md`, "What `kind: "interactive"` denotes") read it
/// differently. At `claude 2.1.278` every record (11/11) was a `claude -p` child,
/// busy from its first instant, so the session's start and its one turn's start
/// coincide and a large age is a reply that has run that long. At `claude 2.1.280`
/// both records (2/2) were pty-backed TUIs, one `busy` and one `idle`. A TUI can stay
/// open across many turns, so there the two need not coincide: `46m` says the
/// session was reported started 46 minutes before the poll, not that any turn has
/// run that long. Under either shape the banner claims exactly the session's age and
/// nothing more. No age is drawn when there is nothing honest to state (no
/// `startedAt`, no stamp, or a start after the stamp): the fallback's status fact is
/// then exactly the phrase alone, and a row naming a turn marker carries no status at
/// all.
///
/// A known age is only HALF of what puts the status beside a turn marker. The other
/// half is that the agent is LIVE — its record carries a `pid`
/// ([`reports_live_process`]) — and only with both does the pinned row append
/// `<status> · <age>` after the marker it names ([`marker_with_live_status`]). An
/// age alone marks nothing live: a finished or parked record carries a `startedAt`
/// too. The fallback asks no such thing and states the age whenever it is known.
///
/// Exposed to `super::update` so the link hit-test can ask the SAME question the
/// view does — "does this pane have a banner?" — and derive the same transcript
/// rect via [`preview_split`]; the two must agree, or a click would resolve to the
/// wrong transcript row.
///
/// An IN-FLIGHT quick-reply send takes precedence: while `App::sending` names the
/// selected session there is NO pinned banner at all — this returns `None`, and
/// the `cooking…` placeholder renders INLINE at the transcript's tail instead
/// ([`sending_tail`]), so the exchange reads as ordinary turns. Returning `None`
/// is also what keeps the render and the click hit-test agreeing on the geometry.
///
/// That precedence is also why the two surfaces divide the way they do, and the
/// age depends on it. When the in-flight child is THIS board's, the tail already
/// says so, and the banner (with its age) steps aside. The age is for the other
/// case, where `App::sending` holds nothing for this session and the banner is all
/// the user gets. At `claude 2.1.278` the samples of 2026-09-21 and 2026-09-23
/// found that case dominant as a `claude -p` child that some OTHER snapback
/// instance dispatched, or
/// that outlived the board that did, so nothing here knew about the send. At
/// `claude 2.1.280` it was also a pty-backed TUI, which is never a quick reply and
/// so never in `App::sending` (see
/// `docs/agents/DOMAIN.md`, "What `kind: "interactive"` denotes"). So the `cooking…`
/// indicator is never copied into the banner,
/// and the age is never copied into the tail. Each fact is told once, on the
/// surface that owns it.
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
    // The fallback only: a session claude does not report still reserves the row,
    // and with no standing failure and no marker to pin it has nothing to say
    // there, so the row stays blank.
    Some(
        reported_status(app, selected)
            .map(|status| Line::from(banner_status_span(status.text)))
            .unwrap_or_default(),
    )
}

/// The SELECTED session's reported status as the pinned row states it: claude's
/// status in words ([`agents::friendly_status`]), then [`BANNER_AGE_SEPARATOR`] and
/// its age when one is known (see [`preview_banner`], "The age").
struct ReportedStatus {
    /// The phrase, with its age appended when [`aged`](Self::aged).
    text: String,
    /// Whether the age was known. One of the TWO facts that let the status join a
    /// turn marker on the pinned row ([`marker_with_live_status`]); the fallback
    /// line states the age whenever it is known, whatever [`live`](Self::live) says.
    aged: bool,
    /// Whether the record marks the agent LIVE ([`reports_live_process`]) — the
    /// other of those two facts. Read by the turn-marker suffix alone: the fallback
    /// line never asks it, so a finished session's fallback is unchanged by it.
    live: bool,
}

/// `selected`'s [`ReportedStatus`], or `None` when claude does not report it.
///
/// The ONE composition of the status and its age, so the fallback row and the
/// suffix a live agent's turn marker carries can never phrase them differently.
/// Read-only over the `App::reported_agent` accessor and `App::reported_at_ms`, the
/// stamp that same map arrived with: no clock, no I/O.
fn reported_status(app: &App, selected: &str) -> Option<ReportedStatus> {
    let agent = app.reported_agent(selected)?;
    let status = agents::friendly_status(agent);
    let live = reports_live_process(agent);
    // The age, measured against the stamp this same map arrived with. No clock
    // is read here, and anything with nothing honest to state leaves the phrase
    // alone.
    let age = app
        .reported_at_ms
        .and_then(|polled_at| agents::elapsed_phrase(agent.started_at_ms, polled_at));
    Some(match age {
        Some(age) => ReportedStatus {
            text: format!("{status}{BANNER_AGE_SEPARATOR}{age}"),
            aged: true,
            live,
        },
        None => ReportedStatus {
            text: status,
            aged: false,
            live,
        },
    })
}

/// Whether the pinned row treats `agent` as a LIVE agent — one whose status and age
/// ride after the turn marker ([`marker_with_live_status`]): claude reports an OS
/// process for the session, i.e. its record carries a
/// [`pid`](ReportedAgent::pid).
///
/// The ONE place the pinned row asks "is this agent live?". The `pid` is the wire's
/// per-record sign of a running process, and the age is not: every record carries a
/// `startedAt`, while a finished or parked one (`blocked`, `stopped`, `done`,
/// `failed`) carries no `pid` (the capture is in `docs/agents/DOMAIN.md`, "Reported
/// agents"). So it reads no [`agents::classify`] bucket, and it is deliberately NOT
/// [`agents::is_active`] — the badge-pulse decision, which calls an `idle` agent
/// resting, while an idle agent claude still holds a process for is live here.
///
/// A DISPLAY reading of the POLLED `--all` map, and of the pid's PRESENCE only: its
/// value is never read here, never signalled, and never decides a hand-off. That
/// stays [`crate::send::interrupt_gate`]'s, off the one-shot probe
/// (`App::live_agent_now`). At worst this answer is one poll stale, like the badge.
///
/// Pure, so the rule is tested without drawing a frame.
fn reports_live_process(agent: &ReportedAgent) -> bool {
    agent.pid.is_some()
}

/// A reported status drawn on the pinned row. Cyan + BOLD marks it as the board
/// speaking rather than transcript content (the search prompt uses the same accent).
/// NAMED so it adapts to the terminal theme — no RGB (TERMINAL-SAFE STYLING).
fn banner_status_span(text: String) -> Span<'static> {
    Span::styled(
        text,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )
}

/// The turn `marker` the pinned row names, followed — for a LIVE agent only — by
/// [`HEADER_SEPARATOR`] and the session's status with its age
/// (`● claude · 10:00  ·  live busy · 46m`), so the turn being read and how long the
/// agent has been running share the one row.
///
/// The suffix needs BOTH of two facts about the selected session's reported record:
/// it is LIVE — it carries a `pid` ([`ReportedStatus::live`], asked through
/// [`reports_live_process`]) — AND its age is known ([`ReportedStatus::aged`] —
/// [`agents::elapsed_phrase`] answered against the poll's stamp), so the row never
/// states an age it cannot measure. Anything else leaves the row EXACTLY the marker —
/// no status, no dangling separator: an unreported session; a record with no `pid`,
/// even one whose age is known (the finished and parked records, see
/// [`reports_live_process`]); and a live record with no age to state (no
/// `startedAt`, a map with no stamp, a start after the stamp). The fallback line
/// ([`preview_banner`]) is not this: it states a reported session's status, with
/// its age when known, live or not.
///
/// The marker comes FIRST, so the row stays the turn header it was and the status
/// rides after it. The row is never wrapped, so on a narrow pane the age is cut off
/// first, then the status, and the marker last; for a live agent's session the row is
/// therefore no longer exactly the marker line. A standing failure never reaches
/// here: [`failed_task_banner_line`] outranks the marker and its suffix alike.
fn marker_with_live_status(app: &App, mut marker: Line<'static>) -> Line<'static> {
    let live = app
        .selected
        .as_deref()
        .and_then(|selected| reported_status(app, selected))
        .filter(|status| status.live && status.aged);
    if let Some(status) = live {
        marker.spans.push(Span::raw(HEADER_SEPARATOR));
        marker.spans.push(banner_status_span(status.text));
    }
    marker
}

/// The pinned row's line while the SELECTED session carries a failed background task
/// ([`Session::failed_task`](crate::store::Session::failed_task)), or `None` when it
/// carries none (or nothing is selected).
///
/// It OUTRANKS every other line the row can show: [`render_preview`]'s banner remap
/// asks this FIRST, so while the failure stands the pinned row says it ALONE —
/// [`failed_task_banner`]'s sentence, claude's own summary quoted verbatim — ahead of
/// the turn marker at the top of the viewport (and the status and age a live agent's
/// marker carries) and of the reported-status fallback [`preview_banner`] carries.
/// The sticky turn header therefore gives way on such a session until the user
/// writes into it and the parse clears the flag: an unanswered failure is the one
/// fact on that row that wants the user, while the turn being read is still on screen
/// in the transcript beneath it.
///
/// CONTENT only, never the reservation: whether the row exists at all is still
/// [`preview_banner`]`(..).is_some()`, so this can only change what a reserved row
/// SHOWS — a draft card or an in-flight reply that suppressed the row keeps it
/// suppressed, and the split and the click hit-test are untouched. Read-only over
/// the selected session's own parsed field: no new `App` state, no I/O.
fn failed_task_banner_line(app: &App) -> Option<Line<'static>> {
    let task = app
        .selected
        .as_deref()
        .and_then(|selected| app.session_by_id(selected))
        .and_then(|session| session.failed_task.as_ref())?;
    // The banner's own BOLD weight, in the failure color the row marker wears
    // ([`FAILED_TASK_COLOR`]), so the row and the pane read as one fact.
    Some(Line::from(Span::styled(
        failed_task_banner(task),
        Style::default()
            .fg(FAILED_TASK_COLOR)
            .add_modifier(Modifier::BOLD),
    )))
}

/// The banner sentence for a failed background task:
/// `background task failed at <when>: <claude's summary>`, or without the
/// `at <when>` when the notice carried no readable timestamp, and without the
/// `: <claude's summary>` when there is nothing to quote ([`quotable_summary`]) —
/// never a dangling colon.
///
/// The summary is QUOTED, never paraphrased: it goes in exactly as the parse kept
/// it — the `<summary>` tag's inner text — so the user reads claude's own account
/// (`Agent "…" failed: Agent stalled: no progress for 600s …`). The time is the
/// NOTICE's, not the session's, and is drawn with the row's own [`short_time`]
/// so the two read alike; it is what tells a failure from this morning apart from
/// one left standing for days, since the flag clears only when the user writes.
///
/// TERMINAL-SAFE by construction rather than by rewriting: a summary is rendered
/// through a ratatui `Span`, whose graphemes drop control characters before they
/// reach the buffer, so a stray escape in claude's text can never be emitted raw.
/// Pure, so the wording is tested without a terminal.
fn failed_task_banner(task: &FailedTask) -> String {
    let lead = match task.timestamp {
        Some(when) => format!("{FAILED_TASK_BANNER_LEAD} at {}", short_time(Some(when))),
        None => FAILED_TASK_BANNER_LEAD.to_string(),
    };
    match quotable_summary(&task.summary) {
        Some(summary) => format!("{lead}: {summary}"),
        None => lead,
    }
}

/// The summary [`failed_task_banner`] quotes, or `None` when there is nothing
/// to quote.
///
/// `None` for the EMPTY summary the parse keeps when a `failed` notice carried
/// no readable `<summary>` (absent, never closed, or empty), and for one made
/// only of whitespace and control characters: the banner's `Span` drops the
/// control characters and draws the whitespace blank, so quoting it would leave
/// the same dangling `: ` with nothing after it. Otherwise the summary itself,
/// UNTOUCHED — blankness is decided on the text, but what is quoted is still
/// claude's words verbatim, padding included. Pure.
fn quotable_summary(summary: &str) -> Option<&str> {
    let blank = summary.chars().all(|c| c.is_whitespace() || c.is_control());
    (!blank).then_some(summary)
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
/// [`preview_banner`]`(..).is_some()` — "a session transcript is on the pane" (see
/// that fn for the cases that return `None`) — and never "the selected session is
/// live". Passing liveness here would desync this geometry from
/// [`super::update`]'s hit-test (which asks [`preview_banner`]) — banner drawn,
/// clicks resolved one row off. It is also unaskable here: liveness means a
/// shell-out to claude ([`App::is_live_now`]), which a render must never do.
///
/// The banner is a dedicated LAYOUT row rather than a line prepended into the
/// scrolled `Text` because the preview is bottom-anchored by DEFAULT
/// (`App::preview_follow_bottom`, re-armed on every selection change): a
/// prepended line is pinned off the top of the viewport for any transcript
/// taller than the pane — which is every realistic session — leaving the banner
/// reachable only via `Home`. As its own row it stays put while the transcript
/// scrolls beneath it.
///
/// When `has_banner` is false the transcript IS the whole inner rect and the
/// banner rect is empty, so a BANNER-LESS pane's geometry is exactly what it was
/// before the banner existed.
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
/// [`compose_hint`] for the measurement). This string is 112 columns, so on the
/// one-line help row the clause would be painted past the end of an 80-column
/// terminal, and on the draft card — which wraps rather than clipping — it would
/// cost a further wrapped row of a placeholder whose whole point is to stay
/// near-empty.
///
/// `Ctrl-L model` (this draft's model picker) is the one key that DID earn a place,
/// and it sits right after `Ctrl-O run interactively` — ending at column 67 — so it
/// is inside the 80 columns the help row draws, where the newline clause and
/// `Esc cancel` behind it already were not. On the card its 15 columns (97 before
/// it, 112 with it) can cost one more wrapped row on a narrow pane: the price of
/// naming the key on the one surface a draft shows before anything is typed.
const BG_DRAFT_HINT: &str = "Enter start in background · Ctrl-O run interactively · \
                             Ctrl-L model · Ctrl-J newline (or Alt+Enter) · Esc cancel";

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
/// block's inner rect — one of exactly TWO places a `ratatui_textarea` value is
/// rendered, the other being [`render_search`], which draws the board's one-line
/// query. The two differ in what they let the widget own: this one takes the
/// widget's cursor as it comes, while the search line overrides the cursor STYLE
/// per frame to drive the blink (see [`super::compose`] and [`render_search`]).
///
/// The box's BOTTOM border carries its `model: …` label ([`compose_model_label`]):
/// what this reply or draft will run on. On the border rather than inside the box,
/// so it costs the editor no row — the box is the same height with or without it,
/// down to the one-row minimum on a short terminal — and it rides whichever place
/// the box is drawn (docked in the preview, or the full-width bottom bar).
fn render_compose_zone(frame: &mut Frame, app: &App, area: Rect) {
    let Some(compose) = &app.compose else {
        return;
    };
    let title = compose_title(app, compose);
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title);
    // Resolved from state already in hand (the cached preview and the settings
    // read), never by reading a file or the environment here in render.
    if let Some(default) = app.compose_default() {
        block = block.title_bottom(compose_model_label(compose.model.as_ref(), &default));
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(&compose.textarea, inner);
}

/// The preview block's title whenever the pane is NOT standing in for a hidden
/// list — which is every layout but [`PaneLayout::PreviewOnly`]. Named once so the
/// string drawn and the string the tests look for cannot drift apart.
const PREVIEW_TITLE: &str = " preview ";

/// The preview block's title: [`PREVIEW_TITLE`], or — in the 0:1
/// [`PaneLayout::PreviewOnly`] layout, where no list is drawn to show which row is
/// selected — the SELECTED session's label, so the pane still says whose
/// transcript it is. `↑`/`↓` keep moving the selection there, and the title is
/// what tells the reader where it went.
///
/// Falls back to the session id for an empty label, as [`compose_title`] does, and
/// to [`PREVIEW_TITLE`] when there is no transcript to name: nothing selected, or a
/// new-session DRAFT card replacing the transcript — naming the selected row over
/// a placeholder for a session that does not exist yet would claim the card
/// belongs to an unrelated conversation. Pure, so the wording is assertable
/// without a terminal.
fn preview_title(app: &App) -> String {
    if app.pane_layout() != PaneLayout::PreviewOnly || app.draft.is_some() {
        return PREVIEW_TITLE.to_string();
    }
    match app.selected_session() {
        Some(session) if !session.label.is_empty() => format!(" {} ", session.label),
        Some(session) => format!(" {} ", session.session_id),
        None => PREVIEW_TITLE.to_string(),
    }
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
    let block = Block::default()
        .borders(Borders::ALL)
        .title(preview_title(app));

    // The selected session leads with the marker of whichever turn owns the TOP
    // row of the viewport, so who spoke — under which agent, on which model, when —
    // stays readable long after that turn's own marker scrolled off the top of a
    // long answer. A LIVE agent's status and age ride after that marker on the same
    // row (`marker_with_live_status`) — unless a background task it launched FAILED
    // and the user has not written into it since, in which case the row quotes
    // claude's own account of the failure instead, alone
    // (`failed_task_banner_line`). This call answers only
    // WHETHER that row is reserved, and carries its fallback line for a transcript
    // with no marker at all (`preview_banner`); the row's CONTENT is resolved
    // further down, once the scroll offset is known. It is PINNED as its own layout
    // row (see `preview_split`) — the transcript scrolls beneath it — so the
    // default bottom-anchored viewport cannot scroll it away. A pane with no
    // session transcript on it (nothing selected, a draft card, an in-flight reply)
    // reserves no row.
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
    // The layout step's reading-position anchor is the same kind of one-shot and
    // is taken here for the same reason.
    let pending_anchor = app.take_preview_anchor();

    // How many LOGICAL lines the cached transcript holds. Asked instead of its
    // wrapped height for the emptiness test alone: at a degenerate zero-width pane
    // every line wraps to zero rows, and a real transcript would read as absent.
    // Also the call that WARMS the cache for this (session, width), which
    // `preview_match_target` below reads without being able to fill.
    let transcript_lines = app.preview_line_count(inner_width);

    // Nothing selected (no text AND no banner, since a banner implies a SELECTED
    // session). A selected session whose transcript is still empty falls through
    // instead: its banner is the one thing worth drawing (a failed task's sentence,
    // or a reported agent's status), and keeping the banner unconditional is what
    // lets the hit-test below derive the same geometry from `banner.is_some()` alone.
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
    // one border. For a banner-less pane this paints exactly what
    // `Paragraph::new(text).block(block)` painted: `preview_split`'s inner rect is
    // `Block::inner` for `Borders::ALL`, and the paragraph's own style is default.
    //
    // The block goes down HERE, but the BANNER cannot: its content is the marker of
    // whichever turn the viewport's top row lands on, so it has to wait for the
    // resolved `offset` below. Only the draw moves — the block still precedes it, so
    // the border can never paint over the pinned row.
    frame.render_widget(block, area);

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
    // A layout step's reading position, resolved the way the jump is and at the
    // same site: the line the reader had at the top is read off the prefix map at
    // THIS width, so it goes back to the top however the new width re-wrapped
    // everything above it. Unlike the jump it parks the line AT the top, with no
    // lead — it restores where the reader was rather than presenting a match. It
    // is only ever noted for a pane the reader positioned, so it needs no write to
    // `preview_follow_bottom`: that is already off. A jump wins if both were ever
    // pending, since it answers the more recent question; under a draft CARD the
    // noted line addresses text that is not on screen, so it is dropped there,
    // exactly as the jump is.
    let anchored = match (jump, pending_anchor) {
        (None, Some(line)) if !showing_card => app
            .preview_rows_above(inner_width, line)
            .map(|rows_above| u32::try_from(rows_above).unwrap_or(u32::MAX)),
        _ => None,
    };
    let offset = clamp_preview_offset(
        follow_bottom,
        jump.or(anchored).unwrap_or(app.preview_scroll),
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

    // The banner's CONTENT is resolved HERE — after `offset` — rather than where
    // `banner` was first read above, because it needs that same resolved offset to
    // know which turn is under the pinned row. ONE precedence decides it, in this
    // one expression and nowhere else:
    //
    // 1. a FAILED background task the selected session still carries
    //    ([`failed_task_banner_line`]) — claude's own account of the failure, in
    //    `FAILED_TASK_COLOR`, ALONE on the row. It outranks the turn marker, so the
    //    sticky header gives way on such a session until the user writes into it;
    // 2. otherwise the marker `Line` of the turn now sitting at the TOP of the
    //    viewport ([`App::preview_marker_at`]), in every scroll state including the
    //    default bottom-anchored one (re-armed on every selection change). For a
    //    LIVE agent — its reported record carries a `pid` ([`reports_live_process`])
    //    — whose age is known, the marker is followed by `HEADER_SEPARATOR` and
    //    `<status> · <age>` ([`marker_with_live_status`]); for every other session,
    //    a finished one with a known age included, the row is EXACTLY the marker;
    // 3. otherwise `preview_banner`'s fallback: `friendly_status`, with its age when
    //    one is known, for a transcript with no marker to pin (an empty one, or a
    //    session file that can no longer be read) that claude reports, and a blank
    //    row for any other session.
    //
    // Whether anything is shown AT ALL is still exactly `banner.is_some()` from
    // above — a card or an in-flight send already forced it to `None`, and nothing
    // here widens that, a standing failure included: `preview_split`'s reservation
    // and the click hit-test's `preview_banner(..).is_some()` contract are
    // UNCHANGED by this remap.
    //
    // The marker is handed `offset_rows` — the SAME `usize` row the window above was
    // taken at, not a second narrowing of `offset` — and reads the SAME cached
    // `row_prefix` that `row_window` just windowed by. That shared identity is the
    // whole correctness argument: the pane's first painted row and the row the
    // banner names its turn from are resolved by one `line_at_row` over one map, so
    // the banner cannot name a turn the pane did not paint there. A card is excluded
    // structurally, since `preview_banner` already returned `None` for one.
    let banner = banner.map(|fallback| {
        failed_task_banner_line(app)
            .or_else(|| {
                app.preview_marker_at(inner_width, offset_rows)
                    .map(|marker| marker_with_live_status(app, marker))
            })
            .unwrap_or(fallback)
    });
    if let Some(banner) = banner {
        // Deliberately NOT wrapped: a pinned row cannot grow, so an over-long
        // banner truncates at the pane edge rather than silently stealing a
        // transcript row and desyncing the hit-test's geometry. The edge cuts the
        // TAIL, which is why a live agent's status and age ride AFTER the marker:
        // on a narrow pane they give way first and the marker survives.
        frame.render_widget(Paragraph::new(banner), banner_area);
    }

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
/// A THIRD consumer asks it on a keypress rather than a frame: a layout step notes
/// the line at the top of the pane (`App::set_pane_layout`) so the next render can
/// put that line back at the top at the new width. It reads the map the last frame
/// was drawn from, so the line it notes is the one that frame painted there.
///
/// A `row` past the map's total answers with the index ONE PAST the last line, since
/// the map holds one more entry than it has lines. The draw clamps that (an offset
/// past the end simply paints nothing); the hit-test rejects it (see
/// [`visual_to_content`]).
pub(crate) fn line_at_row(row_prefix: &[usize], row: usize) -> usize {
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

/// The marker [`Line`] of the turn sitting at wrapped visual row `offset` — the TOP
/// of the preview viewport, in EVERY scroll state including the default
/// bottom-anchored one — or `None` when `markers` is empty or `offset` falls past the
/// end of the content.
///
/// Reuses [`visual_to_content`] — the SAME binary search over the SAME
/// [`wrapped_row_prefix`] map the windowed draw starts at ([`row_window`]) and the
/// click hit-test resolves against ([`link_at`]) — to translate `offset` into the
/// logical content row beneath it, then looks up the LAST marker at or before that
/// row: `markers` is produced in FILE order (see [`preview::render`]), so
/// `content_row`s are monotonically non-decreasing and the last one `<=` the target
/// is the turn that OWNS that row — the marker line itself, or any body line beneath
/// it, belongs to the turn whose marker precedes it. When the target row sits ABOVE
/// every marker (e.g. `offset == 0`, on the blank line that leads the very first
/// turn), the FIRST marker is used instead: the opening turn is still what is "under"
/// the pinned row in that case, there being nothing rendered before it.
///
/// The row half of that lookup is EXACT, and that is load-bearing rather than
/// incidental. An earlier form of this took a per-line display-WIDTH model and
/// re-derived the wrap as `ceil(width / inner_width)`, which drifts by a row for
/// every line the WRAPPER chose to break somewhere else — so a long transcript, or
/// one holding a soft-wrapped GFM table row, could pin a NEIGHBOURING turn's marker
/// while compiling and rendering perfectly. Reading the map `row_window` just
/// windowed by removes that class outright: the pane's first painted row and this
/// lookup's target row are one number resolved through one [`line_at_row`], so the
/// banner cannot name a turn the pane did not paint on its top row.
///
/// Pure and terminal-free, so the mapping is unit-testable from a marker list and a
/// prefix map alone. This answers WHICH turn and nothing else; the pinned row the
/// pane actually paints goes through [`marker_at_top_marked`], which adds the active
/// query's marks to THIS line, and [`App::preview_marker_at`] is the impure, cached
/// wrapper [`render_preview`] calls for it.
///
/// [`App::preview_marker_at`]: super::app::App::preview_marker_at
pub(crate) fn marker_at_top(
    markers: &[preview::MarkerLine],
    row_prefix: &[usize],
    offset: usize,
) -> Option<Line<'static>> {
    marker_owning_row(markers, row_prefix, offset).map(|m| m.line.clone())
}

/// The turn marker that OWNS the content row under wrapped visual row `offset`.
///
/// The ONE place the ownership rule lives — the LAST marker at or before the target
/// row, or the FIRST when the row sits above every one ([`marker_at_top`] owns why) —
/// so the line the banner SHOWS and the content row its marks are looked up by are
/// answered by the SAME rule over the SAME map. A second copy of that rule could name
/// one turn's marker and style it with another turn's matches.
///
/// Note that ONE rule is not one CALL: [`marker_at_top_marked`] resolves through here
/// TWICE per pinned row — once for the line (via [`marker_at_top`]) and once for that
/// marker's own `content_row` — so it is two walks over the one map, not one walk
/// serving both. They cannot disagree: this is pure, both calls are handed the SAME
/// three arguments, and `markers`/`row_prefix` are borrowed IMMUTABLY across the whole
/// of it, so the second walk re-derives the first's answer by construction. The cost
/// is one extra reverse scan of a list holding at most one entry per turn, on a path
/// that runs once per frame for a single row.
fn marker_owning_row<'m>(
    markers: &'m [preview::MarkerLine],
    row_prefix: &[usize],
    offset: usize,
) -> Option<&'m preview::MarkerLine> {
    let (content_row, _) = visual_to_content(row_prefix, offset)?;
    markers
        .iter()
        .rev()
        .find(|m| m.content_row <= content_row)
        .or_else(|| markers.first())
}

/// [`marker_at_top`]'s line carrying the active query's marks — the SAME emphasis,
/// through the SAME helper, the transcript's own rows are drawn with.
///
/// The banner reuses a `Line` the renderer already produced, and that is what keeps
/// the pinned row inside TERMINAL-SAFE STYLING: no color and no attribute is invented
/// here, and nothing is re-derived from the query. But the drawn WINDOW re-styles the
/// lines it paints through [`highlight_matched_spans`] first, so reusing the UNMARKED
/// line put one line on screen in two appearances whenever the query hit a marker —
/// marked in the transcript, unmarked in the pinned row directly above it. Reusing
/// the marks the window already produces is the whole fix.
///
/// `matches` is the width-scoped cache's match map ([`App::preview_matches`], the
/// very map the window marks by), keyed ABSOLUTELY by rendered line index — the same
/// coordinate space [`preview::MarkerLine::content_row`] addresses — so the lookup is
/// the marker's OWN row and nothing has to be rebased. An unsearched pane simply
/// misses and the line is reused verbatim, exactly as before.
///
/// WHICH turn stays [`marker_at_top`]'s answer alone: this adds styling to that
/// line and can never move the banner onto another turn. That is why the resolution
/// is done TWICE rather than threaded through as one value — the line comes back from
/// [`marker_at_top`], the row from a second [`marker_owning_row`] call — and why the
/// two are nonetheless the same turn: identical arguments into one pure rule, with
/// both slices held immutably throughout ([`marker_owning_row`] states the argument).
///
/// [`App::preview_matches`]: super::app::App::preview_matches
pub(crate) fn marker_at_top_marked(
    markers: &[preview::MarkerLine],
    row_prefix: &[usize],
    matches: &HashMap<usize, HashSet<usize>>,
    offset: usize,
) -> Option<Line<'static>> {
    let line = marker_at_top(markers, row_prefix, offset)?;
    // The marks are keyed by the resolved marker's OWN content row, so that row is
    // asked of the SAME shared ownership rule rather than re-derived here.
    let Some(matched) = marker_owning_row(markers, row_prefix, offset)
        .and_then(|marker| matches.get(&marker.content_row))
    else {
        return Some(line);
    };
    Some(highlight_matched_spans(
        &line,
        matched,
        PREVIEW_MATCH_MODIFIER,
    ))
}

/// The `(content_row, content_col)` a mouse click at screen `(col, row)` lands on
/// inside the preview transcript, or `None` when the click is outside `inner` or
/// past the end of the content.
///
/// The resolver behind [`fold_at`], and its only caller: the ONE place a clicked
/// CELL becomes a content coordinate for a fold. Its link sibling [`link_at`] shares
/// the half that matters — the ROW comes from [`visual_to_content`] there too, a
/// binary search of the very map the draw windowed by, which is what stops a fold
/// and a link disagreeing about which line a cell belongs to — but it answers the
/// COLUMN by re-rendering the clicked line ([`region_paints_cell`]) instead of by the
/// packing below, because a wrapped url must be clickable on every cell it was
/// painted into. A [`FoldRegion`] spans its header's WHOLE display width, so the
/// packing resolves a fold to the same node either way and buys the cheaper answer.
///
/// The row half is EXACT; the column half is a packed APPROXIMATION, and this doc
/// OWNS both of its directions — [`link_at`]'s no longer can, having stopped
/// approximating. `sub_row * inner.width` packs characters while the wrapper breaks
/// at word boundaries, so a sub-row's true start falls on EITHER side of that
/// product. Break EARLY at a word boundary and the row spent FEWER source characters
/// than the product assumes, so the computed column runs to the RIGHT of the true
/// one; SWALLOW the whitespace broken on — consumed with no cell painted for it — and
/// the row spent MORE, so the column runs to the LEFT
/// (`character_packing_and_word_wrap_disagree_in_both_directions` pins both halves).
/// The error is bounded either way by ONE logical line's own wrapped extent, and it
/// can never address a DIFFERENT line, because the `content_row` was gated exactly
/// above and that gate is DIRECTION-INDEPENDENT — which is why the second direction
/// costs the bound nothing. The FIRST row of every line, and every row of every line
/// that fits `inner.width`, is exact.
///
/// Containment is checked on BOTH axes, and the column half is not a formality:
/// the mouse arm gates on `App::preview_rect`, the OUTER rect, so the pane's
/// BORDER columns reach this function already. Answering them by clamping the
/// column instead would alias every click on the left border onto content column
/// 0 — a hit on any region that starts there, which is every fold-node header,
/// peer and injected alike.
fn content_hit(
    col: u16,
    row: u16,
    inner: Rect,
    scroll_offset: u32,
    row_prefix: &[usize],
) -> Option<(usize, usize)> {
    if !inner.contains(Position { x: col, y: row }) {
        return None;
    }
    // Both subtractions are proved non-negative by the containment above.
    let rel_col = usize::from(col - inner.x);
    let rel_row = usize::from(row - inner.y);
    // `scroll_offset` is `App::preview_scroll`, so it carries the pane's `u32`
    // offset domain into this `usize` row lookup. Saturating both steps: the worst a
    // saturated one can produce is a row past the end of the map, which
    // `visual_to_content` already answers with the "nothing here" `None`.
    let visual_row = usize::try_from(scroll_offset)
        .unwrap_or(usize::MAX)
        .saturating_add(rel_row);
    let (content_row, sub_row) = visual_to_content(row_prefix, visual_row)?;
    // The one packed step left. A word-wrapped sub-row starts on EITHER side of
    // `sub_row * inner.width`, so this can overshoot OR undershoot the true column —
    // never reaching another line either way, since the content row came from the map
    // above rather than from this arithmetic.
    let content_col = sub_row * usize::from(inner.width) + rel_col;
    Some((content_row, content_col))
}

/// The modifier a link probe re-styles a candidate region with before rendering it,
/// so the cells that region PAINTED can be told from every other cell on the line.
///
/// `CROSSED_OUT` because nothing in this crate styles with it — the preview's
/// vocabulary is `DIM`/`BOLD`/`UNDERLINED`/`REVERSED`/`ITALIC` — so a probe cell
/// carrying it can only have come from the region that was marked. It is composed
/// ONTO each span's existing style, never replacing it, so the marked line differs
/// from the plain one in this one bit and nothing else. The probe buffer is
/// in-memory and dropped, so this attribute never reaches a terminal.
const LINK_PROBE_MARKER: Modifier = Modifier::CROSSED_OUT;

/// The symbol a probe buffer is pre-filled with, so a cell the renderer never wrote
/// is distinguishable from one it wrote a blank into.
///
/// NUL is the choice because `Paragraph` SKIPS every zero-width grapheme, so it can
/// never paint one — a NUL left in a probe buffer is therefore a cell nothing was
/// drawn to. That is what identifies the RIGHT half of a double-width glyph, which
/// ratatui paints by styling the LEFT cell alone: without it a click on the second
/// column of a CJK or emoji label would read an untouched cell and miss a link that
/// is plainly under the pointer.
const LINK_PROBE_UNWRITTEN: &str = "\u{0}";

/// The display width of a DOUBLE-WIDTH glyph, and the ONLY left-neighbour width under
/// which an unwritten probe cell belongs to the glyph beside it.
///
/// An unwritten cell has two possible causes and they demand opposite answers: the
/// right half of a wide glyph (part of what was painted) and a column the renderer
/// never reached at all (painted by nothing). Only a left neighbour measuring this
/// wide can account for the first, so it is what separates them.
const WIDE_GLYPH_COLUMNS: usize = 2;

/// The most TEXT one click may spend probing which link it landed on: the BYTES of
/// the logical line it resolved to, multiplied by the candidate regions on that line.
///
/// [`link_at`] buys exactness by RE-RENDERING ([`region_paints_cell`]), and that
/// render is not free. A candidate walks the line's graphemes to find the region's
/// chars ([`region_char_positions`]), re-styles and COPIES the line end to end
/// ([`highlight_matched_spans`]), then word-wraps that copy end to end again to check
/// the row count — all of it over the WHOLE logical line, however small the clicked
/// cell is. A MISS pays that for each region on the line, since nothing short-circuits
/// an answer that never comes. So one click costs `candidates * line_bytes` of
/// reading, and THAT PRODUCT is what this bounds.
///
/// The product, not either factor, because each cap alone admits the case the other
/// exists to stop. A cap on LENGTH alone still lets a line sitting just under it carry
/// thousands of links — a `[a](u)` needs only a few bytes — and a cap on the candidate
/// COUNT alone still lets ONE link sit inside a megabyte-long minified blob.
///
/// BYTES, and not the wrapped ROWS [`link_at`] already holds free off the row-prefix
/// map, which is the tempting measure and the wrong one. Rows count what the wrapper
/// PAINTS; the probe pays for what it READS, and a grapheme of zero display width is
/// read without ever being painted. A line of tens of MB of ZWSP or combining marks
/// wraps to ONE row, so a row budget would price that click at 1 and then permit
/// thousands of candidates against it — each still copying and re-wrapping every one
/// of those megabytes (`a_zero_width_line_costs_bytes_the_wrapped_rows_cannot_see`
/// pins the gap). Bytes SUBSUME rows, since a non-empty line never wraps to more rows
/// than it has bytes.
///
/// What bytes do NOT subsume on their own is the SPAN count, and a candidate is paid
/// for in BOTH: [`highlight_matched_spans`] emits at least one span per input span and
/// deliberately KEEPS the empty ones, which carry no bytes to charge for. The byte
/// price survives that. A NON-EMPTY span costs at least one byte, so those number at
/// most `bytes`; the EMPTY ones are bounded per line, because a block construct emits
/// O(1) of them per line and so cannot grow them with the line's length. What would
/// actually defeat a byte price is an UNBOUNDED number of empty spans — a line of
/// thousands measures ZERO bytes and buys unlimited candidates against it — and
/// keeping that from arising through an inline form is
/// `store::preview::parse_inline_collect`'s obligation, discharged there, not here.
///
/// A line's spans are therefore `O(bytes) + O(1)`, one candidate costs `O(bytes)`, and
/// `candidates * (bytes + spans)` stays `O(candidates * bytes)` — the product measured
/// here. Stated as a BOUND rather than as a list of which spans happen to be non-empty,
/// deliberately: a bound still holds when someone adds a parser arm, whereas an
/// enumeration silently rots the moment one is added and leaves the budget resting on a
/// claim that stopped being true without anyone editing this comment. So the obligation
/// a new arm inherits is not "emit no empty span" — it may add another O(1)-per-line
/// empty freely. It is: do not emit empty spans in a count that grows with the input.
/// Those span shapes are also not this hit-test's to change — it prices them; it does
/// not own them. `markdown_body_lines_collect_bounds_a_lines_spans_against_its_bytes`
/// pins the bound across them.
///
/// 131,072 (128 KiB) is chosen for HEADROOM, not tightness: a line of ordinary prose
/// or source runs ORDERS OF MAGNITUDE under it. Measured multiples are deliberately
/// not stated here — they drift with the corpus, and the longest line to hand sits in
/// a file this very commit edits.
///
/// PAST the budget the hit-test ABSTAINS: it answers [`LinkProbe::Unresolvable`]
/// rather than a guessed url, the same direction [`region_paints_cell`] takes when it
/// cannot reproduce a row count, and for the same reason — a wrong hit gives a browser
/// an unintended url, while a missed one costs a click. That direction is also what
/// makes the bound safe by construction: it takes effect as an early abstention BEFORE
/// any region is consulted, so it can turn a hit into an abstention but not a non-hit
/// into a hit.
const LINK_PROBE_BYTE_BUDGET: usize = 131_072;

/// The text a probe of `line` must read end to end, in BYTES — the unit
/// [`LINK_PROBE_BYTE_BUDGET`] is spent in.
///
/// Summed over the SPANS rather than over a joined string, because a `Line` never
/// holds one: each `Cow<str>::len` is a field read, so measuring a line walks its
/// spans (a handful) and never its text. Bytes rather than chars or graphemes is slack
/// in the safe direction — it can only over-charge a probe, never under-charge one.
/// Pure and terminal-free.
fn line_probe_bytes(line: &Line<'_>) -> usize {
    line.spans.iter().map(|s| s.content.len()).sum()
}

/// Can a click that resolved to a line of `line_bytes` carrying `candidates` regions
/// be answered within [`LINK_PROBE_BYTE_BUDGET`]?
///
/// Saturating, so an absurd pair cannot wrap its product back under the budget and buy
/// itself exactly the work the budget exists to refuse.
/// Pure and terminal-free.
fn probe_within_budget(line_bytes: usize, candidates: usize) -> bool {
    line_bytes.saturating_mul(candidates) <= LINK_PROBE_BYTE_BUDGET
}

/// What a click inside the preview transcript resolved to — the whole answer
/// [`link_at`] can give, in the three shapes it can honestly take.
///
/// It exists because `Option<&str>` could not say the third one. A `None` meant BOTH
/// "the click landed on text carrying no link" and "the click landed on a line too
/// large to hit-test, so which link — if any — is unknown", and those are different
/// events that owe the reader different answers. Collapsed together, the second one
/// came out as SILENCE: a label that IS rendered and IS underlined, clicked, and
/// nothing said. That is the same indistinguishable silence the hit-test exists to
/// remove, so the distinction has to survive as far as the status line.
///
/// [`Unresolvable`](Self::Unresolvable) is never vacuous, which is what makes it worth
/// telling the reader about: the budget bounds `line_bytes * candidates`, and a line
/// with NO candidates has a product of zero, so it is always within budget. Abstaining
/// therefore IMPLIES at least one link region on that logical line. The click may not
/// have been aimed at it — but there is a rendered link there, and "no link here" would
/// be a claim this probe did not make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkProbe<'a> {
    /// The click landed on a cell this url's region PAINTED.
    Hit(&'a str),
    /// The click resolved to a real cell carrying no link region.
    NoLink,
    /// The click's line exceeded [`LINK_PROBE_BYTE_BUDGET`], so the probe was never
    /// spent and WHICH region was clicked is unknown — see the type doc for why that
    /// is not the same as [`NoLink`](Self::NoLink).
    Unresolvable,
}

/// The CHAR positions within `line`'s plain text whose grapheme clusters overlap the
/// DISPLAY-column range `col_start..col_end` — a [`LinkRegion`]'s columns translated
/// into the units [`highlight_matched_spans`] marks in.
///
/// Two accumulators, deliberately different, because they answer to two different
/// authorities. The SPAN origin advances by each span's whole-string
/// `unicode-width`, which is exactly how `store::preview` produced these columns in
/// the first place (it sums one display width per rendered span). The offset WITHIN
/// a span advances per grapheme cluster, because that is the finest unit a region
/// boundary can honestly land on. Summing per-cluster widths is not the same
/// function as measuring the unsplit span — `unicode-width` is a contextual fold —
/// so using the per-cluster sum for the outer walk would drift from the coordinates
/// the region was recorded in.
///
/// Whole clusters only: every char of an overlapping cluster is included, so a
/// region boundary landing INSIDE a cluster widens outward instead of cutting it.
/// That matters because a severed emoji measures differently from the same bytes
/// unsplit, which would move where the wrapper breaks and make the probe describe a
/// layout the pane never painted. It is the SECOND guard on that, not the only one —
/// [`match_runs`] snaps a run out to cluster edges downstream regardless
/// (`match_runs_snaps_a_partial_mark_out_to_the_whole_cluster` pins it) — but
/// emitting whole clusters here keeps this fn's output meaningful on its own rather
/// than only once something else has repaired it.
/// Pure and terminal-free.
fn region_char_positions(line: &Line<'_>, col_start: usize, col_end: usize) -> HashSet<usize> {
    let mut chars: HashSet<usize> = HashSet::new();
    let mut char_pos = 0usize;
    let mut span_col = 0usize;
    for span in &line.spans {
        let mut col = span_col;
        for cluster in span.content.graphemes(true) {
            let cluster_chars = cluster.chars().count();
            let cluster_cols = UnicodeWidthStr::width(cluster);
            if col < col_end && col + cluster_cols > col_start {
                chars.extend(char_pos..char_pos + cluster_chars);
            }
            char_pos += cluster_chars;
            col += cluster_cols;
        }
        span_col += UnicodeWidthStr::width(span.content.as_ref());
    }
    chars
}

/// Did the [`LinkRegion`] at display columns `col_start..col_end` of `line` paint the
/// cell at `(rel_col, sub_row)` when `line` was word-wrapped at `inner_width`?
///
/// This is the answer a character-packing formula could only approximate, and it is
/// obtained by ASKING THE RENDERER rather than by modelling it. The region's
/// graphemes are re-styled with [`LINK_PROBE_MARKER`], that one line is pushed
/// through the SAME `Paragraph::wrap(Wrap { trim: false })` the pane paints with,
/// into an in-memory [`Buffer`] `inner_width` wide, and the clicked cell is read
/// back. A cell carrying the marker is a cell the region painted — wherever the
/// wrapper chose to break, and whether or not the region straddles that break. No
/// second wrapping model exists to drift from the first.
///
/// Re-styling is what makes the probe sound: [`highlight_matched_spans`] leaves the
/// text BYTE-IDENTICAL and splits only at grapheme-cluster edges, so no glyph's
/// width can move and the probe's wrap is the paint's wrap. `line_rows` — the height
/// the map the pane WINDOWED BY gave this line — is the check on that claim: when
/// the marked line does not measure to it, the probe describes some other layout, so
/// this refuses instead of answering from it. That direction is chosen: a hit-test
/// that guesses hands an unintended url to a browser, while one that abstains costs
/// a click.
///
/// Only rows up to the clicked one are allocated. A wrap is a function of WIDTH
/// alone, so a shorter buffer CLIPS the paint without moving a single break — the
/// cost is the rows a click can actually reach, not the line's full extent.
///
/// The clicked cell is read back under TWO rules, and the second one is narrow on
/// purpose. A cell carrying the marker is a hit outright. A cell the renderer left
/// UNWRITTEN is a hit only when its LEFT neighbour is both marked AND
/// [`WIDE_GLYPH_COLUMNS`] wide — because an unwritten cell is AMBIGUOUS, and the two
/// things it can mean want opposite answers. `render_line` advances by each
/// grapheme's display width and writes once per grapheme, so the second column of a
/// double-width glyph is left untouched and IS part of the link. But `Paragraph` only
/// `set_style`s its area — it never blanks a row's remainder — so every column right
/// of a row's last painted glyph is untouched too, and is part of NOTHING. Without
/// the width test those two are indistinguishable, and the blank column beside a link
/// that ENDS a painted row resolves to that link's url: a click on empty space handing
/// an unintended url to a browser. Measuring the neighbour admits the first case and
/// refuses the second, so the probe abstains exactly where it cannot be sure.
///
/// Pure and terminal-free: the buffer never touches a backend.
fn region_paints_cell(
    line: &Line<'_>,
    col_start: usize,
    col_end: usize,
    inner_width: u16,
    line_rows: usize,
    sub_row: usize,
    rel_col: u16,
) -> bool {
    if inner_width == 0 || sub_row >= line_rows {
        return false;
    }
    let marked = highlight_matched_spans(
        line,
        &region_char_positions(line, col_start, col_end),
        LINK_PROBE_MARKER,
    );
    if wrapped_text_rows(std::slice::from_ref(&marked), inner_width) != line_rows {
        return false;
    }
    let Ok(rows) = u16::try_from(sub_row + 1) else {
        return false;
    };
    let area = Rect {
        x: 0,
        y: 0,
        width: inner_width,
        height: rows,
    };
    let mut buf = Buffer::filled(area, Cell::new(LINK_PROBE_UNWRITTEN));
    Paragraph::new(Text::from(vec![marked]))
        .wrap(Wrap { trim: false })
        .render(area, &mut buf);
    // The clicked row is the last one allocated; `Buffer::cell` answers `None` for a
    // position outside the area, which is the "nothing was rendered here" case.
    let y = rows - 1;
    let marked_at = |x: u16| {
        buf.cell((x, y))
            .is_some_and(|c| c.modifier.contains(LINK_PROBE_MARKER))
    };
    if marked_at(rel_col) {
        return true;
    }
    rel_col > 0
        && buf
            .cell((rel_col, y))
            .is_some_and(|c| c.symbol() == LINK_PROBE_UNWRITTEN)
        && buf.cell((rel_col - 1, y)).is_some_and(|c| {
            c.modifier.contains(LINK_PROBE_MARKER)
                && UnicodeWidthStr::width(c.symbol()) == WIDE_GLYPH_COLUMNS
        })
}

/// What a mouse click at screen `(col, row)` resolved to inside the preview transcript
/// — a url, no link, or an abstention (see [`LinkProbe`], which owns why the last two
/// are not the same answer).
///
/// `inner` is the preview pane's INNER rect (inside the borders), `scroll_offset`
/// the resolved vertical offset in wrapped rows (`App::preview_scroll`), and
/// `row_prefix`, `lines` and `regions` the whole transcript's per-line wrapped-row
/// map, its rendered lines and its clickable [`LinkRegion`]s — all three from the
/// SAME width-scoped cache the pane drew from (`App::preview_hit_context`), so the
/// hit-test and the paint can never describe different renders.
///
/// The click resolves in two steps, and NEITHER is an approximation. Which LINE:
/// screen row -> absolute wrapped row (via `scroll_offset`) -> `(content_row,
/// sub_row)`, a binary search of the map the wrapper itself measured
/// ([`visual_to_content`]) — the same map the draw windows by, so no error is
/// accumulated over the lines above the click. Which REGION: each candidate region
/// on that line is asked whether it PAINTED the clicked cell, by re-rendering that
/// one line with the region marked and reading the cell back
/// ([`region_paints_cell`]). A link that SOFT-WRAPS is therefore hit on every one of
/// its drawn cells, continuation rows included, because the answer comes from where
/// the wrapper actually put them.
///
/// The pane paints the search-MARKED lines while this probes the plain cached ones.
/// That is the same invariant the row map already rests on:
/// [`highlight_matched_spans`] only moves styles, leaving the text byte-identical
/// and splitting at cluster edges alone, so the two wrap identically.
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
/// is scrolled, and so is WHICH CELL of that line. The line comes from the map
/// ([`visual_to_content`]); the cell comes from a render of that one line, so the
/// column step no longer has to predict where a row broke. It used to: `sub_row *
/// inner.width` packed characters while the wrapper breaks at word boundaries, which
/// put a sub-row's computed start on either side of its true one and left a wrapped
/// link's continuation row entirely dead — measured at a 92-character url whose last
/// 73 columns could not be clicked (`link_at_hits_a_wrapped_url_across_its_whole_drawn_extent`).
///
/// The cost is one small in-memory render per CANDIDATE region — the regions on the
/// resolved line, typically one — per click, over only the rows up to the clicked
/// one. A click is a human-paced event, so that buys exactness at a price no frame
/// pays. It is nonetheless BOUNDED rather than trusted to stay small: a click whose
/// line and candidate count would read more than [`LINK_PROBE_BYTE_BUDGET`] bytes of
/// text is answered [`LinkProbe::Unresolvable`] instead — see that const for why the
/// bound is their product, why it is spent in bytes rather than in the wrapped rows
/// already in hand, and why no real transcript reaches it, and [`LinkProbe`] for why
/// that abstention is REPORTED rather than folded into "no link".
/// Pure and terminal-free.
pub(crate) fn link_at<'a>(
    col: u16,
    row: u16,
    inner: Rect,
    scroll_offset: u32,
    row_prefix: &[usize],
    lines: &[Line<'_>],
    regions: &'a [LinkRegion],
) -> LinkProbe<'a> {
    if !inner.contains(Position { x: col, y: row }) {
        return LinkProbe::NoLink;
    }
    let rel_col = col - inner.x;
    let rel_row = usize::from(row - inner.y);
    // `scroll_offset` is `App::preview_scroll`, so it carries the pane's `u32`
    // offset domain into this `usize` row lookup. Saturating both steps: the worst a
    // saturated one can produce is a row past the end of the map, which
    // `visual_to_content` already answers with the "no link here" case.
    let visual_row = usize::try_from(scroll_offset)
        .unwrap_or(usize::MAX)
        .saturating_add(rel_row);
    let Some((content_row, sub_row)) = visual_to_content(row_prefix, visual_row) else {
        return LinkProbe::NoLink;
    };
    let Some(line) = lines.get(content_row) else {
        return LinkProbe::NoLink;
    };
    // The rows the PAINT gave this line, read straight off the map it windowed by —
    // the probe below has to reproduce exactly this or it is describing some other
    // layout. A map and a line list that disagree resolve to no link, never a guess.
    let Some(line_rows) = row_prefix
        .get(content_row + 1)
        .and_then(|next| next.checked_sub(row_prefix[content_row]))
    else {
        return LinkProbe::NoLink;
    };
    // The probe below is bounded BEFORE any of it is spent. Counting the candidates
    // costs one pass over `regions`, which the search itself already costs, so the
    // bound is paid for out of work that was happening anyway.
    let candidates = regions
        .iter()
        .filter(|r| r.content_row == content_row)
        .count();
    if !probe_within_budget(line_probe_bytes(line), candidates) {
        return LinkProbe::Unresolvable;
    }
    regions
        .iter()
        .find(|r| {
            r.content_row == content_row
                && region_paints_cell(
                    line,
                    r.col_start,
                    r.col_end,
                    inner.width,
                    line_rows,
                    sub_row,
                    rel_col,
                )
        })
        .map_or(LinkProbe::NoLink, |r| LinkProbe::Hit(r.url.as_str()))
}

/// The fold key of a fold node — a peer message or injected context — whose
/// HEADER sits under a mouse click at screen `(col, row)`, or `None`.
///
/// A sibling of [`link_at`] over the same width-scoped cache entry
/// (`App::preview_hit_context`) and the same transcript rect from [`preview_split`],
/// and the two agree on the half that matters: WHICH LINE a cell belongs to, resolved
/// through [`visual_to_content`] on both paths, so a fold and a link can never
/// disagree about it. They part on the COLUMN. This one goes through
/// [`content_hit`]'s packed arithmetic; its sibling re-renders the line. That is not
/// a second geometry model smuggled in — it is the cheaper of two answers, taken
/// where it is sufficient.
///
/// A [`FoldRegion`] spans its header's WHOLE display width, so a click anywhere along
/// the line toggles the node, and a header that SOFT-WRAPS at a narrow pane is hit on
/// any of its wrapped segments. That width is exactly why the packing is enough here:
/// the bounded column error [`content_hit`] documents cannot walk a click off a region
/// that claims every column of its row, whereas a link label occupying part of a row
/// needed the exact answer. [`content_hit`] owns both directions of that bound; do not
/// restate them.
///
/// Fail-soft like its sibling: a click outside `inner`, on a blank row, or past
/// the end of the content answers `None` and never indexes past the map. Pure and
/// terminal-free.
pub(crate) fn fold_at<'a>(
    col: u16,
    row: u16,
    inner: Rect,
    scroll_offset: u32,
    row_prefix: &[usize],
    regions: &'a [FoldRegion],
) -> Option<&'a str> {
    let (content_row, content_col) = content_hit(col, row, inner, scroll_offset, row_prefix)?;
    regions
        .iter()
        .find(|r| {
            r.content_row == content_row && r.col_start <= content_col && content_col < r.col_end
        })
        .map(|r| r.key.as_str())
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

/// The search line's label. Declared once, so the string DRAWN and the columns
/// RESERVED for it below cannot drift apart.
const SEARCH_LABEL: &str = "search: ";

/// Columns [`SEARCH_LABEL`] occupies. Derived from the label rather than typed,
/// so re-wording it re-sizes the column with it (NO MAGIC VALUES). The label is
/// ASCII, so its byte length IS its column count.
const SEARCH_LABEL_WIDTH: u16 = SEARCH_LABEL.len() as u16;

/// The search input line: a fixed label, then the query editor itself.
///
/// The query is drawn by its own [`TextArea`](ratatui_textarea::TextArea) rather
/// than assembled into a `Line` here, and that is the whole point of the row's
/// split: a `Paragraph` with no scroll CLIPS a query wider than the terminal, and
/// the caret went first because it drew last. The widget owns its own horizontal
/// scroll under `WrapMode::None`, so the tail of a long query — and the caret —
/// stay on screen. It also owns the caret, which is why nothing is appended after
/// the text any more.
///
/// The pulse therefore changes a STYLE rather than a glyph: `REVERSED` in the
/// visible phase, the plain default in the hidden one. It reads the SAME
/// [`blink_visible`] phase of [`App::tick`] as the live badge's dot, so the board
/// still has exactly ONE blink mechanism and the two pulse together rather than
/// drifting. `REVERSED` is a `Modifier`, not a color (TERMINAL-SAFE STYLING), and
/// it is the one attribute this board already relies on being honoured — the
/// list's selection highlight is drawn with it. Do NOT reach for the ANSI blink
/// attribute: this cursor carried it once and therefore never blinked, which is
/// what [`blink_visible`] exists to explain.
///
/// Takes `&mut App` because the cursor style is set on the widget per frame;
/// [`render`] already holds one.
fn render_search(frame: &mut Frame, app: &mut App, area: Rect) {
    let [label_area, input_area] =
        Layout::horizontal([Constraint::Length(SEARCH_LABEL_WIDTH), Constraint::Min(0)])
            .areas(area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            SEARCH_LABEL,
            Style::default().fg(Color::Cyan),
        ))),
        label_area,
    );
    let cursor_style = if blink_visible(app.tick) {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    };
    app.query_input.set_cursor_style(cursor_style);
    // KNOWN CEILING at 65_535 query characters, deliberately left UNFIXED — the
    // horizontal scroll this row leans on is computed in `u16` inside the pinned
    // `ratatui-textarea-0.9.2`. `TextArea::scroll_top_col` casts the caret column
    // DOWN into that width with `self.screen_cursor().col as u16`
    // (`src/widget.rs:112-113`), and `next_scroll_top` then computes
    // `cursor + 1 - len` in the same `u16` (`src/widget.rs:85-89`).
    //
    // At EXACTLY 65_535 characters the caret sits at column 65_535 and `cursor + 1`
    // overflows: a panic inside the render loop in any debug/dev build, and a wrap
    // to a garbage scroll column in release. At 65_536 or more the cast itself
    // wraps the column small, `top_col` collapses to 0, and this row draws the
    // query's HEAD with the caret off screen — the precise "keep the tail and the
    // caret on screen" guarantee the row's split above exists to provide.
    //
    // REACHABLE, not theoretical: a paste APPENDS to the query against
    // `update`'s 4096-character cap, so roughly 16 maximal pastes cross the
    // ceiling. That is the SAME reachability argument that justified fixing the
    // matching ceiling on the DELETE side — see the `CursorMove::Jump` clamp
    // documented on `App::pop_query_word`, which was fixed while this one
    // knowingly was not.
    //
    // Accepted anyway: the ceiling is far beyond any real search query, and a
    // release build degrades this row's rendering rather than crashing. Fixing it
    // means capping the query length in the query funnel — a behaviour change to
    // every input path, not a render-site tweak.
    frame.render_widget(&app.query_input, input_area);
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
/// 77 columns. Anything added here costs the tail of an 80-column terminal, so a
/// new verb is paid for by shrinking existing wording, never by appending. The
/// `y` verb paid twice. Appended to the old `h show/hide hidden` wording, `y copy`
/// made 89 columns, so `h` shrank to `h hidden` (the toggle is still the one `h`
/// does). Relabelled `y copy session ID`, because a bare "copy" did not say what
/// it copies, it made 90 columns with `· Esc cancel`, so `Esc cancel` went — the
/// trade this note had set aside for the next verb: it was the one entry naming
/// no action, and any key the chord does not bind still cancels it, `Esc`
/// included. `d delete row/lineage` keeps naming both of its targets.
fn chord_hint(selected_hidden: bool) -> String {
    let x = if selected_hidden { "expose" } else { "hide" };
    format!("^X  x {x} · d delete row/lineage · h hidden · r reload · y copy session ID")
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
/// the help row is ONE line that never wraps, and the reply hint must fit an
/// 80-column terminal whole, while [`BG_DRAFT_HINT`] is already past 80 there and
/// the clause would be painted nowhere. What a paste does is documented in full
/// where there is room for it: `KEYS` in `cli.rs` and the README key map.
///
/// The model key (`Ctrl-L`, both targets) was PAID FOR on the reply hint, which sat
/// at 78 columns: its `^L model` segment costs 11, so the newline clause went from
/// `Ctrl-J newline (or Alt+Enter)` to `^J/Alt+Enter newline` — both keys still
/// named, in the board keymap's caret notation — which lands the hint at EXACTLY
/// 80 (pinned by `the_reply_hint_fits_an_eighty_column_terminal`). The background
/// hint keeps its spelled-out keys: it was already cut at 80, and `Ctrl-L model`
/// sits right after `Ctrl-O run interactively`, inside the columns that ARE drawn.
fn compose_hint(target: &ComposeTarget) -> &'static str {
    match target {
        ComposeTarget::Reply { .. } => {
            "Enter send · ^L model · ^J/Alt+Enter newline · paste keeps newlines · Esc cancel"
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
        // The board keymap — one of the five surfaces AGENTS.md's KEEP KEY DOCS IN
        // SYNC names. It does NOT mention the terminal's paste, on COLUMN BUDGET:
        // this line is already 225 columns (measured with the `unicode-width` the
        // renderer counts in) against a help row that is ONE line and never wraps, so
        // on an 80-column terminal it is cut the instant `^K stop` ends and
        // everything from `^X hide/del` (column 84) rightward is already unpainted.
        // A 23-column "paste keeps newlines" clause would land at columns 226-248 —
        // nowhere, on any realistic width. What a board paste DOES (append to the
        // query with newlines flattened to spaces, and never resume) is documented
        // where there is room to say it: `KEYS` in `cli.rs` and the README key map.
        //
        // The QUERY WORD-DELETE keys are omitted for exactly the same reason, and
        // just as deliberately. Even the tersest honest clause (`· ⌥⌫ del word`,
        // 14 columns) would be painted at columns 225-238 — off the end of any
        // realistic width — and terse is the one thing this binding cannot be:
        // `Alt-Backspace`, `Ctrl-W` and `Alt-H` ALL do it, on purpose, so that the
        // board answers the same set the compose box does whatever the terminal
        // sends for Option, and naming one of the three would advertise a key set
        // narrower than the one that works. All three are documented on every
        // OTHER surface KEEP KEY DOCS IN SYNC names — the ones with room for that
        // sentence: the keybinding table in `update.rs`'s module doc, `KEYS` in
        // `cli.rs`, the README key map, and the `Alt`-binding rule in
        // PATTERNS.md's "Keys, actions, outcomes". Plain `Backspace` (one
        // character) is unmentioned here on the same budget.
        //
        // `S-↑↓ match` sits with the search cluster rather than with the scroll
        // keys, because it is search navigation that happens to move a pane — and
        // it is spelled `S-` rather than `⇧` so it needs no glyph the terminal may
        // not have. It is off-screen at 80 columns like everything past `^X`, and
        // that is the same budget every clause here is judged against; the key is
        // documented in full in `KEYS` and the README.
        //
        // `S-←→ layout` is spelled the same way for the same reason, and names the
        // pair rather than the five stops they walk — those take a sentence, and
        // `KEYS` and the README have room for it.
        //
        // `^T/^E` sits beside `Home/End` in the scroll cluster (its twin action, not
        // a separate one) rather than beside `^U/^D`: the scroll cluster already
        // begins past column 171, so wherever in it a new token lands is equally
        // off-screen at 80 columns — this placement is purely for readability.
        //
        // `^K stop` covers all of `Ctrl-K`'s routes with one word, deliberately:
        // the job-id route runs `claude stop` and the pid route sends a SIGTERM,
        // which is a REQUEST to stop, so "stop" is true of both without promising
        // that a process ended. It also cannot grow. It ends at EXACTLY column 80,
        // so any extra glyph (`^K stop/signal`, say) would cut its own tail on an
        // 80-column terminal. The routes are spelled out where there is room:
        // `KEYS` in `cli.rs`, the README key map and the table in `update.rs`.
        //
        // `^R reply` stays one word too. Its refusals (a live agent, a session with
        // no job to stop first, and a session whose own reply is still being sent)
        // are refusals, not routes, and each explains itself on this line when it
        // fires. The key sits at columns 63-70 of a line cut at 80, so it has
        // no room to list them anyway. They are spelled out on the same three
        // surfaces as `^K`'s routes.
        //
        // The compose box's `^L` (pick THIS reply's or draft's model) is absent for
        // the reason `^J` is: it is a key of the COMPOSE box, not of the board, and
        // this line lists the board's keys alone. The compose hint that replaces this
        // line while a box is open names it ([`compose_hint`]), and the model picker
        // it opens names its own `←/→` effort keys in its prompt and footer — so the
        // `←/→` below means fold/expand ON THE BOARD only.
        Line::from(vec![Span::styled(
            "↑↓ move · ←/→ fold/expand · Enter resume · ^F fork · ^N new · ^R reply · ^K stop · ^X hide/del · type to search · Tab name/content · S-↑↓ match · ^A scope · S-←→ layout · PgUp/PgDn·^U/^D·^T/^E·Home/End·wheel scroll · Esc quit",
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

/// Columns a modal's `Borders::ALL` block takes from its content: one on the
/// left, one on the right. The width counterpart of [`MODAL_BORDER_ROWS`].
const MODAL_BORDER_COLS: u16 = 2;

/// Columns a modal's content has inside its borders. The message and a wrapped
/// `List` description ([`modal_list_row_lines`]) both wrap to this one width, so
/// neither can put the border somewhere other than where the box draws it.
const MODAL_INNER_WIDTH: u16 = MODAL_WIDTH - MODAL_BORDER_COLS;

/// What a SELECTED `List`-modal row starts with: the session list's own highlight
/// glyph, so a picker's selection looks like the board's.
const MODAL_LIST_SELECTED_MARKER: &str = "› ";

/// What an UNSELECTED `List`-modal row starts with: blanks, exactly as wide as
/// [`MODAL_LIST_SELECTED_MARKER`]. The label, and a wrapped description's hanging
/// indent ([`modal_list_description_column`]), then sit in the same column
/// whichever row is highlighted. If the widths differed, moving the selection
/// would re-wrap a description and change its row's height mid-keypress.
const MODAL_LIST_UNSELECTED_MARKER: &str = "  ";

/// The gap between a `List`-modal row's label and its description.
const MODAL_LIST_DESCRIPTION_GAP: &str = "  ";

/// Non-message rows a `Row`-layout modal draws, borders excluded: a blank spacer,
/// the button strip, a blank spacer, and the footer help line. The message (one or
/// more wrapped rows) is added on top, so the box grows to fit a long prompt rather
/// than clipping it.
const MODAL_ROW_CHROME_ROWS: u16 = 4;

/// Non-message, non-entry rows a `List`-layout modal draws around its selectable
/// list: a spacer row above the list, a spacer row below it, and a footer help
/// line. The box height is message rows + the lines its choices draw + this
/// chrome + two borders, so a picker grows with its choice count (the picker's old
/// `AGENT_PICK_CHROME_ROWS` reasoning, kept) and any modal grows with a wrapped
/// message.
///
/// The two spacers are where the scrolled-list affordance is PAID FOR: when rows
/// sit off the window they carry [`modal_more_line`]'s dim `N more` marker instead
/// of being blank, so the overflow hint costs the box zero extra rows (see
/// [`modal_list_window`]).
const MODAL_LIST_CHROME_ROWS: u16 = 3;

/// The most choice rows a `List`-layout modal ASKS for before it scrolls instead
/// of growing.
///
/// Without a cap the box grows one row per choice without bound — the agent picker
/// draws one row per user-defined agent, and the model picker one per alias — so on
/// a tall terminal an overlay stops reading as an overlay and covers the board it
/// is supposed to sit on. Twelve is measured against the classic 24-row terminal:
/// a one-row message plus [`MODAL_LIST_CHROME_ROWS`] plus two borders is six rows
/// of chrome, so a full 12-row window lands an 18-row box that still leaves six
/// rows of board visible around it. Whatever the cap, the terminal's own height
/// clamps the box further ([`centered_rect`]) and [`modal_list_window`] scrolls the
/// remainder into reach either way — this only decides how much is offered at once.
///
/// It counts CHOICES, not screen lines. A choice with a wrapped description
/// ([`modal_list_row_lines`]) takes more than one line. The box pays for those
/// extra lines on top of the cap ([`modal_list_lines_asked`]), so a wrapped row
/// never pushes a choice out of the window. A cap in lines would instead offer
/// fewer choices whenever the wrapped row was in view, so the same picker would
/// report a different `N more` count at its two ends.
const MODAL_LIST_MAX_ROWS: u16 = 12;

/// Rows a modal's `Borders::ALL` block costs its content: one top, one bottom.
/// Named because BOTH the height a modal asks for and the viewport
/// [`render_modal`] derives back out of the clamped box subtract it, and the two
/// must be the same number or the list window disagrees with the box drawing it.
const MODAL_BORDER_ROWS: u16 = 2;

/// The marker on a scrolled `List` modal's UPPER spacer row: rows exist above the
/// window. An arrow rather than an ellipsis so the direction to press is the thing
/// the glyph says.
const MODAL_MORE_ABOVE: &str = "\u{2191}";
/// The [`MODAL_MORE_ABOVE`] counterpart on the LOWER spacer row: rows exist below
/// the window.
const MODAL_MORE_BELOW: &str = "\u{2193}";

/// Word-wrap `text` into lines no wider than `width` columns, breaking on
/// whitespace; a single word longer than `width` is kept whole (it clips rather
/// than splitting mid-word — fine for the short, controlled prompts a modal
/// carries). Always returns at least one line so an empty message still reserves a
/// row. Pure, so the wrapped line count that sizes the modal box is unit-testable.
/// Counts by `char`, which is exact for the ASCII prompts these modals use. It is
/// the ONE wrapping rule for a modal: the message and a wrapped `List`
/// description ([`modal_list_row_lines`]) both go through it.
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

/// The interrupt confirmation overlay (`Ctrl-K`), in the shape its
/// [`InterruptRoute`] calls for. Confirming runs `claude stop <job-id>` on the job
/// route, or sends the reported pid a SIGTERM on the signal route — an interrupt
/// either way, so the wording says nothing about a reply, unlike
/// [`render_stop_confirm`].
///
/// The two shapes are drawn from ONE function because they are one confirmation with
/// two handles; the route picks the copy, the title and the box height, and everything
/// else (the [`Clear`], the centering, the label lookup, the footer's shape) is shared
/// so the two cannot drift apart as chrome.
///
/// # The signal variant's copy, and what it may not say
///
/// It names the pid and the session, says a SIGTERM will be sent, and warns that the
/// transcript may end mid-line (a killed `claude -p` can leave a truncated final JSONL
/// line — FAIL-SOFT parsing skips it, which is worth saying HERE rather than leaving a
/// user to discover it afterwards).
///
/// It asserts NOTHING about who owns the process, and that restraint is evidential
/// rather than stylistic. A `kind:"interactive"` record has been measured as two
/// different processes: at `claude 2.1.278` every one (11/11) was a `claude -p` child,
/// and at `claude 2.1.280` both (2/2) were pty-backed TUIs, one `busy` and one
/// `idle`. The evidence and its limits are in `docs/agents/DOMAIN.md`, "What
/// `kind: "interactive"` denotes". Both readings point the same way for the
/// mechanism, so the copy is written to survive EITHER: it describes only what was
/// OBSERVED — claude reports no attachable job, and a process with this pid — and
/// never who started it or which terminal it belongs to. A render test pins the
/// absence of those phrasings.
///
/// Drawn last (on top of the board) with a [`Clear`]. Pure presentation — the target
/// session and the route live on [`App::pending_interrupt`]; styled with named colors
/// and modifiers only (TERMINAL-SAFE STYLING).
fn render_interrupt_confirm(frame: &mut Frame, app: &App) {
    let Some(pending) = &app.pending_interrupt else {
        return;
    };
    let label = app
        .session_by_id(&pending.session_id)
        .map(|s| s.label.as_str())
        .filter(|l| !l.is_empty())
        .unwrap_or(pending.session_id.as_str());

    let warn = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().add_modifier(Modifier::DIM);

    let (title, height, lines) = match &pending.route {
        InterruptRoute::Job { .. } => (
            " stop this agent? ",
            8,
            vec![
                Line::from(Span::styled("This session is running as an agent.", warn)),
                Line::from(""),
                Line::from(Span::raw(format!("Stop it?  \u{2014}  {label}"))),
                Line::from(Span::styled(
                    "(ends the live agent; its conversation is kept)",
                    dim,
                )),
                Line::from(""),
                Line::from(Span::styled("Enter  stop    \u{b7}    Esc  cancel", dim)),
            ],
        ),
        InterruptRoute::Signal { pid } => (
            " signal this process? ",
            11,
            vec![
                Line::from(Span::styled(
                    "claude reports no attachable job for this session.",
                    warn,
                )),
                Line::from(""),
                Line::from(Span::raw(format!(
                    "Send SIGTERM to pid {pid}?  \u{2014}  {label}"
                ))),
                Line::from(Span::styled(
                    "(the pid on the record is the only handle left)",
                    dim,
                )),
                Line::from(""),
                Line::from(Span::styled("The transcript may end mid-line.", dim)),
                Line::from(Span::styled(
                    "snapback reads it fail-soft, so the row stays readable.",
                    dim,
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Enter  send SIGTERM    \u{b7}    Esc  cancel",
                    dim,
                )),
            ],
        ),
    };

    let area = centered_rect(frame.area(), 64, height);
    let block = Block::default().borders(Borders::ALL).title(title);
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
/// colors + modifiers only (terminal-safe). The message accent and alignment are
/// derived from the layout, preserving each overlay's original chrome: a `Row`
/// reads as a warning/confirm (`Yellow`, centered), a `List` as a picker (`Cyan`,
/// left-aligned). The footer is NOT derived from it: the two `List` pickers share a
/// layout but not their verbs, so each modal carries its own ([`Modal::footer`])
/// and this draws it as given.
///
/// A `List` also SCROLLS, which is why this takes `&mut`: the box asks for at most
/// [`MODAL_LIST_MAX_ROWS`] choices' worth of lines, [`centered_rect`] clamps even
/// that on a short terminal, and [`modal_list_window`] then resolves which slice of
/// the choices the surviving lines show — writing the resolved offset back onto the
/// modal the way [`render_list`] writes back `App::scroll`, because only a render
/// knows the viewport. Without it the box drew every choice top-down and a clamped
/// height simply lost the tail: a picker's later rows were unreachable rather than
/// scrolled, and both pickers grow with data (one row per defined agent, one per
/// model alias) rather than being fixed-size.
///
/// A choice is not always one line. Its height is the number of lines
/// [`modal_list_row_lines`] draws for it, and the asked height, the window and the
/// drawn slice all take that height from the SAME built lines.
fn render_modal(frame: &mut Frame, modal: &mut Modal) {
    let accent = match modal.layout {
        ModalLayout::Row => Color::Yellow,
        ModalLayout::List => Color::Cyan,
    };

    // Wrap the message to the box's inner width (borders excluded) so a long prompt
    // — e.g. the delete confirmation — shows in full instead of clipping at the
    // border; the box height below counts the wrapped rows so the two agree.
    let message = wrap_message(&modal.message, MODAL_INNER_WIDTH);
    let message_rows = message.len() as u16;

    // A `List`'s choices as the lines they DRAW, built once. Each row's height is
    // its `len()`, and the box height, the window and the lines drawn below all
    // read that one count. A wrapped description can then never be drawn taller
    // than the arithmetic made room for. Empty for a `Row`, which has no list.
    let list_rows: Vec<Vec<Line<'static>>> = match modal.layout {
        ModalLayout::Row => Vec::new(),
        ModalLayout::List => modal
            .choices
            .iter()
            .enumerate()
            .map(|(i, choice)| modal_list_row_lines(choice, i == modal.selected))
            .collect(),
    };
    let row_heights: Vec<usize> = list_rows.iter().map(Vec::len).collect();
    let max_rows = usize::from(MODAL_LIST_MAX_ROWS);

    // The height the box ASKS for (message rows + chrome + borders; a list also
    // grows with the lines its choices draw, up to the cap). `centered_rect` clamps
    // it, and for a `List` the surviving rows are what the window below is measured
    // against — so the height is resolved BEFORE the window, not alongside it.
    let height = match modal.layout {
        ModalLayout::Row => message_rows
            .saturating_add(MODAL_ROW_CHROME_ROWS)
            .saturating_add(MODAL_BORDER_ROWS),
        ModalLayout::List => u16::try_from(modal_list_lines_asked(&row_heights, max_rows))
            .unwrap_or(u16::MAX)
            .saturating_add(message_rows)
            .saturating_add(MODAL_LIST_CHROME_ROWS)
            .saturating_add(MODAL_BORDER_ROWS),
    };
    let area = centered_rect(frame.area(), MODAL_WIDTH, height);

    let mut lines: Vec<Line> = message
        .into_iter()
        .map(|l| {
            Line::from(Span::styled(
                l,
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ))
        })
        .collect();

    match modal.layout {
        ModalLayout::Row => {
            lines.push(Line::from(""));
            lines.push(Line::from(modal_button_row(&modal.choices, modal.selected)));
            lines.push(Line::from(""));
        }
        ModalLayout::List => {
            // How many list LINES the CLAMPED box actually has room for — the same
            // subtraction the height was built from, run backwards.
            let viewport = usize::from(
                area.height
                    .saturating_sub(message_rows)
                    .saturating_sub(MODAL_LIST_CHROME_ROWS)
                    .saturating_sub(MODAL_BORDER_ROWS),
            );
            let total = list_rows.len();
            modal.scroll = modal_list_window(
                &row_heights,
                modal.selected,
                viewport,
                max_rows,
                modal.scroll,
            );
            let shown = modal_list_shown(&row_heights, modal.scroll, viewport, max_rows);
            // The two spacers carry the overflow hint instead of being blank, so the
            // affordance costs the box nothing.
            lines.push(modal_more_line(MODAL_MORE_ABOVE, modal.scroll));
            // A row taller than the room left is cut at the viewport: its first line
            // (the marker and label) is drawn and the rest is clipped.
            let drawn: Vec<Line<'static>> = list_rows
                .into_iter()
                .skip(modal.scroll)
                .take(shown)
                .flatten()
                .take(viewport)
                .collect();
            // Pad to the viewport, so the lower spacer and the footer stay on the
            // box's last rows when the window holds fewer lines than the tallest
            // window the box was sized for.
            let padding = viewport.saturating_sub(drawn.len());
            lines.extend(drawn);
            lines.extend(std::iter::repeat_n(Line::from(""), padding));
            lines.push(modal_more_line(
                MODAL_MORE_BELOW,
                total.saturating_sub(modal.scroll.saturating_add(shown)),
            ));
        }
    }

    lines.push(Line::from(Span::styled(
        modal.footer,
        Style::default().add_modifier(Modifier::DIM),
    )));

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

/// The FIRST line of a `List`-layout modal row: a [`MODAL_LIST_SELECTED_MARKER`]
/// and a reversed, bold label when selected (the same highlight glyph the session
/// list uses), else a padded label, then the model picker's inline `effort` when
/// there is one ([`modal_effort_span`]), with an optional dim description trailing
/// after [`MODAL_LIST_DESCRIPTION_GAP`]. For most rows this is the whole row. A
/// wrapping choice gets its extra lines from [`modal_list_row_lines`], which owns
/// the rule for how tall a row is. Owns its text (`'static`) so it composes into
/// the modal `Paragraph`. (The picker's old `agent_entry_line`, generalized to any
/// list modal.)
fn modal_list_row(
    label: &str,
    effort: Option<Span<'static>>,
    description: Option<&str>,
    selected: bool,
) -> Line<'static> {
    let (marker, label_style) = if selected {
        (
            MODAL_LIST_SELECTED_MARKER,
            Style::default()
                .add_modifier(Modifier::REVERSED)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (MODAL_LIST_UNSELECTED_MARKER, Style::default())
    };
    let mut spans = vec![
        Span::raw(marker),
        Span::styled(label.to_string(), label_style),
    ];
    spans.extend(effort);
    if let Some(desc) = description {
        spans.push(Span::raw(MODAL_LIST_DESCRIPTION_GAP));
        spans.push(Span::styled(
            desc.to_string(),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    Line::from(spans)
}

/// The inline effort a model-picker row draws right after its label, or `None`.
///
/// Only a model row has one — a [`ModalAction::SetModel`]`(Some(_))` choice, the
/// rows `←`/`→` act on. Every other row, the picker's default row and every
/// agent-picker row included, draws exactly as it always has. On a model row:
///
/// * a SET effort draws as ` · <level>` in [`MODEL_LABEL_STYLE`] whether or not
///   the row is highlighted — each row keeps its own effort while the picker is
///   open, so a level left on a row the highlight moved away from stays visible
///   rather than becoming hidden state;
/// * an UNSET effort draws as a dim ` · default effort` ([`EFFORT_UNSET_LABEL`]) on
///   the HIGHLIGHTED row only, marking the cycle's unset stop where the keys act,
///   and as nothing on any other row.
///
/// The ` · ` is [`MODEL_EFFORT_SEPARATOR`], the compose label's, so a confirmed row
/// reads exactly like the `model:` label it becomes. Pure, so what each row shows is
/// asserted without a terminal.
#[must_use]
fn modal_effort_span(action: &ModalAction, selected: bool) -> Option<Span<'static>> {
    let ModalAction::SetModel(Some(pick)) = action else {
        return None;
    };
    match pick.effort {
        Some(level) => Some(Span::styled(
            format!("{MODEL_EFFORT_SEPARATOR}{level}"),
            MODEL_LABEL_STYLE,
        )),
        None if selected => Some(Span::styled(
            format!("{MODEL_EFFORT_SEPARATOR}{EFFORT_UNSET_LABEL}"),
            Style::default().add_modifier(Modifier::DIM),
        )),
        None => None,
    }
}

/// The column inside the box where a `List`-modal row's description starts:
/// marker, label, then the gap.
///
/// A WRAPPED description indents its continuation lines to exactly this column (a
/// hanging indent). That is also why one wrap width serves every line: the room
/// beside the label on the first line and the room under it on each later line
/// are both `MODAL_INNER_WIDTH - column`. So [`wrap_message`] wraps the whole
/// description in one call, and no second rule with two widths is needed. Counted
/// by `char`, the way `wrap_message` counts.
fn modal_list_description_column(label: &str) -> u16 {
    let column = MODAL_LIST_SELECTED_MARKER.chars().count()
        + label.chars().count()
        + MODAL_LIST_DESCRIPTION_GAP.chars().count();
    u16::try_from(column).unwrap_or(u16::MAX)
}

/// One `List`-modal choice as the lines it DRAWS, which also makes it the row's
/// height. Everything that counts a choice's lines reads the `len()` of this: the
/// box height ([`modal_list_lines_asked`]), the scroll window
/// ([`modal_list_window`]) and the drawn slice ([`modal_list_shown`]). One wrap
/// produces the lines and the count, so the drawing and the arithmetic cannot
/// disagree.
///
/// A choice is ONE line ([`modal_list_row`]), its description trailing the label
/// and clipped at the border, unless it opts into
/// [`ModalChoice::wrap_description`]. For such a choice, [`wrap_message`] wraps
/// the description to the room right of the label. The first chunk trails the
/// label as usual. Each further chunk gets its own DIM line, indented to the
/// description's column ([`modal_list_description_column`]), so the whole text is
/// drawn and reads as part of the row. The line count is whatever the wrap
/// produced, never a fixed number: it moves with the label's width (a reply's
/// `session's model (Opus 5.5)` leaves less room beside it than a bare `default`)
/// and with the description it explains. Named modifiers only (TERMINAL-SAFE
/// STYLING).
fn modal_list_row_lines(choice: &ModalChoice, selected: bool) -> Vec<Line<'static>> {
    let description = choice.description.as_deref();
    let effort = modal_effort_span(&choice.action, selected);
    let Some(text) = description.filter(|_| choice.wrap_description) else {
        return vec![modal_list_row(&choice.label, effort, description, selected)];
    };
    // A wrapping row's hanging indent is measured from the LABEL alone, so it must
    // never also carry an effort. It cannot today: only the model picker's
    // default row wraps, and that row holds no pick (so no effort).
    debug_assert!(effort.is_none(), "a wrapping row never carries an effort");
    let column = modal_list_description_column(&choice.label);
    let mut chunks = wrap_message(text, MODAL_INNER_WIDTH.saturating_sub(column)).into_iter();
    // `wrap_message` always yields at least one line; the default is unreachable.
    let first = chunks.next().unwrap_or_default();
    let indent = " ".repeat(usize::from(column));
    let mut lines = vec![modal_list_row(&choice.label, None, Some(&first), selected)];
    lines.extend(chunks.map(|chunk| {
        Line::from(vec![
            Span::raw(indent.clone()),
            Span::styled(chunk, Style::default().add_modifier(Modifier::DIM)),
        ])
    }));
    lines
}

/// The least first row `start <= end` from which rows `start..=end` are all drawn
/// WHOLE: at most `max_rows` of them, in at most `viewport` lines, given each
/// row's line count in `heights`.
///
/// When `end` ALONE is taller than the viewport, the answer is `end` itself. Its
/// top lines, which carry the marker and the label, are drawn and the rest is
/// clipped. That keeps a row taller than the whole box selectable instead of
/// unreachable. `end` past the last row is treated as the last row, and an empty
/// list answers 0. Pure.
#[must_use]
fn modal_list_first_fitting(
    heights: &[usize],
    end: usize,
    viewport: usize,
    max_rows: usize,
) -> usize {
    let Some(last) = heights.len().checked_sub(1) else {
        return 0;
    };
    let end = end.min(last);
    // `start` is exclusive of what is taken so far: `start..=end` fits.
    let mut start = end + 1;
    let mut used = 0usize;
    while start > 0 && end + 1 - start < max_rows {
        let next = used.saturating_add(heights[start - 1]);
        if next > viewport {
            break;
        }
        used = next;
        start -= 1;
    }
    start.min(end)
}

/// Resolve a `List`-layout modal's scroll window: given each choice's line count
/// in `heights` (from [`modal_list_row_lines`]), the `selected` index, a
/// `viewport` of that many drawable LINES holding at most `max_rows` choices, and
/// the offset the modal is CURRENTLY scrolled to, return the index of the first
/// choice to draw.
///
/// The rule is ratatui `ListState`'s, which is the board list's rule (PATTERNS §5,
/// via `render_list`) rather than a second scrolling idiom: keep the current offset
/// wherever the selection is still inside it, and otherwise move by the least
/// amount that brings the selection back — to the top when it sits above the
/// window, to the bottom when it sits below. That is what makes SELECTION FOLLOW
/// SCROLL: whatever `App::cycle_modal` picked, including a `rem_euclid` wrap from
/// one end of the list to the other, the returned window contains it. "Inside"
/// means drawn WHOLE: every line of a wrapped selected row, not only its first
/// ([`modal_list_first_fitting`]). The exception is a selected row taller than the
/// whole viewport, which is shown from its top.
///
/// Total and saturating over every degenerate input, because the viewport is
/// derived from a terminal the user can size freely: a `viewport` of 0 (a box with
/// no room for a single line) answers 0 and draws nothing rather than underflowing,
/// a `viewport` of 1 pins the window to the selection, a list SHORTER than the
/// viewport answers 0 (there is nothing to scroll), and a `selected` past the end
/// is treated as the last row rather than trusted.
///
/// Pure, so the window arithmetic is unit-tested without a terminal.
#[must_use]
fn modal_list_window(
    heights: &[usize],
    selected: usize,
    viewport: usize,
    max_rows: usize,
    current: usize,
) -> usize {
    let Some(last) = heights.len().checked_sub(1) else {
        return 0;
    };
    if viewport == 0 || max_rows == 0 {
        return 0;
    }
    let selected = selected.min(last);
    // Past this offset the window would end before the list does, leaving blank
    // lines below the last row. So an offset a longer list or a shorter terminal
    // left behind is pulled back here first.
    let max_scroll = modal_list_first_fitting(heights, last, viewport, max_rows);
    // Every offset from `lowest` to `selected` draws the selection whole, so
    // clamping into that range is exactly the least move that brings it back. The
    // clamp cannot push past `max_scroll`: `lowest <= max_scroll` because
    // `selected <= last`, and a later row never fits from an earlier start.
    let lowest = modal_list_first_fitting(heights, selected, viewport, max_rows);
    current.min(max_scroll).clamp(lowest, selected)
}

/// How many choices, starting at `scroll`, get at least their FIRST line drawn in
/// a `viewport` of that many lines holding at most `max_rows` choices. A row cut
/// off at the viewport's end counts as shown: its label is on screen, so the
/// `N more` count below it leaves it out. Pure.
#[must_use]
fn modal_list_shown(heights: &[usize], scroll: usize, viewport: usize, max_rows: usize) -> usize {
    let mut used = 0usize;
    let mut shown = 0usize;
    for &height in heights.iter().skip(scroll) {
        if shown == max_rows || used >= viewport {
            break;
        }
        used = used.saturating_add(height);
        shown += 1;
    }
    shown
}

/// The list lines a `List` modal ASKS for: what the tallest run of `max_rows`
/// consecutive choices takes, or the whole list's lines when it has no more
/// choices than that.
///
/// Using the TALLEST run makes the box one fixed height for the whole modal. The
/// window can then scroll anywhere, including onto a wrapped row, without pushing
/// a choice out of the [`MODAL_LIST_MAX_ROWS`] it offers. A box sized to the
/// current window would change height under the user's keypress. A window with no
/// wrapped row in it leaves the extra lines blank at the bottom of the list.
/// Pure.
#[must_use]
fn modal_list_lines_asked(heights: &[usize], max_rows: usize) -> usize {
    if max_rows == 0 {
        return 0;
    }
    if heights.len() <= max_rows {
        return heights.iter().sum();
    }
    heights
        .windows(max_rows)
        .map(|run| run.iter().sum::<usize>())
        .max()
        .unwrap_or(0)
}

/// The dim `↑ N more` / `↓ N more` marker a scrolled `List` modal draws on the
/// spacer row [`MODAL_LIST_CHROME_ROWS`] already reserves, or a blank line when
/// nothing is off-window in that direction.
///
/// It costs the box no height at all — the spacer was there and blank — which is
/// why the affordance is here rather than as a row of its own: a picker that had to
/// grow to admit it would be fighting the very clamp this viewport exists to
/// survive. Named ANSI arrows + `Modifier::DIM` only, no RGB and no raw escapes
/// (TERMINAL-SAFE STYLING); the leading two spaces line the marker up with
/// [`modal_list_row`]'s unselected indent so it reads as part of the list.
fn modal_more_line(arrow: &str, hidden: usize) -> Line<'static> {
    if hidden == 0 {
        return Line::from("");
    }
    Line::from(Span::styled(
        format!("  {arrow} {hidden} more"),
        Style::default().add_modifier(Modifier::DIM),
    ))
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
/// light-blue italic underlined link labels). So this splits the line's own spans
/// at the matched positions and ADDS the modifier to the matched runs, rather than
/// replacing their style — a marked word inside a DIM code span stays DIM, and a
/// marked link keeps its color, italic and underline.
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
    use crate::send::OWNERSHIP_CLAIMS;
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

    /// The header is scope, search and counts ONLY: no `model:` segment. A model is
    /// picked per compose now, so the board has no model of its own to name — and
    /// a pick made in a compose must not leak onto the header either.
    #[test]
    fn the_header_names_no_model() {
        let mut app = App::new(
            vec![sample_session()],
            Scope::CurrentFolder,
            PathBuf::from("/tmp/launch"),
        );
        app.set_settings_model(Some("opus[1m]".to_string()));
        let header = drawn_header(&app);
        assert!(
            header.contains("folder:launch") && header.contains("search: name"),
            "the scope and search segments are drawn: {header}"
        );
        crate::tui::compose::open_background(&mut app, None);
        app.set_compose_model(Some(ModelPick::new("haiku")));
        let composing = drawn_header(&app);
        for leaked in ["model:", "opus[1m]", "haiku", MODEL_NEW_SESSION_SCOPE] {
            assert!(
                !header.contains(leaked) && !composing.contains(leaked),
                "the header must not name a model ({leaked:?}): {header} / {composing}"
            );
        }
        let counts = app.session_counts();
        assert!(
            header.contains(&format!("{} / {} sessions", counts.visible, counts.total)),
            "the counter is whole at the harness width: {header}"
        );
    }

    /// [`model_pick_label`] is a pick's wording, stated directly.
    #[test]
    fn the_model_pick_label_is_the_model_then_its_effort() {
        assert_eq!(model_pick_label(&ModelPick::new("sonnet")), "sonnet");
        assert_eq!(
            model_pick_label(&ModelPick {
                model: "sonnet".to_string(),
                effort: Some("xhigh"),
            }),
            "sonnet · xhigh"
        );
    }

    /// [`compose_model_label`]'s wording AND styling per case, stated directly: the
    /// value that names a model (`default` included) in magenta, the `model: `
    /// prefix, the draft's `(new sessions only)` scope and the padding unstyled.
    #[test]
    fn the_compose_model_label_names_each_case_and_styles_only_the_value() {
        let magenta = Style::default().fg(Color::Magenta);
        let label = |pick: Option<ModelPick>, default: ComposeDefault| {
            compose_model_label(pick.as_ref(), &default).spans
        };

        // A reply's default: the session's own model, or `default` when claude
        // would not restore it.
        assert_eq!(
            label(None, ComposeDefault::SessionModel("Opus 5.5".to_string())),
            vec![
                Span::raw(" model: "),
                Span::styled("session (Opus 5.5)", magenta),
                Span::raw(" "),
            ]
        );
        for plain in [
            ComposeDefault::RestoreOverridden,
            ComposeDefault::NoSessionModel,
            ComposeDefault::BuiltIn,
        ] {
            assert_eq!(
                label(None, plain.clone()),
                vec![
                    Span::raw(" model: "),
                    Span::styled("default", magenta),
                    Span::raw(" "),
                ],
                "{plain:?} reads a bare `model: default`"
            );
        }
        // A draft's default with a settings value: scoped to new sessions, the
        // scope unstyled.
        assert_eq!(
            label(None, ComposeDefault::Settings("opus[1m]".to_string())),
            vec![
                Span::raw(" model: "),
                Span::styled("default (opus[1m])", magenta),
                Span::raw(" (new sessions only)"),
                Span::raw(" "),
            ]
        );
        // A pick replaces whichever default, with its effort after it.
        for default in [
            ComposeDefault::SessionModel("Opus 5.5".to_string()),
            ComposeDefault::Settings("opus[1m]".to_string()),
        ] {
            assert_eq!(
                label(Some(ModelPick::new("sonnet")), default.clone()),
                vec![
                    Span::raw(" model: "),
                    Span::styled("sonnet", magenta),
                    Span::raw(" "),
                ],
                "a pick replaces the {default:?} default"
            );
        }
        assert_eq!(
            label(
                Some(ModelPick {
                    model: "opus".to_string(),
                    effort: Some("high"),
                }),
                ComposeDefault::BuiltIn,
            ),
            vec![
                Span::raw(" model: "),
                Span::styled("opus · high", magenta),
                Span::raw(" "),
            ]
        );
    }

    /// A board row backed by a committed PREVIEW fixture, so drawing its preview
    /// really parses a transcript — the one way a session's model is on record.
    fn preview_fixture_row(id: &str, file: &str) -> Session {
        let mut row = sample_session();
        row.session_id = id.to_string();
        row.label = id.to_string();
        row.file = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("preview")
            .join(file);
        row
    }

    /// A board wide and tall enough that the compose box DOCKS in the preview pane
    /// and its bottom border is wide enough for the longest label these cases draw
    /// (`model: default (opus[1m]) (new sessions only)`) with room to spare.
    const LABEL_BOARD: (u16, u16) = (140, 30);

    /// The compose box's bottom border row — the row its `model:` label rides on —
    /// as drawn, with its index: the first `└` row BELOW the row whose top border
    /// carries `title`. Read off the buffer, never off the geometry under test.
    fn drawn_compose_bottom(
        buffer: &ratatui::buffer::Buffer,
        (width, height): (u16, u16),
        title: &str,
    ) -> (u16, String) {
        let rows: Vec<String> = (0..height)
            .map(|y| full_row_text(buffer, y, width))
            .collect();
        let top = rows
            .iter()
            .position(|row| row.contains(title))
            .unwrap_or_else(|| panic!("the compose box titled {title:?} is drawn"));
        let bottom = (top + 1..rows.len())
            .find(|&y| rows[y].contains(BOX_BOTTOM_LEFT))
            .expect("the compose box is closed by a bottom border");
        (
            u16::try_from(bottom).expect("a terminal row"),
            rows[bottom].clone(),
        )
    }

    /// The foreground colour of every cell `needle` covers on row `y`, as drawn.
    fn row_fgs(buffer: &ratatui::buffer::Buffer, y: u16, width: u16, needle: &str) -> Vec<Color> {
        let text = full_row_text(buffer, y, width);
        let byte = text
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} is not drawn on row {y}: {text}"));
        let start = u16::try_from(text[..byte].chars().count()).expect("a column");
        let len = u16::try_from(needle.chars().count()).expect("a short needle");
        (start..start + len)
            .map(|x| buffer.cell((x, y)).expect("on screen").fg)
            .collect()
    }

    /// A REPLY box names what the reply runs on, on its bottom border, drawn — and
    /// the label follows the state: the session's own model by default (the newest
    /// one the transcript recorded), `default` once an environment override means
    /// claude would not restore it, and the compose's own pick once one is made.
    /// The value is magenta (spelled out here rather than read back from
    /// [`MODEL_LABEL_STYLE`], so a change to that constant turns this red) and none
    /// of it ever reaches the status line.
    #[test]
    fn a_reply_box_names_the_model_the_reply_runs_on() {
        let mut app = App::new(
            vec![preview_fixture_row(
                "sbv-switch",
                "sess-model-switch-1.jsonl",
            )],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        crate::tui::compose::open(&mut app, "sbv-switch".to_string(), None);

        let buffer = drawn_board(&mut app, LABEL_BOARD.0, LABEL_BOARD.1);
        let (y, bottom) = drawn_compose_bottom(&buffer, LABEL_BOARD, COMPOSE_TITLE_MARKER);
        assert!(
            bottom.contains("model: session (Sonnet 5)"),
            "the reply's default is its session's newest answering model: {bottom}"
        );
        assert!(
            row_fgs(&buffer, y, LABEL_BOARD.0, "session (Sonnet 5)")
                .iter()
                .all(|fg| *fg == Color::Magenta),
            "the value is drawn magenta"
        );
        assert!(
            row_fgs(&buffer, y, LABEL_BOARD.0, "model: ")
                .iter()
                .all(|fg| *fg != Color::Magenta),
            "the prefix is not"
        );

        app.set_restore_overridden(true);
        let buffer = drawn_board(&mut app, LABEL_BOARD.0, LABEL_BOARD.1);
        let (_, bottom) = drawn_compose_bottom(&buffer, LABEL_BOARD, COMPOSE_TITLE_MARKER);
        assert!(
            bottom.contains("model: default ") && !bottom.contains("session ("),
            "an override means claude will not restore it, so the box says default: \
             {bottom}"
        );

        app.set_compose_model(Some(ModelPick {
            model: "opus".to_string(),
            effort: Some("high"),
        }));
        let buffer = drawn_board(&mut app, LABEL_BOARD.0, LABEL_BOARD.1);
        let (y, bottom) = drawn_compose_bottom(&buffer, LABEL_BOARD, COMPOSE_TITLE_MARKER);
        assert!(
            bottom.contains("model: opus · high"),
            "a pick names the alias and its effort: {bottom}"
        );
        assert!(
            row_fgs(&buffer, y, LABEL_BOARD.0, "opus · high")
                .iter()
                .all(|fg| *fg == Color::Magenta),
            "the whole pick, effort included, is magenta"
        );
        assert_eq!(
            app.status, None,
            "the label is compose STATE, never a status-line message"
        );
    }

    /// A reply whose transcript records NO answering model says `model: default`:
    /// claude has nothing to restore, so naming a session model would be a guess.
    #[test]
    fn a_reply_with_no_answering_model_on_record_says_default() {
        let mut app = App::new(
            vec![preview_fixture_row(
                "sbv-absent",
                "sess-model-absent-1.jsonl",
            )],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        crate::tui::compose::open(&mut app, "sbv-absent".to_string(), None);
        let buffer = drawn_board(&mut app, LABEL_BOARD.0, LABEL_BOARD.1);
        let (_, bottom) = drawn_compose_bottom(&buffer, LABEL_BOARD, COMPOSE_TITLE_MARKER);
        assert!(
            bottom.contains("model: default ") && !bottom.contains("session ("),
            "no model on record: {bottom}"
        );
    }

    /// A DRAFT box names what the new session starts on: bare `model: default`
    /// while the settings name nothing, and `model: default (<value>) (new sessions
    /// only)` once they do — the value magenta, the scope not.
    #[test]
    fn a_draft_box_names_the_settings_model_scoped_to_new_sessions() {
        /// The draft box's top-border title, used to find it on the board.
        const DRAFT_TITLE: &str = "new background agent";
        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        crate::tui::compose::open_background(&mut app, None);

        let buffer = drawn_board(&mut app, LABEL_BOARD.0, LABEL_BOARD.1);
        let (_, bottom) = drawn_compose_bottom(&buffer, LABEL_BOARD, DRAFT_TITLE);
        assert!(
            bottom.contains("model: default ") && !bottom.contains(MODEL_NEW_SESSION_SCOPE),
            "no settings value: a bare default, with nothing to scope: {bottom}"
        );

        app.set_settings_model(Some("opus[1m]".to_string()));
        let buffer = drawn_board(&mut app, LABEL_BOARD.0, LABEL_BOARD.1);
        let (y, bottom) = drawn_compose_bottom(&buffer, LABEL_BOARD, DRAFT_TITLE);
        assert!(
            bottom.contains("model: default (opus[1m]) (new sessions only)"),
            "the settings value is named and scoped: {bottom}"
        );
        assert!(
            row_fgs(&buffer, y, LABEL_BOARD.0, "default (opus[1m])")
                .iter()
                .all(|fg| *fg == Color::Magenta),
            "`default` and its value are magenta"
        );
        assert!(
            row_fgs(&buffer, y, LABEL_BOARD.0, " (new sessions only)")
                .iter()
                .all(|fg| *fg != Color::Magenta),
            "the scope, leading space included, is not"
        );
    }

    /// The label rides the box's BORDER, so it costs the editor NO row: on the
    /// short board — the full-width bottom bar, where rows are scarcest — an empty
    /// draft's box is still exactly one text row tall, and the label sits on the
    /// border row that closes it.
    #[test]
    fn the_model_label_costs_the_editor_no_row_even_in_the_bottom_bar() {
        let (width, height) = BAR_BOARD;
        assert!(
            compose_uses_bottom_bar(true, height),
            "the fixture must be the bottom-bar layout"
        );
        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        crate::tui::compose::open(&mut app, "sess-normal-1".to_string(), None);
        let buffer = drawn_board(&mut app, width, height);

        assert_eq!(
            drawn_compose_text_rows(&buffer, width, height).len(),
            1,
            "an empty draft keeps its one text row"
        );
        let (_, bottom) = drawn_compose_bottom(&buffer, (width, height), COMPOSE_TITLE_MARKER);
        assert!(
            bottom.contains("model: "),
            "the label is on the closing border row: {bottom}"
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
            background: false,
            has_agent_name: false,
            has_agent_setting: false,
            failed_task: None,
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
            app.query().is_empty(),
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
    /// A TEST FOIL, and only that — which is why it lives in `mod tests`. NOTHING in
    /// production models a wrap: the transcript's height and the per-line map of
    /// where each line starts are both asked of the widget (`wrapped_text_rows` /
    /// `wrapped_row_prefix`), the click hit-test resolves a clicked ROW through that
    /// same map, and it resolves the CELL inside that row by re-rendering the line
    /// ([`region_paints_cell`]) rather than by packing characters into it.
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
    ///
    /// It outlived the packed COLUMN step in `link_at` on purpose. Both claims here
    /// are about HEIGHT, measured against the rows actually PAINTED, and height is
    /// still asked of the widget everywhere the pane computes an offset — so this
    /// keeps pinning why `wrapped_text_rows` and `wrapped_row_prefix` may not be
    /// swapped for arithmetic. It also keeps `wrapped_line_height` honest for the one
    /// fixture that still needs a foil: `a_click_below_wrapping_lines_resolves_the_link_under_it`
    /// measures the drift a packed walk would have accumulated, to prove its lines
    /// really wrap before it asserts anything about a click.
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

    /// A `MarkerLine` test fixture: `content_row` plus a distinguishing label so
    /// a test can name which marker won without depending on styling.
    fn marker(content_row: usize, label: &str) -> preview::MarkerLine {
        preview::MarkerLine {
            content_row,
            line: Line::from(label.to_string()),
        }
    }

    /// An UNWRAPPED [`wrapped_row_prefix`]-shaped map over `lines` logical lines —
    /// every line occupies exactly one screen row, so a visual row and a content row
    /// coincide 1:1 and these tests read as the OWNERSHIP rule alone. The wrapped
    /// case is covered separately, against the real wrapper, by
    /// `the_banner_tracks_the_top_turn_across_a_wrapped_table_row`.
    fn flat_prefix(lines: usize) -> Vec<usize> {
        (0..=lines).collect()
    }

    #[test]
    fn marker_at_top_owns_the_last_marker_at_or_before_the_target_row() {
        let markers = [marker(0, "m0"), marker(4, "m4"), marker(9, "m9")];
        let prefix = flat_prefix(12);

        // Exactly on a marker's own row.
        assert_eq!(
            marker_at_top(&markers, &prefix, 4).map(|l| l.to_string()),
            Some("m4".to_string())
        );
        // A body row under a turn belongs to the marker that PRECEDES it, not
        // the next one — this is the "turn ownership" the pinned banner relies
        // on: scrolling to any row of a turn, not only its exact marker row,
        // must still show that turn's marker.
        assert_eq!(
            marker_at_top(&markers, &prefix, 6).map(|l| l.to_string()),
            Some("m4".to_string()),
            "a row under a turn must resolve to that turn's marker, not the next one"
        );
        // Past the last marker's row: the last marker still owns every row
        // after it (its own body).
        assert_eq!(
            marker_at_top(&markers, &prefix, 11).map(|l| l.to_string()),
            Some("m9".to_string())
        );
    }

    /// The lookup follows the MAP, so a turn whose lines WRAP is still owned by its
    /// own marker at every one of its wrapped rows.
    ///
    /// The map below is one no `ceil(width / inner_width)` packing walk produces for
    /// these lines: line 1 takes three rows, so the second turn's marker (content row
    /// 2) opens at visual row 4. A width model that called line 1 two rows would
    /// resolve visual row 4 to content row 3 and pin the SECOND turn one row early —
    /// and the drift compounds with every wrapping line above, which is exactly the
    /// bug this signature change removes.
    #[test]
    fn marker_at_top_follows_the_map_where_a_width_model_would_pin_the_wrong_turn() {
        let markers = [marker(0, "first"), marker(2, "second")];
        // line 0: 1 row, line 1: 3 wrapped rows, line 2 (the marker): 1 row, line 3: 1 row.
        let prefix = [0usize, 1, 4, 5, 6];
        for row in 0..4 {
            assert_eq!(
                marker_at_top(&markers, &prefix, row).map(|l| l.to_string()),
                Some("first".to_string()),
                "visual row {row} is still inside the FIRST turn's wrapped body"
            );
        }
        assert_eq!(
            marker_at_top(&markers, &prefix, 4).map(|l| l.to_string()),
            Some("second".to_string()),
            "row 4 is where the wrapper actually starts the second turn's marker"
        );
    }

    #[test]
    fn marker_at_top_falls_back_to_the_first_marker_above_every_one() {
        // A target row that sits ABOVE the very first marker (the blank line
        // that leads the first turn, e.g.) still resolves to the OPENING turn's
        // marker rather than to nothing — there being nothing rendered before it.
        let markers = [marker(2, "m2")];
        assert_eq!(
            marker_at_top(&markers, &flat_prefix(5), 0).map(|l| l.to_string()),
            Some("m2".to_string())
        );
    }

    #[test]
    fn marker_at_top_is_none_with_no_markers_or_past_the_content() {
        let prefix = flat_prefix(5);
        assert_eq!(marker_at_top(&[], &prefix, 0), None, "no markers at all");
        let markers = [marker(0, "m0")];
        assert_eq!(
            marker_at_top(&markers, &prefix, 99),
            None,
            "an offset past the end of the content has no owning row"
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

    /// A content line carrying a link label at display columns `col_start..col_end`,
    /// padded out to `cols` with dots so the columns around it are real drawn cells
    /// rather than the blank tail of a short line.
    ///
    /// Dots, not spaces: the wrapper breaks on whitespace, so a padded line of spaces
    /// would wrap somewhere a test has no reason to expect. A run of dots is one
    /// unbreakable token, which puts the break exactly at the pane width.
    fn link_line(cols: usize, col_start: usize, col_end: usize) -> Line<'static> {
        Line::from(vec![
            Span::raw(".".repeat(col_start)),
            Span::styled(
                ".".repeat(col_end - col_start),
                Style::default().add_modifier(Modifier::UNDERLINED),
            ),
            Span::raw(".".repeat(cols.saturating_sub(col_end))),
        ])
    }

    #[test]
    fn link_at_returns_url_inside_a_link_and_none_just_outside() {
        let inner = inner_rect();
        // Three unwrapped content lines; a link on line 2 at columns 4..8.
        let lines = vec![
            Line::from(String::new()),
            Line::from(String::new()),
            link_line(12, 4, 8),
        ];
        let prefix = wrapped_row_prefix(&lines, inner.width);
        assert_eq!(prefix, vec![0, 1, 2, 3], "each line must fit one row");
        let regions = [region(2, 4, 8, "u")];
        // Inside the label (content col 4..7) -> the url.
        assert_eq!(
            link_at(
                inner.x + 4,
                inner.y + 2,
                inner,
                0,
                &prefix,
                &lines,
                &regions
            ),
            LinkProbe::Hit("u")
        );
        assert_eq!(
            link_at(
                inner.x + 7,
                inner.y + 2,
                inner,
                0,
                &prefix,
                &lines,
                &regions
            ),
            LinkProbe::Hit("u")
        );
        // One cell past the end (col_end is exclusive) -> no link. NOT `Unresolvable`:
        // the probe was spent and came back empty, which is a different answer.
        assert_eq!(
            link_at(
                inner.x + 8,
                inner.y + 2,
                inner,
                0,
                &prefix,
                &lines,
                &regions
            ),
            LinkProbe::NoLink
        );
        // One cell before the start -> no link.
        assert_eq!(
            link_at(
                inner.x + 3,
                inner.y + 2,
                inner,
                0,
                &prefix,
                &lines,
                &regions
            ),
            LinkProbe::NoLink
        );
    }

    /// The budget bounds the PRODUCT, so neither factor alone decides.
    ///
    /// Both single-dimension caps that were rejected are pinned here as the cases
    /// they would have let through: a short line carrying many links, and a long line
    /// carrying one. Either would pass a cap on the other dimension.
    #[test]
    fn the_probe_budget_bounds_bytes_times_candidates_not_either_alone() {
        // An ordinary click is nowhere near it: a wrapped paragraph, a link or two.
        assert!(
            probe_within_budget(400, 2),
            "an ordinary prose line with links"
        );
        assert!(
            probe_within_budget(800, 8),
            "and a pane-clamped grid table row with one link per cell"
        );

        // The boundary, from each side and in each dimension.
        assert!(
            probe_within_budget(1, LINK_PROBE_BYTE_BUDGET),
            "at the budget"
        );
        assert!(
            !probe_within_budget(1, LINK_PROBE_BYTE_BUDGET + 1),
            "one candidate past it"
        );
        assert!(
            probe_within_budget(LINK_PROBE_BYTE_BUDGET, 1),
            "and in bytes"
        );
        assert!(
            !probe_within_budget(LINK_PROBE_BYTE_BUDGET + 1, 1),
            "one byte past it"
        );

        // The product is the quantity spent: a line well under any plausible length
        // cap, carrying a count well under any plausible candidate cap, still costs
        // more than the budget between them.
        assert!(
            probe_within_budget(BUDGET_LINE_BYTES, BUDGET_AT_LIMIT_CANDIDATES),
            "an ordinary-length line, 32 links deep, is the budget exactly"
        );
        assert!(
            !probe_within_budget(BUDGET_LINE_BYTES, BUDGET_AT_LIMIT_CANDIDATES + 1),
            "and one link deeper is past it"
        );

        // Saturating, so an absurd pair cannot wrap back under the budget and buy
        // itself the work this exists to refuse.
        assert!(!probe_within_budget(usize::MAX, 2));
        assert!(!probe_within_budget(2, usize::MAX));
    }

    /// The gap a WRAPPED-ROW budget could not see, and the whole reason this one is
    /// spent in bytes: a line whose graphemes occupy no column costs a full read per
    /// candidate while measuring ONE row tall.
    ///
    /// Rows count what the wrapper PAINTS; the probe pays for what it READS. A run of
    /// zero-width spaces is read and never painted, so a row budget prices this click
    /// at `1 * candidates` and waves it through, while every candidate still walks,
    /// re-styles, copies and re-wraps every byte the line holds. Transcript text is
    /// whatever a model or a tool dumped into the session, so the shape is reachable
    /// without anything exotic — which is why both prices are asserted here, not just
    /// the right one.
    #[test]
    fn a_zero_width_line_costs_bytes_the_wrapped_rows_cannot_see() {
        const ZERO_WIDTH: &str = "\u{200b}";
        const GRAPHEMES: usize = 8192;
        const CANDIDATES: usize = 8;
        let inner = inner_rect();
        let lines = vec![Line::from(ZERO_WIDTH.repeat(GRAPHEMES))];
        let bytes = line_probe_bytes(&lines[0]);
        assert_eq!(
            bytes,
            GRAPHEMES * ZERO_WIDTH.len(),
            "the fixture must really hold the bytes it claims"
        );
        assert_eq!(
            lines[0].width(),
            0,
            "and none of them may take a column, or the wrapped rows would see them"
        );
        assert_eq!(
            wrapped_row_prefix(&lines, inner.width),
            vec![0, 1],
            "the wrapper paints this as ONE row however much text it holds — which is \
             exactly why rows cannot price the probe"
        );
        assert!(
            probe_within_budget(1, CANDIDATES),
            "priced in ROWS this click is trivially affordable"
        );
        assert!(
            !probe_within_budget(bytes, CANDIDATES),
            "priced in the bytes it actually reads it is not, and that is the answer \
             the budget has to give"
        );
    }

    /// Bytes in the line the end-to-end boundary below is measured on: an ordinary
    /// paragraph's worth of text, and an exact divisor of [`LINK_PROBE_BYTE_BUDGET`]
    /// so a candidate count can straddle the boundary precisely rather than near it.
    const BUDGET_LINE_BYTES: usize = 4096;

    /// Candidates that spend [`BUDGET_LINE_BYTES`] EXACTLY up to the budget — the last
    /// count still answered, so `+ 1` is the first one refused.
    const BUDGET_AT_LIMIT_CANDIDATES: usize = LINK_PROBE_BYTE_BUDGET / BUDGET_LINE_BYTES;

    /// Past the budget the hit-test ABSTAINS — even on a cell a region plainly
    /// painted, which is the only version of this that proves anything.
    ///
    /// The two runs differ in ONE candidate and nothing else, so what is pinned is the
    /// boundary itself rather than "a big number abstains". The hitting region is
    /// FIRST in both, so the at-budget run short-circuits on it: the over-budget run
    /// therefore refuses because of the BUDGET, not because it ran out of regions to
    /// try, and abstaining costs a hit that was genuinely available.
    ///
    /// The abstention is asserted as [`LinkProbe::Unresolvable`] and NOT as
    /// [`LinkProbe::NoLink`], which is the whole point of there being two: this fixture
    /// has a link on the clicked cell, so "no link" would be a false statement about
    /// the transcript rather than an honest refusal to answer.
    #[test]
    fn link_at_abstains_once_a_click_would_cost_more_than_the_probe_budget() {
        let inner = inner_rect();
        let lines = vec![link_line(BUDGET_LINE_BYTES, 4, 8)];
        let prefix = wrapped_row_prefix(&lines, inner.width);
        assert_eq!(
            line_probe_bytes(&lines[0]),
            BUDGET_LINE_BYTES,
            "the dot fixture must be one byte per column, or the two runs below are \
             not measured in the budget's own unit"
        );
        assert_eq!(
            BUDGET_AT_LIMIT_CANDIDATES * BUDGET_LINE_BYTES,
            LINK_PROBE_BYTE_BUDGET,
            "the fixture must divide the budget EXACTLY, or the two runs straddle \
             nothing and the boundary is not pinned"
        );

        // Decoys sit on the same content line at columns the click never touches, so
        // they are CANDIDATES (same line) that can never answer.
        let decoy = region(0, 0, 1, "decoy");
        let mut at_budget = vec![region(0, 4, 8, "u")];
        at_budget.resize(BUDGET_AT_LIMIT_CANDIDATES, decoy.clone());
        assert_eq!(at_budget.len(), BUDGET_AT_LIMIT_CANDIDATES);
        assert_eq!(
            link_at(inner.x + 4, inner.y, inner, 0, &prefix, &lines, &at_budget),
            LinkProbe::Hit("u"),
            "AT the budget the click is answered exactly as it always was"
        );

        let mut past_budget = at_budget;
        past_budget.push(decoy);
        assert_eq!(past_budget.len(), BUDGET_AT_LIMIT_CANDIDATES + 1);
        assert_eq!(
            link_at(
                inner.x + 4,
                inner.y,
                inner,
                0,
                &prefix,
                &lines,
                &past_budget
            ),
            LinkProbe::Unresolvable,
            "ONE region past it the probe must abstain rather than answer from work \
             it refuses to do — a missed hit costs a click, a guessed one hands a \
             browser a url the reader never aimed at. And it must abstain IN ITS OWN \
             WORDS: the very same click is a HIT one candidate earlier, so reporting \
             `NoLink` would deny a link this fixture demonstrably has"
        );
    }

    #[test]
    fn link_at_is_none_on_blank_rows_and_outside_the_pane() {
        let inner = inner_rect();
        // Line 0 carries TEXT at the very columns the link occupies on line 2; line 1
        // is blank. The text line is the one that earns its place: against a BLANK
        // line, a hit-test that forgot WHICH line a region belongs to would still
        // answer None, because nothing is painted there to match — so the row gate
        // would go untested. Here a forgotten gate resolves line 0's prose as a link.
        let lines = vec![
            Line::from(".".repeat(12)),
            Line::from(String::new()),
            link_line(12, 4, 8),
        ];
        let prefix = wrapped_row_prefix(&lines, inner.width);
        let regions = [region(2, 4, 8, "u")];
        // A click on line 0's prose, at the link's own columns, hits no region.
        assert_eq!(
            link_at(inner.x + 4, inner.y, inner, 0, &prefix, &lines, &regions),
            LinkProbe::NoLink,
            "a region belongs to ONE content line, not to those columns on every line"
        );
        // A click on the blank content line 1 hits no region.
        assert_eq!(
            link_at(
                inner.x + 4,
                inner.y + 1,
                inner,
                0,
                &prefix,
                &lines,
                &regions
            ),
            LinkProbe::NoLink
        );
        // A click left of the inner rect is rejected outright.
        assert_eq!(
            link_at(0, inner.y + 2, inner, 0, &prefix, &lines, &regions),
            LinkProbe::NoLink
        );
        // A click below the content (inside the pane, past the last line) is no link.
        assert_eq!(
            link_at(
                inner.x + 4,
                inner.y + 5,
                inner,
                0,
                &prefix,
                &lines,
                &regions
            ),
            LinkProbe::NoLink
        );
    }

    #[test]
    fn link_at_is_none_left_or_right_of_the_inner_rect() {
        let inner = inner_rect();
        // Three unwrapped content lines; a link FLUSH LEFT on line 2. Only a region
        // starting at content column 0 can catch a missing COLUMN guard: an indented
        // one (as `link_at_is_none_on_blank_rows_and_outside_the_pane` uses) passes
        // whether the guard is there or not, because column 0 is outside it anyway.
        let flush_lines = vec![
            Line::from(String::new()),
            Line::from(String::new()),
            link_line(12, 0, 8),
        ];
        let prefix = wrapped_row_prefix(&flush_lines, inner.width);
        let flush = [region(2, 0, 8, "u")];
        assert_eq!(
            link_at(
                inner.x,
                inner.y + 2,
                inner,
                0,
                &prefix,
                &flush_lines,
                &flush
            ),
            LinkProbe::Hit("u"),
            "the pane's own first column still hits a flush-left link"
        );
        // One cell LEFT of the pane is the border column, which `App::preview_rect`
        // — the OUTER rect the mouse arm gates on — still contains, so this is a
        // reachable click rather than a hypothetical: the guard here is the only
        // thing that stops it aliasing onto content column 0.
        assert_eq!(
            link_at(
                inner.x - 1,
                inner.y + 2,
                inner,
                0,
                &prefix,
                &flush_lines,
                &flush
            ),
            LinkProbe::NoLink,
            "the border column left of the pane is outside the transcript"
        );
        // The other side: ONE content line whose link spans 46 display columns, so at
        // this 20-wide pane it wraps over 3 visual rows. A click one cell past the
        // pane's last column lands inside that range unless the guard refuses first.
        let wrapped_lines = vec![link_line(46, 0, 46)];
        let wrapped = wrapped_row_prefix(&wrapped_lines, inner.width);
        let wide = [region(0, 0, 46, "w")];
        assert_eq!(
            link_at(
                inner.x + inner.width - 1,
                inner.y,
                inner,
                0,
                &wrapped,
                &wrapped_lines,
                &wide
            ),
            LinkProbe::Hit("w"),
            "the pane's last column is still inside"
        );
        assert_eq!(
            link_at(
                inner.x + inner.width,
                inner.y,
                inner,
                0,
                &wrapped,
                &wrapped_lines,
                &wide
            ),
            LinkProbe::NoLink,
            "one cell past the pane's last column is outside the transcript"
        );
    }

    #[test]
    fn link_at_hits_a_soft_wrapped_link_on_its_second_visual_row() {
        let inner = inner_rect();
        // One unbreakable content line of 45 columns in a 20-wide pane, so the
        // wrapper breaks at exactly 20 and 40. A link at content columns 25..30 is
        // therefore painted on the SECOND wrapped row, at its columns 5..10.
        let lines = vec![link_line(45, 25, 30)];
        let prefix = wrapped_row_prefix(&lines, inner.width);
        assert_eq!(
            prefix,
            vec![0, 3],
            "the fixture must occupy three visual rows"
        );
        let regions = [region(0, 25, 30, "w")];
        assert_eq!(
            link_at(
                inner.x + 7,
                inner.y + 1,
                inner,
                0,
                &prefix,
                &lines,
                &regions
            ),
            LinkProbe::Hit("w"),
            "a wrapped link is clickable on its second visual segment"
        );
        // The SAME column on the first visual row is content col 7 -> no link.
        assert_eq!(
            link_at(inner.x + 7, inner.y, inner, 0, &prefix, &lines, &regions),
            LinkProbe::NoLink
        );
    }

    #[test]
    fn link_at_respects_the_scroll_offset() {
        let inner = inner_rect();
        // Five unwrapped lines; a link on line 3 spanning columns 0..3. The OTHER
        // lines carry text at those same columns on purpose: the "without the scroll"
        // assertion below lands on line 1, and only a line with something painted
        // there can tell a working offset from one that resolved the wrong line.
        let lines = vec![
            Line::from(".".repeat(12)),
            Line::from(".".repeat(12)),
            Line::from(".".repeat(12)),
            link_line(12, 0, 3),
            Line::from(".".repeat(12)),
        ];
        let prefix = wrapped_row_prefix(&lines, inner.width);
        assert_eq!(prefix, vec![0, 1, 2, 3, 4, 5], "each line must fit one row");
        let regions = [region(3, 0, 3, "s")];
        // Scrolled down 2 rows, screen row rel 1 => visual row 3 => content line 3.
        assert_eq!(
            link_at(
                inner.x + 1,
                inner.y + 1,
                inner,
                2,
                &prefix,
                &lines,
                &regions
            ),
            LinkProbe::Hit("s")
        );
        // Without the scroll, the same screen cell is content line 1 -> no link.
        assert_eq!(
            link_at(
                inner.x + 1,
                inner.y + 1,
                inner,
                0,
                &prefix,
                &lines,
                &regions
            ),
            LinkProbe::NoLink
        );
    }

    // --- the measured wrapped-link shape ----------------------------------

    /// The pane width the reported miss was measured at.
    const HIT_PANE_WIDTH: u16 = 80;
    /// Characters in the measured url. Longer than the pane, so it cannot help but
    /// straddle a wrap.
    const HIT_URL_LEN: usize = 92;
    /// Display columns in the measured logical line, url and prose together.
    const HIT_LINE_COLS: usize = 305;
    /// The prose the measured line opens with, which is also the url's start column.
    const HIT_HEAD: &str = "I have ";

    /// The measured line as the preview renders it: plain prose, the url in
    /// [`preview::link_style`] (the style `store::preview` gives a link label), then
    /// plain prose out to [`HIT_LINE_COLS`] columns. Returns the line and the url it
    /// carries.
    fn hit_shape_line() -> (Line<'static>, String) {
        let url = format!("https://example.com/{}", "a".repeat(HIT_URL_LEN - 20));
        assert_eq!(
            url.len(),
            HIT_URL_LEN,
            "the fixture url must be the measured length"
        );
        let mut tail = String::from(" ");
        let tail_len = HIT_LINE_COLS - HIT_HEAD.len() - HIT_URL_LEN;
        while tail.len() < tail_len {
            tail.push_str("the quick brown fox jumps over the lazy dog ");
        }
        tail.truncate(tail_len);
        let line = Line::from(vec![
            Span::raw(HIT_HEAD),
            Span::styled(url.clone(), preview::link_style()),
            Span::raw(tail),
        ]);
        assert_eq!(
            line.width(),
            HIT_LINE_COLS,
            "the fixture must be the measured width"
        );
        (line, url)
    }

    /// Every `(x, y)` cell the renderer paints `modifier` into for `lines` at
    /// `width` — the label's REAL drawn extent, read off a rendered buffer rather
    /// than computed from the geometry under test.
    fn cells_with(
        lines: &[Line<'static>],
        width: u16,
        rows: u16,
        modifier: Modifier,
    ) -> Vec<(u16, u16)> {
        let area = Rect {
            x: 0,
            y: 0,
            width,
            height: rows,
        };
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::Widget::render(
            Paragraph::new(Text::from(lines.to_vec())).wrap(Wrap { trim: false }),
            area,
            &mut buffer,
        );
        (0..rows)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                buffer
                    .cell((x, y))
                    .is_some_and(|c| c.modifier.contains(modifier))
            })
            .collect()
    }

    #[test]
    fn link_at_hits_a_wrapped_url_across_its_whole_drawn_extent() {
        let (line, url) = hit_shape_line();
        let lines = vec![line];
        let prefix = wrapped_row_prefix(&lines, HIT_PANE_WIDTH);
        let rows = u16::try_from(*prefix.last().expect("a non-empty map")).expect("a short line");
        let regions = [region(
            0,
            HIT_HEAD.len(),
            HIT_HEAD.len() + HIT_URL_LEN,
            &url,
        )];
        let inner = Rect {
            x: 1,
            y: 1,
            width: HIT_PANE_WIDTH,
            height: rows,
        };

        // The url's REAL drawn extent: the preview underlines a link label, so the
        // underlined cells are exactly the cells a user can see it in. Read off a
        // rendered buffer, never computed from the arithmetic under test.
        let drawn = cells_with(&lines, HIT_PANE_WIDTH, rows, Modifier::UNDERLINED);
        assert_eq!(
            drawn.len(),
            HIT_URL_LEN,
            "every one of the url's columns must reach a cell"
        );
        let drawn_rows: HashSet<u16> = drawn.iter().map(|&(_, y)| y).collect();
        assert!(
            drawn_rows.len() > 1,
            "the url must span more than one visual row, or its CONTINUATION row — \
             the row the packed column step killed outright — is not under test"
        );
        assert!(
            !drawn_rows.contains(&0),
            "the wrapper must have pushed the whole url off row 0, or the fixture is \
             not the shape that was measured"
        );

        for &(x, y) in &drawn {
            assert_eq!(
                link_at(
                    inner.x + x,
                    inner.y + y,
                    inner,
                    0,
                    &prefix,
                    &lines,
                    &regions
                ),
                LinkProbe::Hit(url.as_str()),
                "a click on drawn cell ({x}, {y}) must open the url it was painted for"
            );
        }

        // And the cells immediately outside that extent must stay dead: the prose
        // before the url on row 0, and the first cell after its last drawn one.
        let (last_x, last_y) = *drawn.last().expect("a drawn url");
        assert_eq!(
            link_at(
                inner.x + last_x + 1,
                inner.y + last_y,
                inner,
                0,
                &prefix,
                &lines,
                &regions
            ),
            LinkProbe::NoLink,
            "the cell just past the url's last drawn one is prose, not the link"
        );
        assert_eq!(
            link_at(inner.x, inner.y, inner, 0, &prefix, &lines, &regions),
            LinkProbe::NoLink,
            "the prose the line opens with is not the link"
        );
    }

    /// Widths the marking sweep below measures every case at. 40 because the widest
    /// case is 40 columns of text, so the sweep runs from a one-column pane (where
    /// every glyph breaks) right through to one that fits the whole line — covering
    /// every break position a lost column could possibly move.
    const MARK_SWEEP_WIDTH: u16 = 40;

    /// The equality the probe's soundness rests on: re-styling a region cannot move
    /// the wrap, so the marked line measures to exactly the rows the plain one does.
    ///
    /// The failure mode it guards is a WIDTH that moves when a line is re-split into
    /// more spans, which only a contextual, multi-column glyph can expose — hence the
    /// CJK and VS16 cases beside the measured ASCII one. Each is swept across every
    /// width from 1 to [`MARK_SWEEP_WIDTH`] rather than asserted at one hand-picked
    /// number: a width where a lost column happens not to move a break would let a
    /// broken splitter through, and a sweep has no such gap.
    #[test]
    fn marking_a_link_region_does_not_move_the_wrap() {
        let (hit_line, _) = hit_shape_line();
        let cjk = Line::from(vec![
            Span::raw("see "),
            Span::styled(
                "日本語のドキュメント".to_string(),
                Style::default().add_modifier(Modifier::UNDERLINED),
            ),
            Span::raw(" for the rest of it"),
        ]);
        // A VS16 emoji is the hard case: the selector adds a column to the glyph it
        // follows, so severing it from its base measures ONE column narrower than the
        // same bytes unsplit. This region STOPS INSIDE that cluster — a boundary the
        // renderer's own columns never produce, but exactly where a char-indexed
        // marker would cut, so the snap-out has something to prove.
        let emoji = Line::from(vec![
            Span::raw("ok ☺\u{fe0f}"),
            Span::styled(
                "☺\u{fe0f}link".to_string(),
                Style::default().add_modifier(Modifier::UNDERLINED),
            ),
            Span::raw(" go on then"),
        ]);
        for (name, line, col_start, col_end) in [
            (
                "the measured url",
                hit_line,
                HIT_HEAD.len(),
                HIT_HEAD.len() + HIT_URL_LEN,
            ),
            ("a CJK label", cjk, 4, 24),
            ("a VS16 emoji cut mid-cluster", emoji, 5, 6),
        ] {
            let marked = highlight_matched_spans(
                &line,
                &region_char_positions(&line, col_start, col_end),
                LINK_PROBE_MARKER,
            );
            assert!(
                marked
                    .spans
                    .iter()
                    .any(|s| s.style.add_modifier.contains(LINK_PROBE_MARKER)),
                "{name}: the region must really have been marked, or the equality \
                 below is asserting nothing"
            );
            for width in 1..=MARK_SWEEP_WIDTH {
                assert_eq!(
                    wrapped_text_rows(std::slice::from_ref(&marked), width),
                    wrapped_text_rows(std::slice::from_ref(&line), width),
                    "{name}: marking a region must not change how tall the line \
                     wraps, and it did at width {width}"
                );
            }
        }
    }

    /// A double-width label is clickable across BOTH columns of every glyph.
    ///
    /// ratatui paints a wide glyph by styling its LEFT cell alone and leaving the
    /// right one untouched, so a probe that only ever read the clicked cell would
    /// answer "no link" on half of a CJK label's visible width.
    #[test]
    fn link_at_hits_both_halves_of_a_double_width_label() {
        // "see " (4 cols) then a 5-glyph, 10-column label, then prose.
        let label = "日本語文書";
        let lines = vec![Line::from(vec![
            Span::raw("see "),
            Span::styled(
                label.to_string(),
                Style::default().add_modifier(Modifier::UNDERLINED),
            ),
            Span::raw(" ok"),
        ])];
        let inner = Rect {
            x: 1,
            y: 1,
            width: 40,
            height: 4,
        };
        let prefix = wrapped_row_prefix(&lines, inner.width);
        let regions = [region(0, 4, 4 + 10, "cjk")];
        for x in 4..14u16 {
            assert_eq!(
                link_at(inner.x + x, inner.y, inner, 0, &prefix, &lines, &regions),
                LinkProbe::Hit("cjk"),
                "column {x} is inside the label's drawn width and must open its url"
            );
        }
        for x in [3u16, 14] {
            assert_eq!(
                link_at(inner.x + x, inner.y, inner, 0, &prefix, &lines, &regions),
                LinkProbe::NoLink,
                "column {x} is outside the label"
            );
        }
    }

    /// The blank column immediately RIGHT of a link that ENDS a painted row is NOT
    /// part of that link.
    ///
    /// `Paragraph` only `set_style`s its area — it never blanks a row's remainder — so
    /// every column past a row's last painted glyph is left UNWRITTEN, which by symbol
    /// alone is indistinguishable from the right half of a double-width glyph. Admit
    /// both and a click on empty space opens the link beside it: the one failure this
    /// hit-test exists to prevent, since an abstention costs a click while a wrong hit
    /// hands an unintended url to a browser.
    ///
    /// The fixture deliberately does NOT use [`link_line`], whose dot padding would put
    /// a real, unmarked cell at that column and arrange the ambiguity away entirely.
    /// That the column really is unwritten is therefore ASSERTED, off a render of the
    /// same line, before anything is concluded from it.
    #[test]
    fn link_at_refuses_the_blank_column_right_of_a_link_that_ends_a_row() {
        const HEAD: &str = "see ";
        const LABEL: &str = "docs";
        let start = HEAD.len();
        let end = start + LABEL.len();
        // The label ENDS the line: the row's last painted column is the link's last.
        let lines = vec![Line::from(vec![
            Span::raw(HEAD),
            Span::styled(
                LABEL.to_string(),
                Style::default().add_modifier(Modifier::UNDERLINED),
            ),
        ])];
        let inner = inner_rect();
        let blank = u16::try_from(end).expect("a short fixture");
        assert!(
            blank < inner.width,
            "the pane must be wider than the line, or there is no column to its right"
        );

        // The premise, read off a render rather than assumed: the column just past the
        // label was never written, so it is exactly the ambiguous cell under test.
        let area = Rect {
            x: 0,
            y: 0,
            width: inner.width,
            height: 1,
        };
        let mut painted = Buffer::filled(area, Cell::new(LINK_PROBE_UNWRITTEN));
        Paragraph::new(Text::from(lines.clone()))
            .wrap(Wrap { trim: false })
            .render(area, &mut painted);
        assert_eq!(
            painted.cell((blank - 1, 0)).map(|c| c.symbol()),
            Some("s"),
            "the label's last column must really be the row's last painted one"
        );
        assert_eq!(
            painted.cell((blank, 0)).map(|c| c.symbol()),
            Some(LINK_PROBE_UNWRITTEN),
            "the column right of the label must be UNWRITTEN, or this fixture cannot \
             pose the ambiguity it exists to pose"
        );

        let prefix = wrapped_row_prefix(&lines, inner.width);
        let regions = [region(0, start, end, "u")];
        assert_eq!(
            link_at(
                inner.x + blank - 1,
                inner.y,
                inner,
                0,
                &prefix,
                &lines,
                &regions
            ),
            LinkProbe::Hit("u"),
            "the label's own last cell still opens its url"
        );
        assert_eq!(
            link_at(
                inner.x + blank,
                inner.y,
                inner,
                0,
                &prefix,
                &lines,
                &regions
            ),
            LinkProbe::NoLink,
            "an unwritten cell whose left neighbour is a NARROW glyph is blank space, \
             not that glyph's second half — clicking it must open nothing"
        );
        assert_eq!(
            link_at(
                inner.x + blank + 1,
                inner.y,
                inner,
                0,
                &prefix,
                &lines,
                &regions
            ),
            LinkProbe::NoLink,
            "and deeper into the unpainted tail stays dead too"
        );
    }

    /// The probe REFUSES rather than guesses when the row count it is handed does not
    /// match what the marked line measures to.
    ///
    /// That disagreement means the probe render is describing a layout the pane never
    /// painted, and the safe direction is unambiguous here: a hit-test that guesses
    /// hands an unintended url to a browser, while one that abstains costs a click.
    #[test]
    fn a_region_probe_refuses_a_row_count_it_cannot_reproduce() {
        let (line, _) = hit_shape_line();
        let real_rows = wrapped_text_rows(std::slice::from_ref(&line), HIT_PANE_WIDTH);
        let (start, end) = (HIT_HEAD.len(), HIT_HEAD.len() + HIT_URL_LEN);
        assert!(
            region_paints_cell(&line, start, end, HIT_PANE_WIDTH, real_rows, 1, 0),
            "the true row count must resolve the cell, or the refusal below proves \
             nothing"
        );
        assert!(
            !region_paints_cell(&line, start, end, HIT_PANE_WIDTH, real_rows + 1, 1, 0),
            "a row count the marked line cannot reproduce must resolve to no link"
        );
    }

    // --- fold hit-test: the SAME geometry as `link_at`, a different region list --

    fn fold_region(content_row: usize, col_start: usize, col_end: usize, key: &str) -> FoldRegion {
        FoldRegion {
            content_row,
            col_start,
            col_end,
            key: key.to_string(),
        }
    }

    #[test]
    fn fold_at_returns_the_key_inside_a_node_header_and_none_just_outside() {
        let inner = inner_rect();
        // Three unwrapped content lines; a peer node's header on line 2 claiming its
        // whole 18-column display width, which is how `peer_node_lines` emits it.
        let prefix = [0usize, 1, 2, 3];
        let regions = [fold_region(2, 0, 18, "a03505fe4b1c2d3e0")];
        // The header's FIRST cell and its LAST both toggle the node: the whole line
        // is the click target, not just the affordance text at its right.
        assert_eq!(
            fold_at(inner.x, inner.y + 2, inner, 0, &prefix, &regions),
            Some("a03505fe4b1c2d3e0")
        );
        assert_eq!(
            fold_at(inner.x + 17, inner.y + 2, inner, 0, &prefix, &regions),
            Some("a03505fe4b1c2d3e0")
        );
        // One cell past the header's end (col_end is exclusive) -> None, so the pane
        // to the right of a short header stays inert.
        assert_eq!(
            fold_at(inner.x + 18, inner.y + 2, inner, 0, &prefix, &regions),
            None
        );
        // A region that does NOT start at column 0 is refused one cell before it, so
        // the inclusive lower edge is pinned rather than assumed from `col_start` 0.
        let indented = [fold_region(2, 4, 8, "k")];
        assert_eq!(
            fold_at(inner.x + 4, inner.y + 2, inner, 0, &prefix, &indented),
            Some("k")
        );
        assert_eq!(
            fold_at(inner.x + 3, inner.y + 2, inner, 0, &prefix, &indented),
            None
        );
    }

    #[test]
    fn fold_at_is_none_on_blank_rows_and_outside_the_pane() {
        let inner = inner_rect();
        let prefix = [0usize, 1, 2, 3];
        // A node's block is [blank, header, body...], so content line 1 here is the
        // blank separator sitting directly ABOVE the header: the row an off-by-one
        // would hand the toggle.
        let regions = [fold_region(2, 0, 18, "k")];
        assert_eq!(
            fold_at(inner.x + 4, inner.y + 1, inner, 0, &prefix, &regions),
            None,
            "the blank above a node header claims no click"
        );
        // A click left of the inner rect is rejected outright, even though the region
        // starts at content column 0.
        assert_eq!(fold_at(0, inner.y + 2, inner, 0, &prefix, &regions), None);
        // A click below the content (inside the pane, past the last line) -> None.
        assert_eq!(
            fold_at(inner.x + 4, inner.y + 5, inner, 0, &prefix, &regions),
            None
        );
    }

    #[test]
    fn fold_at_hits_a_soft_wrapped_node_header_on_its_later_visual_rows() {
        let inner = inner_rect();
        // ONE content line occupying 3 visual rows at inner width 20: a long header
        // (`* message from @a03505fe4b1c2d3e0 * 14:15 * (click to expand)`) at a
        // narrow pane. The region spans its whole 46-column display width.
        let prefix = [0usize, 3];
        let regions = [fold_region(0, 0, 46, "w")];
        // Every wrapped segment toggles the node — first row, second, and third.
        assert_eq!(
            fold_at(inner.x + 5, inner.y, inner, 0, &prefix, &regions),
            Some("w")
        );
        assert_eq!(
            fold_at(inner.x + 5, inner.y + 1, inner, 0, &prefix, &regions),
            Some("w"),
            "a wrapped header is clickable on its second visual segment"
        );
        assert_eq!(
            fold_at(inner.x + 5, inner.y + 2, inner, 0, &prefix, &regions),
            Some("w")
        );
        // And the tail of the LAST segment stops at the header's end: column 6 of the
        // third row is content column 46, one past it. This is what dies if the
        // `sub_row * inner.width` term is ever dropped — the same cell would then
        // resolve to content column 6 and hit.
        assert_eq!(
            fold_at(inner.x + 6, inner.y + 2, inner, 0, &prefix, &regions),
            None,
            "past the header's display width, even on a continuation row"
        );
    }

    #[test]
    fn fold_at_respects_the_scroll_offset() {
        let inner = inner_rect();
        // Five unwrapped lines; a node header on line 3 spanning columns 0..12.
        let prefix = [0usize, 1, 2, 3, 4, 5];
        let regions = [fold_region(3, 0, 12, "s")];
        // Scrolled down 2 rows, screen row rel 1 => visual row 3 => content line 3.
        assert_eq!(
            fold_at(inner.x + 1, inner.y + 1, inner, 2, &prefix, &regions),
            Some("s")
        );
        // Without the scroll, the same screen cell is content line 1 -> no node.
        assert_eq!(
            fold_at(inner.x + 1, inner.y + 1, inner, 0, &prefix, &regions),
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

    // --- the interrupt confirmation overlay -------------------------------
    //
    // The phrasings no drawn text may contain are `crate::send::OWNERSHIP_CLAIMS`:
    // one list, shared with the refusal-copy test in `send`, whose doc comment
    // carries the reason.

    /// Every drawn row of a full board frame, borders included.
    fn drawn_frame(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let buffer = drawn_buffer(app, width, height);
        (0..height)
            .map(|y| full_row_text(&buffer, y, width))
            .collect()
    }

    /// A full board frame's cells, drawn with whatever overlay `app` has open.
    fn drawn_buffer(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height))
            .expect("build an in-memory test terminal");
        terminal
            .draw(|frame| render(frame, app))
            .expect("the board must draw with the confirm open");
        terminal.backend().buffer().clone()
    }

    /// The text drawn INSIDE the bordered box whose top border carries `title`: one
    /// string per content row, borders excluded. Panics if no such box is drawn.
    ///
    /// Found by CONTENT (the title, then the box's own corners) rather than by
    /// recomputing the renderer's `centered_rect`. A copied size can drift from the
    /// renderer's, and a drifted rect lands on BOARD cells, where the session's label
    /// is drawn too, so an assertion over them would pass with the box saying nothing.
    fn boxed_rows(buffer: &ratatui::buffer::Buffer, title: &str) -> Vec<String> {
        let (width, height) = (buffer.area.width, buffer.area.height);
        let symbol = |x: u16, y: u16| buffer.cell((x, y)).map_or("", |cell| cell.symbol());
        let title_len = u16::try_from(title.chars().count()).expect("a short title");
        let spells_title = |x: u16, y: u16| {
            (x..x.saturating_add(title_len))
                .map(|cx| symbol(cx, y))
                .collect::<String>()
                == title
        };
        let (left, top) = (0..height)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .find(|&(x, y)| symbol(x, y) == "┌" && spells_title(x + 1, y))
            .unwrap_or_else(|| panic!("no box titled {title:?} is drawn"));
        let right = (left + 1..width)
            .find(|&x| symbol(x, top) == "┐")
            .expect("the box's top-right corner");
        let bottom = (top + 1..height)
            .find(|&y| symbol(left, y) == "└")
            .expect("the box's bottom-left corner");
        (top + 1..bottom)
            .map(|y| (left + 1..right).map(|x| symbol(x, y)).collect())
            .collect()
    }

    /// A board with the interrupt confirm open on `route`.
    fn confirm_app(route: InterruptRoute) -> App {
        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        app.open_interrupt_confirm("sess-normal-1".to_string(), route);
        app
    }

    /// The pid variant must draw the pid, the session, the verb, and the truncated-
    /// transcript warning — and must assert NOTHING about who owns the process.
    ///
    /// The presence half is read from INSIDE the confirm's own box. The board row
    /// under it draws the session's label too, so a whole-frame search for it passes
    /// with the confirm not naming the session at all (observed: it stayed green with
    /// the label dropped from the confirm's text).
    ///
    /// The absence half is the point of the test, and it is checked against every row
    /// of the frame rather than against the lines this function happens to build, so a
    /// later edit cannot sneak an ownership claim in through the title, the footer, or
    /// a helper.
    #[test]
    fn the_signal_confirm_names_the_pid_and_claims_no_owner() {
        const PID: u32 = 29628;
        let (width, height) = (100, 30);
        let mut app = confirm_app(InterruptRoute::Signal { pid: PID });
        let buffer = drawn_buffer(&mut app, width, height);
        let screen = (0..height)
            .map(|y| full_row_text(&buffer, y, width))
            .collect::<Vec<_>>()
            .join("\n");
        let confirm = boxed_rows(&buffer, " signal this process? ").join("\n");

        for required in [
            "29628",             // the pid, so the user can see WHAT is being signalled
            "SIGTERM",           // the verb, named rather than implied
            "sess-normal-1",     // which session it belongs to
            "no attachable job", // the observation that put us on this route
            "mid-line",          // the truncated final JSONL line, named up front
            "fail-soft",         // ...and that it is survivable
            "Esc",               // the way out
        ] {
            assert!(
                confirm.contains(required),
                "the signal confirm must draw {required:?} INSIDE its box:\n{confirm}"
            );
        }

        for claim in OWNERSHIP_CLAIMS {
            assert!(
                !screen.to_lowercase().contains(claim),
                "no drawn text may claim {claim:?}:\n{screen}"
            );
        }
        // "kill" would overclaim in the other direction: SIGTERM is a request, and
        // nothing here waits to see it honoured.
        assert!(
            !screen.contains("Kill") && !screen.contains("kill"),
            "the copy must not promise a kill:\n{screen}"
        );
    }

    /// The job-id variant keeps its own wording, unchanged: one function draws both
    /// shapes, so the signal copy must not have leaked into the delegated route.
    #[test]
    fn the_job_confirm_still_draws_the_delegated_stop_wording() {
        let mut app = confirm_app(InterruptRoute::Job {
            job_id: "job-k".to_string(),
        });
        let rows = drawn_frame(&mut app, 100, 30);
        let screen = rows.join("\n");

        assert!(
            screen.contains("running as an agent") && screen.contains("conversation is kept"),
            "the job route keeps its own copy:\n{screen}"
        );
        for leaked in ["SIGTERM", "pid", "mid-line"] {
            assert!(
                !screen.contains(leaked),
                "the signal copy must not reach the job route ({leaked:?}):\n{screen}"
            );
        }
        for claim in OWNERSHIP_CLAIMS {
            assert!(!screen.to_lowercase().contains(claim));
        }
    }

    // --- the List modal's scroll window -----------------------------------

    /// `len` one-line choices: the shape every `List` modal had before a
    /// description could wrap, so the window cases written for it still ask the
    /// questions they always asked.
    fn one_line_rows(len: usize) -> Vec<usize> {
        vec![1; len]
    }

    /// No cap on the choice count, so a case is about the line viewport alone.
    const UNCAPPED: usize = usize::MAX;

    /// The whole window rule, stated as arithmetic: the offset is KEPT while the
    /// selection is inside it and moved by the least amount that brings it back
    /// otherwise. Every case is one the modal can actually reach — `cycle_modal`
    /// steps by one and WRAPS, so both ends are ordinary keystrokes, not edge cases.
    #[test]
    fn the_list_window_keeps_its_offset_until_the_selection_leaves_it() {
        // A list that FITS never scrolls, whatever the caller asks for: there is
        // nothing off-window, so an inherited offset must be discarded rather than
        // blanking rows the box has room for.
        assert_eq!(modal_list_window(&one_line_rows(3), 2, 10, UNCAPPED, 0), 0);
        assert_eq!(modal_list_window(&one_line_rows(3), 0, 10, UNCAPPED, 7), 0);
        assert_eq!(modal_list_window(&one_line_rows(10), 9, 10, UNCAPPED, 4), 0);

        // Selection INSIDE the current window: the offset is untouched, which is
        // what stops the list re-centring on every keypress.
        assert_eq!(modal_list_window(&one_line_rows(20), 5, 5, UNCAPPED, 3), 3);
        assert_eq!(modal_list_window(&one_line_rows(20), 7, 5, UNCAPPED, 3), 3);

        // Selection ABOVE the window: scroll up exactly onto it.
        assert_eq!(modal_list_window(&one_line_rows(20), 2, 5, UNCAPPED, 6), 2);

        // Selection BELOW the window: scroll down the least that shows it, which
        // puts it on the LAST visible row (`selected + 1 - viewport`).
        assert_eq!(modal_list_window(&one_line_rows(20), 9, 5, UNCAPPED, 3), 5);

        // Both ends, reached the way the wrap actually reaches them.
        assert_eq!(
            modal_list_window(&one_line_rows(20), 0, 5, UNCAPPED, 12),
            0,
            "top pins to zero"
        );
        assert_eq!(
            modal_list_window(&one_line_rows(20), 19, 5, UNCAPPED, 0),
            15,
            "bottom pins to max_scroll = len - viewport"
        );

        // An offset past the end (a shrunken list) is clamped, never trusted.
        assert_eq!(
            modal_list_window(&one_line_rows(20), 19, 5, UNCAPPED, 99),
            15
        );
    }

    /// The degenerate viewports a freely-resized terminal produces. A short enough
    /// box leaves ZERO rows for the list, and the arithmetic must answer rather than
    /// underflow — `selected + 1 - viewport` is the subtraction that would.
    #[test]
    fn the_list_window_survives_a_viewport_of_zero_or_one() {
        // Zero rows: nothing is drawable, so the answer is 0 and no subtraction
        // happens at all.
        assert_eq!(modal_list_window(&one_line_rows(20), 19, 0, UNCAPPED, 7), 0);
        assert_eq!(modal_list_window(&one_line_rows(0), 0, 0, UNCAPPED, 0), 0);

        // One row: the window IS the selection, from either direction.
        assert_eq!(modal_list_window(&one_line_rows(20), 0, 1, UNCAPPED, 9), 0);
        assert_eq!(modal_list_window(&one_line_rows(20), 9, 1, UNCAPPED, 0), 9);
        assert_eq!(
            modal_list_window(&one_line_rows(20), 19, 1, UNCAPPED, 19),
            19
        );

        // An empty list cannot select anything; the window is still 0.
        assert_eq!(modal_list_window(&one_line_rows(0), 0, 5, UNCAPPED, 3), 0);

        // A `selected` past the end (defensive — `cycle_modal` keeps it in range)
        // is bounded by `max_scroll` instead of running off it.
        assert_eq!(modal_list_window(&one_line_rows(4), 99, 2, UNCAPPED, 0), 2);
    }

    /// A wrapped row counts EVERY line it draws. The selection is inside the
    /// window only when it is drawn whole. Each case here has a different answer
    /// if the two-line row is counted as one line.
    #[test]
    fn the_list_window_counts_every_line_a_wrapped_row_draws() {
        /// The model picker's shape: a two-line `default` row, then one-line
        /// aliases.
        const HEIGHTS: [usize; 6] = [2, 1, 1, 1, 1, 1];
        /// Four lines of list: the wrapped row plus two aliases fill it.
        const VIEWPORT: usize = 4;

        let window =
            |selected, current| modal_list_window(&HEIGHTS, selected, VIEWPORT, UNCAPPED, current);
        // Rows 0..=2 take exactly four lines, so the top window holds them.
        assert_eq!(window(0, 0), 0);
        assert_eq!(window(2, 0), 0);
        // Row 3 is the fifth line. The least move drops the whole two-line row, so
        // the window starts one ROW down. Counting row 0 as one line keeps it at 0.
        assert_eq!(window(3, 0), 1);
        // The bottom pins where the last four one-line rows fill the window.
        assert_eq!(window(5, 0), 2);
        // Wrapping back to the top scrolls onto the whole wrapped row.
        assert_eq!(window(0, 2), 0);

        // A THREE-line row: it and one alias fill the window, and the next alias
        // scrolls it away as a unit.
        let tall = [3, 1, 1, 1];
        assert_eq!(modal_list_window(&tall, 1, VIEWPORT, UNCAPPED, 0), 0);
        assert_eq!(modal_list_window(&tall, 2, VIEWPORT, UNCAPPED, 0), 1);
    }

    /// A selected row TALLER than the whole viewport, which a short terminal
    /// produces, is shown from its top: marker and label. It is never scrolled
    /// past, so it stays reachable.
    #[test]
    fn a_row_taller_than_the_viewport_is_shown_from_its_top() {
        let heights = [3, 1, 1];
        assert_eq!(modal_list_window(&heights, 0, 2, UNCAPPED, 0), 0);
        assert_eq!(modal_list_window(&heights, 0, 2, UNCAPPED, 1), 0);
        assert_eq!(modal_list_window(&heights, 1, 2, UNCAPPED, 0), 1);
        // Its first line lands in the viewport, so it counts as shown. The one line
        // of room goes to it and the rows below are the `N more`.
        assert_eq!(modal_list_shown(&heights, 0, 1, UNCAPPED), 1);
        assert_eq!(modal_list_shown(&heights, 0, 2, UNCAPPED), 1);
    }

    /// The choice cap bounds the window as well as the lines. That is what gives
    /// a picker longer than [`MODAL_LIST_MAX_ROWS`] the same `N more` count at both
    /// ends, even though one end has a wrapped row and the other does not.
    #[test]
    fn the_choice_cap_bounds_the_window_at_both_ends_of_a_wrapped_list() {
        /// A cap-sized window, as `render_modal` passes it.
        const CAP: usize = 12;
        // A wrapped `default` row plus twenty one-line aliases.
        let mut heights = vec![2];
        heights.extend(one_line_rows(20));
        let asked = modal_list_lines_asked(&heights, CAP);
        assert_eq!(
            asked, 13,
            "the tallest twelve-choice run: the wrapped row + 11"
        );

        // At the top: twelve choices in thirteen lines, nine below.
        assert_eq!(modal_list_window(&heights, 0, asked, CAP, 0), 0);
        assert_eq!(modal_list_shown(&heights, 0, asked, CAP), 12);
        // At the bottom: twelve one-line choices. Without the cap, thirteen would
        // fit the thirteen lines and only eight would be above.
        let bottom = modal_list_window(&heights, 20, asked, CAP, 0);
        assert_eq!(bottom, 9);
        assert_eq!(modal_list_shown(&heights, bottom, asked, CAP), 12);
    }

    /// Which choices get their first line drawn, counting each one's height.
    #[test]
    fn the_shown_count_follows_row_heights_and_the_cap() {
        let heights = [2, 1, 1];
        assert_eq!(
            modal_list_shown(&heights, 0, 0, UNCAPPED),
            0,
            "no room, no rows"
        );
        assert_eq!(modal_list_shown(&heights, 0, 3, UNCAPPED), 2);
        assert_eq!(modal_list_shown(&heights, 0, 4, UNCAPPED), 3);
        assert_eq!(modal_list_shown(&heights, 0, 10, UNCAPPED), 3);
        assert_eq!(modal_list_shown(&heights, 0, 10, 2), 2, "the cap stops it");
        assert_eq!(modal_list_shown(&heights, 1, 10, UNCAPPED), 2);
        assert_eq!(
            modal_list_shown(&heights, 9, 10, UNCAPPED),
            0,
            "past the end"
        );
        // One-line rows: the old `min(len - scroll, viewport)`, unchanged.
        assert_eq!(modal_list_shown(&one_line_rows(20), 15, 5, UNCAPPED), 5);
        assert_eq!(modal_list_shown(&one_line_rows(20), 17, 5, UNCAPPED), 3);
    }

    /// What the box asks for: every line when the list is within the cap, the
    /// tallest cap-sized run otherwise.
    #[test]
    fn the_asked_lines_are_the_tallest_cap_sized_run() {
        assert_eq!(modal_list_lines_asked(&[2, 1, 1, 1], 12), 5);
        assert_eq!(modal_list_lines_asked(&[3, 1, 1, 1, 1, 1], 12), 8);
        assert_eq!(modal_list_lines_asked(&one_line_rows(20), 12), 12);
        // The tallest run is found wherever it sits, not only at the top.
        assert_eq!(modal_list_lines_asked(&[1, 1, 3, 1], 2), 4);
        assert_eq!(modal_list_lines_asked(&[], 12), 0);
        assert_eq!(modal_list_lines_asked(&[2, 1], 0), 0);
    }

    /// A wrapping choice's continuation lines are DIM and sit at the hanging
    /// indent. The selection does not change the row's height. A choice that does
    /// not opt in stays one line, however long its description.
    #[test]
    fn modal_list_row_lines_wraps_only_an_opted_in_description() {
        // A real default-row explanation: a reply claude will not restore for.
        let choice = |wrap| ModalChoice {
            label: "default".to_string(),
            description: Some(
                ComposeDefault::RestoreOverridden
                    .picker_description()
                    .to_string(),
            ),
            wrap_description: wrap,
            action: ModalAction::Cancel,
        };
        let text =
            |line: &Line| -> String { line.spans.iter().map(|s| s.content.as_ref()).collect() };

        // The room right of `› default  ` is 49 columns, and `wrap_message` fills
        // each line greedily by whole words: 34 columns (the next word, 25 wide,
        // does not fit beside it), then 48, then the rest.
        let wrapped = modal_list_row_lines(&choice(true), true);
        assert_eq!(
            wrapped.len(),
            3,
            "the explanation wraps onto two more lines"
        );
        assert_eq!(
            text(&wrapped[0]),
            "\u{203a} default  no --model \u{b7} ANTHROPIC_MODEL or an"
        );
        let indent = " ".repeat(usize::from(modal_list_description_column("default")));
        assert_eq!(
            text(&wrapped[1]),
            format!("{indent}ANTHROPIC_DEFAULT_*_MODEL is set, so claude uses")
        );
        assert_eq!(
            text(&wrapped[2]),
            format!("{indent}its startup model, not the session's")
        );
        assert!(
            wrapped
                .iter()
                .all(|line| text(line).chars().count() <= usize::from(MODAL_INNER_WIDTH)),
            "every line fits inside the borders"
        );
        for line in &wrapped[1..] {
            let continuation = line
                .spans
                .iter()
                .find(|s| !s.content.trim().is_empty())
                .expect("the continuation has text");
            assert!(
                continuation.style.add_modifier.contains(Modifier::DIM),
                "every continuation is dim, like the description it continues"
            );
        }
        assert_eq!(
            modal_list_row_lines(&choice(true), false).len(),
            wrapped.len(),
            "moving the selection off the row does not change its height"
        );

        assert_eq!(
            modal_list_row_lines(&choice(false), true).len(),
            1,
            "a choice that does not opt in stays one line"
        );
    }

    /// The overflow marker is drawn ONLY when something is actually off-window, and
    /// carries the count rather than a bare arrow — "there is more" is far less
    /// useful than "there are seven more" when deciding whether to keep pressing.
    #[test]
    fn the_more_marker_appears_only_when_rows_are_off_window() {
        let blank = modal_more_line(MODAL_MORE_ABOVE, 0);
        assert_eq!(
            blank
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>(),
            "",
            "nothing hidden means the spacer stays a spacer"
        );

        let more = modal_more_line(MODAL_MORE_BELOW, 7);
        let text = more
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(text.contains('7'), "the marker names the count: {text:?}");
        assert!(
            text.contains(MODAL_MORE_BELOW),
            "and the direction to press: {text:?}"
        );
        assert!(
            more.spans
                .iter()
                .all(|s| s.style.add_modifier.contains(Modifier::DIM)),
            "the marker is chrome, not a choice"
        );
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
            background: false,
            has_agent_name: false,
            has_agent_setting: false,
            failed_task: None,
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
        app.set_pane_layout(DOCK_LAYOUT);
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

    /// A DOCKED compose-zone board: 60 columns drawn at the 1:1 [`PaneLayout::Even`]
    /// split (a 28-column list, so a 32-column preview pane), and tall enough to
    /// clear the dock threshold. Both halves and the layout are fixed rather than
    /// defaulted because the tests below need to know where the editor actually
    /// wraps.
    const DOCK_BOARD: (u16, u16) = (60, 30);
    /// The layout [`DOCK_BOARD`] is drawn with. Set explicitly, so a change to the
    /// board's starting layout cannot quietly move the editor these cases measure.
    const DOCK_LAYOUT: PaneLayout = PaneLayout::Even;
    /// The editor's REAL inner width on [`DOCK_BOARD`]: the 32-column preview pane,
    /// less the pane's own border (2), less the compose block's border (2). Spelled
    /// out because those four columns are exactly what the docked path used to lose
    /// track of — and PINNED to the drawn box by [`drawn_editor_width`] rather than
    /// trusted, so a layout change that moved the editor fails the cases below instead
    /// of leaving their premise reasoning about a width the editor no longer has.
    const DOCK_EDITOR_WIDTH: u16 = 28;

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
        app.set_pane_layout(DOCK_LAYOUT);
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
        app.set_pane_layout(PaneLayout::Even);
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
            app.set_pane_layout(PaneLayout::Even);
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
                pid: None,
                started_at_ms: None,
            },
        );
        app.set_reported_agents(reported, None);
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
            "Enter send · ^L model · ^J/Alt+Enter newline · paste keeps newlines · Esc cancel",
        );
        assert!(
            !reply_hint.contains("Ctrl-O"),
            "the reply hints must not grow a key the reply target ignores: {reply_hint}"
        );

        // The background hint names both verbs, honestly — and its model key.
        let bg_hint = compose_hint(&ComposeState::new_background(None).target);
        assert!(bg_hint.contains("Enter start in background"), "{bg_hint}");
        assert!(bg_hint.contains("Ctrl-O run interactively"), "{bg_hint}");
        assert!(bg_hint.contains("Ctrl-L model"), "{bg_hint}");
        for dishonest in ["review", "edit", "before sending", "prefill", "pre-fill"] {
            assert!(
                !bg_hint.to_lowercase().contains(dishonest),
                "the prompt AUTO-SUBMITS, so the hint must not imply {dishonest:?}: {bg_hint}"
            );
        }
    }

    /// The reply hint's column budget: it is ONE help-row line that is truncated,
    /// never wrapped, so it must fit an 80-column terminal WHOLE — `Esc cancel` last
    /// — measured with the `unicode-width` the renderer counts in. The model key was
    /// paid for inside that budget (see [`compose_hint`]), and it is inside the
    /// drawn columns of the BACKGROUND hint too, which runs past 80 on purpose.
    #[test]
    fn the_reply_hint_fits_an_eighty_column_terminal() {
        use unicode_width::UnicodeWidthStr;

        /// The narrowest terminal the help row is budgeted for.
        const HELP_ROW_BUDGET: usize = 80;

        let reply = compose_hint(&ComposeTarget::Reply {
            session_id: "s".to_string(),
            stop_job: None,
        });
        assert!(
            reply.width() <= HELP_ROW_BUDGET,
            "the reply hint must fit {HELP_ROW_BUDGET} columns: {} in {reply:?}",
            reply.width()
        );
        let model_key_end = BG_DRAFT_HINT
            .find("Ctrl-L model")
            .map(|at| BG_DRAFT_HINT[..at + "Ctrl-L model".len()].width())
            .expect("the background hint names its model key");
        assert!(
            model_key_end <= HELP_ROW_BUDGET,
            "the background hint's model key must sit inside the drawn columns: ends \
             at {model_key_end}"
        );
    }

    /// While a send is in flight for the selected session the pinned banner is
    /// SUPPRESSED (so it cannot desync the hit-test) and the send renders INLINE at
    /// the transcript tail: the echoed message under a `▶ you` turn plus a single
    /// `● claude` **cooking…** placeholder. The placeholder no longer depends on the
    /// agents poll; it reads `cooking…` before and after claude reports working.
    /// The `▶ you` echo drops the instant the real turn lands on disk; when the send
    /// finishes the banner is no longer suppressed and the pinned row returns to
    /// its normal content — the marker of the turn under the top of the viewport —
    /// with the agent status reaching it only as the fallback for a transcript with
    /// no marker at all.
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

        // Nothing in flight -> no inline tail, and the pinned row IS reserved even
        // though claude reports no agent for this session: the reservation keys on
        // the selection, so the suppression below is suppressing a real row.
        assert!(
            preview_banner(&app).is_some(),
            "an unreported selected session still reserves its pinned row"
        );
        assert!(sending_tail(&app, 80).is_none());

        // In flight, nothing on disk yet (msg_count still the baseline) -> the
        // pinned banner is suppressed and the tail echoes the message + "cooking…".
        app.sending = vec![Sending {
            session_id: "sess-normal-1".to_string(),
            message: "please summarize this".to_string(),
            baseline_msg_count: 0,
        }];
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
                pid: None,
                started_at_ms: None,
            },
        );
        app.set_reported_agents(reported, None);
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

        // Send done -> no inline tail, and the banner is no longer suppressed. What
        // `render_preview` finally draws in that row is the marker of the turn under
        // the top of the viewport; the agent status returned here reaches the screen
        // only as the fallback for a transcript with no marker at all.
        app.sending.clear();
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
        app.sending = vec![Sending {
            session_id: "sess-normal-1".to_string(),
            message: "please summarize this".to_string(),
            baseline_msg_count: 0,
        }];

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
        // this fixture's specific turn count. The scrollbar spans the
        // TRANSCRIPT's rows, beneath the pinned banner row.
        let transcript = transcript_rect(&app, width, height);
        let inner_height = transcript.height;
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
        // (`↓`), which itself sits on the transcript's last row, just above the
        // block's bottom border.
        let begin_row = transcript.y;
        let end_row = transcript.bottom() - 1;
        let last_track_row = end_row - 1;
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

        // The track spans the TRANSCRIPT's rows, beneath the pinned banner row.
        let transcript = transcript_rect(&app, width, height);
        let thumb_col = width - 1;
        let begin_row = transcript.y;
        let end_row = transcript.bottom() - 1;
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
        // 6-row track (a transcript of 8 rows under the pinned banner row, minus
        // the 2 reserved arrow rows) — the huge content_h/track_length ratio that
        // exposed the old rounding bug (a tiny real scroll rounding straight back
        // onto an edge track row).
        let width = 3u16;
        let height = 11u16;
        let mut terminal = Terminal::new(TestBackend::new(width, height))
            .expect("build an in-memory test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_preview(frame, &mut app, area);
            })
            .expect("render_preview must not panic on a narrow, tall viewport");

        // The track spans the TRANSCRIPT's rows, beneath the pinned banner row.
        let transcript = transcript_rect(&app, width, height);
        let inner_height = transcript.height;
        let content_h = content_height(&mut app, width);
        let max_offset = (content_h - usize::from(inner_height)) as u32;
        assert!(
            app.preview_scroll > 0 && app.preview_scroll < max_offset,
            "the requested offset (5) must remain a genuine INTERIOR scroll for \
             this geometry (offset={}, max_offset={max_offset})",
            app.preview_scroll
        );

        let thumb_col = width - 1;
        let begin_row = transcript.y;
        let end_row = transcript.bottom() - 1;
        let first_track_row = begin_row + 1;
        let last_track_row = end_row - 1;
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

        app.sending = vec![Sending {
            session_id: "sess-normal-1".to_string(),
            message: "ping".to_string(),
            baseline_msg_count: 0,
        }];
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
        let dir = unique_temp_dir("window-parity");
        let mut app = window_app(&dir);
        let inner = (width - 2, transcript_rect(&app, width, height).height);

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
            let drawn = transcript_rows(&mut app, width, height);
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
        let dir = unique_temp_dir("window-residual");
        let mut app = window_app(&dir);
        let inner = (width - 2, transcript_rect(&app, width, height).height);

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
        let drawn = transcript_rows(&mut app, width, height);

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

        let window = app.preview_window(inner_w, geometry.rows_above, geometry.inner_h);
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
        let dir = unique_temp_dir("window-scrollbar");
        let mut app = window_app(&dir);
        // The scrollbar spans the TRANSCRIPT's rows, beneath the pinned banner row.
        let transcript = transcript_rect(&app, width, height);
        let inner_h = transcript.height;
        let end_row = transcript.bottom() - 1;

        let content_h = content_height(&mut app, width);
        let max_offset = content_h - usize::from(inner_h);
        assert!(
            max_offset > usize::from(inner_h),
            "the transcript must be several viewports long, or the thumb's travel \
             says nothing (max_offset={max_offset})"
        );

        // Track rows, excluding the two reserved boundary-arrow slots.
        let track = transcript.y + 1..end_row;
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
            middle.cell((width - 1, end_row)).map(|c| c.symbol()),
            Some(SCROLLBAR_ARROW_HIDDEN),
            "the end arrow belongs to the transcript's bottom, not the window's"
        );

        // And the transcript's real bottom — an offset the window itself cannot tell
        // apart from the mid-scroll one, since both hand the widget one viewport.
        app.preview_scroll = u32::try_from(max_offset).expect("a small test offset");
        let bottom = preview_buffer(&mut app, width, height);
        assert_eq!(
            bottom.cell((width - 1, end_row)).map(|c| c.symbol()),
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
            background: false,
            has_agent_name: false,
            has_agent_setting: false,
            failed_task: None,
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
        // The TRANSCRIPT's rect — beneath the pinned banner row — which is what the
        // click hit-test resolves against.
        let inner = transcript_rect(&app, width, height);
        let offset = usize::try_from(app.preview_scroll).expect("a small test offset");
        assert!(
            app.preview_window(inner_w, offset, inner.height).start > 0,
            "the drawn window must really start past line 0, or an absolute offset \
             and a window-relative one are indistinguishable"
        );

        let (col, row) = (inner.y..inner.bottom())
            .flat_map(|y| (inner.x..inner.right()).map(move |x| (x, y)))
            .find(|&(x, y)| {
                buffer
                    .cell((x, y))
                    .is_some_and(|c| c.modifier.contains(Modifier::UNDERLINED))
            })
            .expect("the fixture's link label must be drawn inside the pane");

        let scroll = app.preview_scroll;
        let (row_prefix, lines, regions, _folds) = app
            .preview_hit_context(inner_w)
            .expect("a selected preview");
        assert_eq!(
            link_at(col, row, inner, scroll, row_prefix, lines, regions),
            LinkProbe::Hit(WINDOW_LINK_URL),
            "a click on the cell the label was DRAWN on must open its url"
        );
        assert_eq!(
            link_at(col, row - 1, inner, scroll, row_prefix, lines, regions),
            LinkProbe::NoLink,
            "and the row above it is another transcript line, not the link"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A preview link is DRAWN light blue, italic and underlined — its label's cells
    /// and no others.
    ///
    /// Read off the rendered pane rather than off the parser's spans (PATTERNS —
    /// assert drawn cells): a style the parser set but the render lost is not a link
    /// anyone can see. Both directions are pinned. Every `LightBlue` cell spells the
    /// label, so the color reaches neither the prose around it nor the pane's chrome
    /// (nothing else the preview pane draws is `LightBlue` — the list's search-match
    /// highlight is, but it is not in this pane — which is what makes the color a
    /// sound locator); and every one of them is italic and underlined, the half of the
    /// look that does not depend on how the terminal's theme draws the blue.
    #[test]
    fn a_preview_link_is_drawn_light_blue_italic_and_underlined() {
        let (width, height) = WINDOW_PANE;
        let dir = unique_temp_dir("link-color");
        let mut app = App::new(
            vec![window_link_session(&dir)],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );

        // The default bottom anchor scrolls the pane to the tail, where the link is.
        let buffer = preview_buffer(&mut app, width, height);
        let blue: Vec<&Cell> = (0..height)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .filter_map(|(x, y)| buffer.cell((x, y)))
            .filter(|c| c.fg == Color::LightBlue)
            .collect();
        let blue_text: String = blue.iter().map(|c| c.symbol()).collect();
        assert_eq!(
            blue_text, WINDOW_LINK_LABEL,
            "exactly the link's label must be drawn LightBlue"
        );
        assert!(
            blue.iter().all(|c| c.modifier.contains(Modifier::ITALIC)),
            "every LightBlue label cell must also be italic"
        );
        assert!(
            blue.iter()
                .all(|c| c.modifier.contains(Modifier::UNDERLINED)),
            "every LightBlue label cell must also be underlined"
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
            background: false,
            has_agent_name: false,
            has_agent_setting: false,
            failed_task: None,
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

        // The TRANSCRIPT's rect — beneath the pinned banner row — which is what the
        // click hit-test resolves against.
        let inner = transcript_rect(&app, width, height);
        let (col, row) = (inner.y..inner.bottom())
            .flat_map(|y| (inner.x..inner.right()).map(move |x| (x, y)))
            .find(|&(x, y)| {
                buffer
                    .cell((x, y))
                    .is_some_and(|c| c.modifier.contains(Modifier::UNDERLINED))
            })
            .expect("the fixture's link label must be drawn inside the pane");

        let scroll = app.preview_scroll;
        let (row_prefix, hit_lines, regions, _folds) = app
            .preview_hit_context(inner_w)
            .expect("a selected preview");
        assert_eq!(
            link_at(col, row, inner, scroll, row_prefix, hit_lines, regions),
            LinkProbe::Hit(WRAP_LINK_URL),
            "a click on the cell the label was DRAWN on must open its url, however \
             many wrapped lines sit above it"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The url behind the GFM TABLE fixture below, and the label it renders as.
    const TABLE_LINK_URL: &str = "https://example.com/table-cell";
    const TABLE_LINK_LABEL: &str = "spec";

    /// The column rule a GRID-mode table draws between its columns.
    ///
    /// Its presence on the clicked row is what tells the grid layout from the stacked
    /// RECORD fallback, which records no clickable region at all — so without it this
    /// test could go green over a table that was never the shape under test.
    const GRID_COLUMN_RULE: &str = "\u{2502}";

    /// A transcript whose one turn is a GFM TABLE carrying a link in a body cell.
    ///
    /// Written as MARKDOWN SOURCE and rendered by the production pass, never as
    /// hand-built [`LinkRegion`]s: the columns a grid cell's region is recorded in are
    /// produced by `store::preview`'s own table layout, so regions stated by hand would
    /// test the probe against the test's arithmetic instead of against the render — and
    /// reproduce exactly the blindness this case exists to remove.
    /// Headers wide enough that the table's NATURAL width overflows [`WINDOW_PANE`],
    /// so the grid is clamped to fill the pane's inner width EXACTLY.
    ///
    /// That is what makes this fixture able to catch a probe measuring at a width the
    /// grid was not laid out at. A short table fits any nearby width unchanged and
    /// wraps at none of them, so it stays green against exactly the divergence this
    /// case exists to detect — the shape of a fixture that arranges the failure away.
    const TABLE_HEADERS: &str = "| Document reference | Notes about the document |";

    fn table_link_session(dir: &Path) -> Session {
        let file = dir.join("sess-table-link.jsonl");
        let body = format!(
            "{TABLE_HEADERS}\\n| --- | --- |\\n| [{TABLE_LINK_LABEL}]({TABLE_LINK_URL}) | ok |"
        );
        let jsonl = format!(
            concat!(
                r#"{{"type":"user","sessionId":"sess-table-link","cwd":"/tmp","#,
                r#""timestamp":"2026-07-01T10:00:00.000Z","#,
                r#""message":{{"role":"user","content":"{body}"}}}}"#,
                "\n",
            ),
            body = body,
        );
        std::fs::write(&file, jsonl).expect("write the table link fixture");
        Session {
            file,
            session_id: "sess-table-link".to_string(),
            cwd: PathBuf::from("/tmp"),
            git_branch: Some("main".to_string()),
            timestamp: None,
            repo: "repo".to_string(),
            label: "table link session".to_string(),
            root_uuid: None,
            msg_count: 0,
            content_index: String::new(),
            background: false,
            has_agent_name: false,
            has_agent_setting: false,
            failed_task: None,
        }
    }

    /// A link inside a GFM GRID table is clickable through the REAL click path.
    ///
    /// Table links were verified at the `store::preview` layer alone, which proves the
    /// regions are RECORDED but not that anything can ever HIT one. Nothing drove a
    /// grid table through `link_at`, so the whole capability could have been inert —
    /// the grid render width diverging from the pane's `inner.width` by a single column
    /// makes every table region fail the probe's row-count check and abstain, silently,
    /// with the suite still green. This closes that: markdown source in, production
    /// render out, and the click aimed at the cells the label was actually PAINTED in
    /// (found by the `UNDERLINED` modifier the preview marks a label with, never
    /// computed from the geometry under test).
    #[test]
    fn a_click_opens_a_link_inside_a_gfm_grid_table() {
        let (width, height) = WINDOW_PANE;
        let inner_w = width - 2;
        let dir = unique_temp_dir("table-link");
        let mut app = App::new(
            vec![table_link_session(&dir)],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );

        let buffer = preview_buffer(&mut app, width, height);
        // The TRANSCRIPT's rect — beneath the pinned banner row — which is what the
        // click hit-test resolves against.
        let inner = transcript_rect(&app, width, height);

        // The cells the label was drawn in, read off the render.
        let drawn: Vec<(u16, u16)> = (inner.y..inner.bottom())
            .flat_map(|y| (inner.x..inner.right()).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                buffer
                    .cell((x, y))
                    .is_some_and(|c| c.modifier.contains(Modifier::UNDERLINED))
            })
            .collect();
        let drawn_text: String = drawn
            .iter()
            .filter_map(|&(x, y)| buffer.cell((x, y)).map(|c| c.symbol().to_string()))
            .collect();
        assert_eq!(
            drawn_text, TABLE_LINK_LABEL,
            "the table cell's link label must be what is painted underlined in the \
             pane, or this is aiming at some other span"
        );

        // It must really be a GRID: the record fallback stacks the cells instead and
        // records no region at all, so a green run there would mean nothing.
        let row = drawn[0].1;
        let rule_col = (inner.x..inner.right())
            .find(|&x| {
                buffer
                    .cell((x, row))
                    .is_some_and(|c| c.symbol() == GRID_COLUMN_RULE)
            })
            .expect("the label's row must draw a grid column rule at this pane width");

        let scroll = app.preview_scroll;
        let (row_prefix, lines, regions, _folds) = app
            .preview_hit_context(inner_w)
            .expect("a selected preview");
        assert_eq!(
            regions.len(),
            1,
            "the production render must have recorded the table cell's one region"
        );
        // The premise that makes the probe's width answerable at all: the clamped grid
        // fills the pane's inner width EXACTLY, so the layout the region's columns were
        // recorded in is the layout the probe re-renders. A grid laid out at some other
        // width would put the label's columns somewhere the probe never paints, and
        // every table region would abstain.
        assert_eq!(
            lines[regions[0].content_row].width(),
            usize::from(inner_w),
            "the grid must be clamped to fill the pane's inner width exactly, or the \
             region's columns and the probe's render describe different layouts"
        );
        for &(x, y) in &drawn {
            assert_eq!(
                link_at(x, y, inner, scroll, row_prefix, lines, regions),
                LinkProbe::Hit(TABLE_LINK_URL),
                "a click on drawn cell ({x}, {y}) of a table-cell link must open its url"
            );
        }
        assert_eq!(
            link_at(rule_col, row, inner, scroll, row_prefix, lines, regions),
            LinkProbe::NoLink,
            "the column rule between the cells belongs to no link"
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
            background: false,
            has_agent_name: false,
            has_agent_setting: false,
            failed_task: None,
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
        /// The TRANSCRIPT's height — the viewport the jump is resolved against,
        /// beneath any pinned banner row.
        inner_h: u16,
        /// The screen row the transcript's first row is painted on.
        top: u16,
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

    /// Measure the pane the jump is about to be asserted against, in the state it
    /// will be drawn in — so call it AFTER anything that moves the banner (an
    /// in-flight reply gives the pinned row back to the transcript).
    fn jump_geometry(app: &mut App, (width, height): (u16, u16)) -> JumpGeometry {
        let inner_w = width - 2;
        let transcript = transcript_rect(app, width, height);
        let inner_h = transcript.height;
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
            top: transcript.y,
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
        let row = row_text(&drawn, geo.top + geo.lead as u16, width);
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
    /// The 1:0 [`PaneLayout::ListOnly`] layout takes the pane away without clearing
    /// anything behind it, so a query typed while it is gone still arms the one-shot
    /// and `render_preview` — its only consumer — never runs. Left armed, it fires on
    /// the frame the pane comes BACK on: the user brings the preview back expecting
    /// the newest turn (what leaving 1:0 promises) and lands on a match from a query
    /// they have since moved on from.
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
        app.set_pane_layout(PaneLayout::ListOnly);
        assert!(
            !app.pane_layout().shows_preview(),
            "the 1:0 layout takes the pane away"
        );

        // Searching with no pane on screen: the board still draws, and that frame is
        // where the request has to die.
        let mut terminal =
            Terminal::new(TestBackend::new(80, 24)).expect("build an in-memory test terminal");
        app.push_query_str(JUMP_QUERY);
        terminal
            .draw(|frame| render(frame, &mut app))
            .expect("the board must draw with no preview pane");

        // And the pane comes back where leaving 1:0 puts it.
        app.step_pane_layout(false);
        preview_buffer(&mut app, width, height);
        assert_eq!(
            app.preview_scroll, bottom_offset,
            "a re-opened pane must show the newest turn, not act on a request no \
             frame could see"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- pane layout: the five stops, the 0:1 title, the reading position -----

    /// A board wide enough that every split stop's share of it is a whole number of
    /// columns, and short enough that the anchor fixture below overflows the pane.
    const LAYOUT_BOARD: (u16, u16) = (100, 20);

    /// How many turns [`numbered_session_at`] writes: enough to overflow
    /// [`LAYOUT_BOARD`]'s preview several times over at EITHER width the reading-
    /// position test draws, so neither offset is decided by the bottom clamp.
    const ANCHOR_TURNS: usize = 16;

    /// The turn the reading-position test parks at the top of the pane — far from
    /// both ends, so it is the reader's position and not a clamp's.
    const ANCHOR_TURN: usize = 4;

    /// The marker opening turn `turn`'s body, so a drawn row names the turn it
    /// belongs to. The generic jump fixture repeats ONE body for every miss turn,
    /// which cannot tell "the same line came back" from "an identical line did".
    fn turn_marker(turn: usize) -> String {
        format!("turn{turn:02}-marker")
    }

    /// The text after each turn's marker: ~120 columns of SHORT words, so it wraps
    /// close to evenly — three rows in the 1:1 pane of [`LAYOUT_BOARD`] and two in
    /// the wider 1:3 one. That one-row difference per turn is what makes a carried
    /// row offset land on a different line after the step.
    const ANCHOR_BODY: &str = "alpha bravo charlie delta echo foxtrot golf hotel india \
         juliet kilo lima mike november oscar papa quebec romeo sierra tango";

    /// A transcript whose every turn is IDENTIFIABLE on screen: [`turn_marker`]
    /// followed by [`ANCHOR_BODY`], which wraps to a different number of rows at
    /// the two widths the anchor test draws.
    fn numbered_session_at(dir: &Path, id: &str, turns: usize) -> Session {
        let path = dir.join(format!("{id}.jsonl"));
        let mut out =
            String::from(r#"{"type":"summary","summary":"Layout anchor","leafUuid":"a1"}"#);
        out.push('\n');
        for turn in 0..turns {
            let role = if turn % 2 == 0 { "user" } else { "assistant" };
            let body = format!("{} {ANCHOR_BODY}", turn_marker(turn));
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
            label: "Layout anchor".to_string(),
            root_uuid: None,
            msg_count: turns,
            content_index: String::new(),
            background: false,
            has_agent_name: false,
            has_agent_setting: false,
            failed_task: None,
        }
    }

    /// The preview's FIRST transcript row exactly as drawn, read off the buffer: the
    /// top row of the transcript rect `render_preview` drew into, between the pane's
    /// side borders.
    ///
    /// Derived through the SAME `preview_split` the view draws with, asked the same
    /// `preview_banner(..).is_some()` question, because every selected session pins
    /// a row above its transcript — the turn it is reading — so the pane's first
    /// inner row is that pinned row, never a transcript line.
    fn preview_top_row(buffer: &ratatui::buffer::Buffer, app: &App) -> String {
        let (_, transcript) = preview_split(app.preview_rect, preview_banner(app).is_some());
        let y = transcript.y;
        (transcript.x..transcript.right())
            .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol().to_string()))
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    /// Each stop divides the body as its name says, on a drawn board: the three
    /// splits at 25 / 48 / 75 percent of the width with the preview starting where
    /// the list ends, and 1:0 giving the list the whole body and the preview an
    /// EMPTY rect that no hit-test can match.
    #[test]
    fn each_pane_layout_divides_the_body_as_its_stop_says() {
        let (width, height) = LAYOUT_BOARD;
        for (layout, list_cols) in [
            (PaneLayout::PreviewWide, 25),
            (PaneLayout::Even, 48),
            (PaneLayout::ListWide, 75),
        ] {
            let mut app = App::new(
                vec![markable_session()],
                Scope::All,
                PathBuf::from("/tmp/launch"),
            );
            app.set_pane_layout(layout);
            let _ = drawn_board(&mut app, width, height);
            assert_eq!(
                app.list_rect.width, list_cols,
                "{layout:?}: the list's share"
            );
            assert_eq!(
                app.preview_rect.x, list_cols,
                "{layout:?}: the preview starts where the list ends"
            );
            assert_eq!(
                app.preview_rect.width,
                width - list_cols,
                "{layout:?}: and takes the rest"
            );
        }

        let mut app = App::new(
            vec![markable_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        app.set_pane_layout(PaneLayout::ListOnly);
        let _ = drawn_board(&mut app, width, height);
        assert_eq!(
            app.list_rect.width, width,
            "1:0 gives the list the whole body"
        );
        assert!(
            app.preview_rect.is_empty(),
            "and the preview an empty rect, so no hit-test can land on it"
        );
    }

    /// 0:1 gives the list an EMPTY area: no list block is drawn anywhere, its rect
    /// is empty so a wheel or click can never be routed to it, and the preview owns
    /// the body edge to edge — titled with the selected session's name, since no
    /// list is left on screen to say which row it is.
    #[test]
    fn preview_only_gives_the_list_an_empty_area_and_the_preview_the_whole_body() {
        let (width, height) = LAYOUT_BOARD;
        let list_title = "┌ sessions ";

        // The control: at the default 1:1 the list block IS drawn, so the absence
        // asserted below is a real absence and not a title this board never shows.
        let mut app = App::new(
            vec![markable_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        let even = buffer_rows(&drawn_board(&mut app, width, height), width, height);
        assert!(
            even.iter().any(|row| row.contains(list_title)),
            "premise: the 1:1 board draws the list block"
        );

        app.set_pane_layout(PaneLayout::PreviewOnly);
        let rows = buffer_rows(&drawn_board(&mut app, width, height), width, height);
        assert!(
            app.list_rect.is_empty(),
            "the list's area is empty: {:?}",
            app.list_rect
        );
        assert_eq!(
            (app.preview_rect.x, app.preview_rect.width),
            (0, width),
            "the preview owns the whole body"
        );
        assert!(
            !rows.iter().any(|row| row.contains(list_title)),
            "no list block is drawn at 0:1; board:\n{}",
            rows.join("\n")
        );
        let top_border = &rows[usize::from(app.preview_rect.y)];
        assert!(
            top_border.starts_with("┌ Fix the payment webhook retries "),
            "the preview's title names the selected session: {top_border:?}"
        );
    }

    /// The preview's title is the generic one wherever a list still shows which row
    /// is selected, and the selected session's name only at 0:1 — falling back to
    /// the id for an empty label, and to the generic title with nothing selected or
    /// with a draft card standing in for a session that does not exist yet.
    #[test]
    fn the_preview_title_names_the_selected_session_only_when_the_list_is_hidden() {
        use crate::tui::compose::ComposeState;

        let mut app = App::new(
            vec![markable_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        for layout in [
            PaneLayout::PreviewWide,
            PaneLayout::Even,
            PaneLayout::ListWide,
        ] {
            app.set_pane_layout(layout);
            assert_eq!(preview_title(&app), PREVIEW_TITLE, "{layout:?}");
        }

        app.set_pane_layout(PaneLayout::PreviewOnly);
        assert_eq!(preview_title(&app), " Fix the payment webhook retries ");

        app.sessions[0].label.clear();
        assert_eq!(
            preview_title(&app),
            " sess-normal-1 ",
            "an empty label falls back to the id"
        );

        app.open_compose(
            ComposeState::new_background(None),
            Some(NewSessionDraft::default()),
        );
        assert_eq!(
            preview_title(&app),
            PREVIEW_TITLE,
            "a draft card is not the selected session's transcript"
        );

        let mut empty = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp/launch"));
        empty.set_pane_layout(PaneLayout::PreviewOnly);
        assert_eq!(preview_title(&empty), PREVIEW_TITLE, "nothing selected");
    }

    /// A layout step keeps the reader's place: the line at the top of the pane
    /// before the step is the line at the top after it, at the new width.
    ///
    /// The premise is what gives this teeth. `preview_scroll` counts WRAPPED rows,
    /// and the fixture's turns wrap to fewer rows in the wider pane, so the raw row
    /// offset carried across the step would put a LATER line at the top — the
    /// assertion below that it does not is what fails if the anchor is ever lost.
    /// The line is identified by what was DRAWN (its turn's marker), never by the
    /// offset arithmetic under test.
    #[test]
    fn a_layout_step_keeps_the_top_visible_line_at_the_top() {
        let dir = unique_temp_dir("layout-anchor");
        let (width, height) = LAYOUT_BOARD;
        let marker = turn_marker(ANCHOR_TURN);
        let mut app = App::new(
            vec![numbered_session_at(&dir, "sess-anchor-1", ANCHOR_TURNS)],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );

        // The reader parks the anchor turn's line at the top of the 1:1 pane.
        let _ = drawn_board(&mut app, width, height);
        let old_inner = app.preview_rect.width - 2;
        let target = app
            .preview_text(old_inner)
            .lines
            .iter()
            .position(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
                    .contains(&marker)
            })
            .expect("the anchor turn's body is a rendered line");
        app.preview_top();
        let old_offset = app
            .preview_rows_above(old_inner, target)
            .expect("the target line is in the transcript");
        app.preview_scroll = u32::try_from(old_offset).expect("a small fixture");
        let before = drawn_board(&mut app, width, height);
        assert!(
            preview_top_row(&before, &app).contains(&marker),
            "premise: the reader's line is at the top before the step"
        );

        // `Shift-←`: 1:1 -> 1:3, a wider preview.
        app.step_pane_layout(false);
        assert_eq!(app.pane_layout(), PaneLayout::PreviewWide);
        let after = drawn_board(&mut app, width, height);
        let new_inner = app.preview_rect.width - 2;
        assert!(new_inner > old_inner, "premise: the step widened the pane");
        let new_prefix = app
            .preview_hit_context(new_inner)
            .expect("the anchor session is selected, so it has a preview")
            .0
            .to_vec();
        assert_ne!(
            line_at_row(&new_prefix, old_offset),
            target,
            "premise: the old ROW offset names a different line at the new width, \
             so only the anchor can bring this one back"
        );

        let top = preview_top_row(&after, &app);
        assert!(
            top.contains(&marker),
            "the line the reader had at the top must still be at the top after the \
             step; drawn top row: {top:?}"
        );
        assert_eq!(
            usize::try_from(app.preview_scroll).ok(),
            new_prefix.get(target).copied(),
            "and the pane is scrolled to exactly where that line starts now"
        );
        assert!(
            !app.preview_follow_bottom,
            "a restored position is still the reader's, not a subscription"
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
                geo.top + geo.lead as u16,
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
        app.sending = vec![super::super::app::Sending {
            session_id: id.to_string(),
            message: "any update on the rollout?".to_string(),
            baseline_msg_count: JUMP_TURNS,
        }];
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
        // The send goes in FIRST: an in-flight reply gives the pinned row back to the
        // transcript, and the geometry must describe the pane as it is drawn.
        let tail_rows = send_in_flight(&mut app, "sess-jump-1", width - 2);
        let geo = jump_geometry(&mut app, JUMP_PANE);

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
            row_text(&first, geo.top + geo.lead as u16, width),
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
            row_text(&second, geo.top + geo.lead as u16, width),
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
    /// row, bringing the pane back from the 1:0 layout) takes it back. A scroll states
    /// a POSITION, never a
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
        // The send goes in FIRST: an in-flight reply gives the pinned row back to the
        // transcript, and the geometry must describe the pane as it is drawn.
        let tail_rows = send_in_flight(&mut app, "sess-jump-1", width - 2);
        let geo = jump_geometry(&mut app, JUMP_PANE);
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
        let top = row_text(&landed, geo.top, width);

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
            row_text(&after, geo.top, width),
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
        // The transcript's own top row, beneath the pinned banner row.
        let transcript = transcript_rect(&app, wide, height);
        let top = row_text(&held, transcript.y, wide);

        // The session gains turns — the only thing that can tell a pane parked on the
        // last row from one following the newest.
        const GROWN_TURNS: usize = JUMP_TURNS + 8;
        app.apply_sessions(vec![jump_session_of(&dir, "sess-jump-1", GROWN_TURNS)]);
        let following = content_height(&mut app, wide) - usize::from(transcript.height);
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
            row_text(&after, transcript.y, wide),
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
        // The transcript's own top row, beneath the pinned banner row.
        let transcript = transcript_rect(&app, width, height);
        let top = row_text(&held, transcript.y, width);

        // And the turns come back.
        app.apply_sessions(vec![jump_session_at(&dir, "sess-jump-1")]);
        let following = content_height(&mut app, width) - usize::from(transcript.height);
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
            row_text(&after, transcript.y, width),
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
        // One row taller than `CARD_PANE`, so the bottom-anchored transcript window
        // beneath the pinned banner row still reaches a line that says the query.
        let (width, height) = (CARD_PANE.0, CARD_PANE.1 + PREVIEW_BANNER_ROWS);
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
    /// banner has to survive (a session's transcript grows, and the preview
    /// bottom-anchors by default). Each test re-asserts the overflow rather than
    /// trusting this comment.
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
                    pid: None,
                    started_at_ms: None,
                },
            );
            app.set_reported_agents(reported, None);
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

    /// The transcript's rect inside a `(width, height)` preview pane drawn at the
    /// origin, derived exactly as `render_preview` and `update`'s click hit-test
    /// derive it — `preview_split` keyed on `preview_banner` — so a test states where
    /// the transcript REALLY sits instead of assuming it owns the whole inner rect:
    /// every selected session pins a banner row above it, and only a pane with no
    /// session transcript on it (a draft card, an in-flight reply) gives that back.
    fn transcript_rect(app: &App, width: u16, height: u16) -> Rect {
        let pane = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        preview_split(pane, preview_banner(app).is_some()).1
    }

    /// Render the preview and return only the TRANSCRIPT's drawn rows, top to
    /// bottom — [`inner_rows`] minus any pinned banner row above them.
    fn transcript_rows(app: &mut App, width: u16, height: u16) -> Vec<String> {
        // Rows between the pane's top border and the transcript's first row.
        let above = transcript_rect(app, width, height).y - 1;
        inner_rows(app, width, height).split_off(usize::from(above))
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
            rows[0], "\u{25cf} claude \u{b7} 10:00",
            "a reported session must LEAD with its pinned banner row (the turn \
             marker under the top of the viewport) in the default view"
        );
        assert!(
            reported.preview_scroll > 0,
            "the transcript beneath the banner must really be scrolled to the \
             newest turn, not parked at the top where a banner survives for free"
        );

        // The banner steals a row from the pane, not from the transcript's tail:
        // the newest line stays on the bottom row, so the transcript scrolls
        // BENEATH the banner rather than being pushed down by it. Read off the
        // same cache the pane draws from rather than hardcoded.
        let newest: String = reported
            .preview_text(width - 2)
            .lines
            .last()
            .expect("the fixture renders at least one line")
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            newest.chars().count() < usize::from(width - 2),
            "the newest line must fit one row, or the bottom-row check proves nothing"
        );
        assert_eq!(
            rows.last().map(String::as_str),
            Some(newest.trim_end()),
            "the newest transcript line must stay anchored to the pane's bottom row"
        );
        // The banner costs the transcript EXACTLY one row of viewport — no
        // silent gap under it, no second reserved row.
        assert_eq!(
            reported.preview_viewport_h,
            inner_height - PREVIEW_BANNER_ROWS,
            "the pinned banner must cost the transcript exactly one row"
        );
    }

    /// A session claude does NOT report pins the SAME row a reported one does: the
    /// row names the turn you are reading, which every transcript has, so the
    /// reservation keys on the selection and never on claude's agent list. The two
    /// panes must therefore be IDENTICAL, cell text for cell text — the same turn
    /// marker pinned above the same bottom-anchored transcript window.
    ///
    /// Left in the DEFAULT bottom-anchored state (never `preview_top()`), which is
    /// the state a user sees and the one a prepended line would scroll away in.
    #[test]
    fn an_unreported_session_pins_the_same_turn_marker_row_as_a_reported_one() {
        let (width, height) = BANNER_PANE;
        let mut unreported = banner_app(None);
        assert!(
            unreported.reported_agent("sess-normal-1").is_none(),
            "the session must really be unreported, or this is the reported case"
        );
        assert!(
            unreported.preview_follow_bottom,
            "the preview must be bottom-anchored by default, or this test proves nothing"
        );
        let inner_height = height - 2;
        let content_h = content_height(&mut unreported, width);
        assert!(
            content_h > usize::from(inner_height),
            "the fixture must overflow the pane, or this test proves nothing \
             (content_h={content_h}, inner_height={inner_height})"
        );

        let rows = inner_rows(&mut unreported, width, height);
        assert_eq!(
            rows[0], "\u{25cf} claude \u{b7} 10:00",
            "an unreported session must LEAD with the pinned turn marker under the \
             top of the viewport: {rows:?}"
        );
        assert!(
            unreported.preview_scroll > 0,
            "the transcript must really be scrolled to the newest turn, not parked \
             at the top where the marker would be row 0 for free"
        );
        assert_eq!(
            unreported.preview_viewport_h,
            inner_height - PREVIEW_BANNER_ROWS,
            "the pinned row must cost the unreported transcript exactly one row"
        );

        let mut reported = banner_app(Some("blocked"));
        assert_eq!(
            rows,
            inner_rows(&mut reported, width, height),
            "whether claude reports the session must not change the pane at all"
        );
    }

    /// An UNREPORTED session whose transcript has no marker to pin still reserves
    /// the row — the geometry the hit-test derives from `preview_banner` alone — and
    /// leaves it BLANK: there is no reported status to fall back to, and the pane
    /// must not claim that nothing is selected when something is.
    #[test]
    fn an_unreported_marker_less_transcript_reserves_a_blank_pinned_row() {
        let (width, height) = BANNER_PANE;
        let mut session = sample_session();
        session.file = empty_transcript_fixture();
        let mut app = App::new(vec![session], Scope::All, PathBuf::from("/tmp/launch"));
        assert!(
            app.preview_text(width - 2).lines.is_empty(),
            "the fixture must render an EMPTY transcript for this test to reach \
             the marker-less fallback"
        );
        assert!(app.reported_agent("sess-normal-1").is_none());

        let rows = inner_rows(&mut app, width, height);
        assert_eq!(
            app.preview_viewport_h,
            height - 2 - PREVIEW_BANNER_ROWS,
            "the row is reserved even with nothing to show in it"
        );
        assert_eq!(rows[0], "", "no marker and no reported status: a blank row");
        assert!(
            !rows.iter().any(|row| row.contains("No session selected.")),
            "a SELECTED session must never fall through to the nothing-selected \
             placeholder: {rows:?}"
        );
    }

    /// A FINISHED agent must keep its banner: the pane keys on the selection, never
    /// on live, so a `done` session still leads with its pinned banner row (the
    /// turn marker under the top of the viewport) rather than silently losing
    /// the row the moment the agent wraps up. `done` vs `blocked` only picks
    /// which bucket `ReportedAgent` falls in — neither the transcript nor the
    /// resolved offset depends on it — so this renders the exact same marker
    /// text as the bottom-anchored default proven above.
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
            rows[0], "\u{25cf} claude \u{b7} 10:00",
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
                pid: None,
                started_at_ms: None,
            },
        );
        app.set_reported_agents(reported, None);

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

    // --- failed background task: the banner ---------------------------------

    /// A failed background task as the store derives it, stamped at `unix_secs`
    /// (or undated).
    fn failed_task(summary: &str, unix_secs: Option<i64>) -> FailedTask {
        FailedTask {
            summary: summary.to_string(),
            timestamp: unix_secs
                .map(|s| OffsetDateTime::from_unix_timestamp(s).expect("a valid test timestamp")),
        }
    }

    /// 2023-11-14T22:13:20Z — the instant `short_time`'s own test pins, so the
    /// expected banner time is a known string rather than a re-derivation.
    const FAILED_AT: i64 = 1_700_000_000;

    /// A summary short enough that the whole sentence fits [`BANNER_PANE`], so a
    /// test can compare the drawn row whole instead of a prefix of it.
    const SHORT_FAILURE: &str = "Agent \"Fix CI\" failed: exit 1";

    /// The sentence QUOTES claude: the summary goes in exactly as the store kept
    /// it — quotes, doubled spaces and padding included — after the board's
    /// lead-in and the notice's own time.
    #[test]
    fn the_failed_task_banner_quotes_claudes_summary_verbatim() {
        let summary = " Agent \"Remediate  findings\" failed: Agent stalled: no progress for 600s ";
        assert_eq!(
            failed_task_banner(&failed_task(summary, Some(FAILED_AT))),
            format!("background task failed at 2023-11-14 22:13: {summary}")
        );
        // Undated: the sentence drops the time rather than inventing a `--`.
        assert_eq!(
            failed_task_banner(&failed_task(summary, None)),
            format!("background task failed: {summary}")
        );
    }

    /// A failure with NOTHING to quote reads as the bare sentence — never a
    /// dangling `: ` with nothing after it. Covers the empty summary the parse
    /// keeps for a notice with no readable `<summary>` (absent, unclosed or
    /// empty), and a summary of only whitespace and control characters, which the
    /// banner's `Span` would draw as blank; dated and undated alike.
    #[test]
    fn the_failed_task_banner_quotes_nothing_when_there_is_no_summary() {
        for summary in ["", "   ", "\t\n", "\u{1b}"] {
            assert_eq!(
                failed_task_banner(&failed_task(summary, Some(FAILED_AT))),
                "background task failed at 2023-11-14 22:13",
                "nothing to quote in {summary:?}, so no colon and no quote"
            );
            assert_eq!(
                failed_task_banner(&failed_task(summary, None)),
                "background task failed",
                "undated, and still nothing to quote in {summary:?}"
            );
        }
    }

    /// The failure banner, read off the DRAWN pane: a session claude never
    /// reported still leads with it, in the failure color — and it costs the
    /// transcript NO row of its own, because it takes over the one pinned row every
    /// selected session already reserves: the same geometry the click hit-test
    /// derives from `preview_banner(..).is_some()`.
    #[test]
    fn a_session_whose_background_task_failed_leads_with_a_banner_quoting_claude() {
        let (width, height) = BANNER_PANE;
        let mut session = sample_session();
        session.failed_task = Some(failed_task(SHORT_FAILURE, Some(FAILED_AT)));
        let mut app = App::new(vec![session], Scope::All, PathBuf::from("/tmp/launch"));
        assert!(
            app.reported_agent("sess-normal-1").is_none(),
            "the session must be UNREPORTED, or the status banner could be what drew"
        );

        let rows = inner_rows(&mut app, width, height);
        assert_eq!(
            rows[0],
            format!("background task failed at 2023-11-14 22:13: {SHORT_FAILURE}"),
            "the pane must lead with claude's own account of the failure"
        );
        let mut plain = banner_app(None);
        let _ = inner_rows(&mut plain, width, height);
        assert_eq!(
            app.preview_viewport_h, plain.preview_viewport_h,
            "the failure takes over the row every selected session already pins, so it \
             must cost the transcript no row of its own"
        );
        assert!(
            preview_banner(&app).is_some(),
            "the hit-test asks this, so it must agree with what was drawn"
        );

        // Style read off the buffer: a NAMED failure color at the banner's weight.
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
        assert_eq!(cell.fg, Color::Red, "a NAMED ansi color, never RGB");
        assert!(cell.modifier.contains(Modifier::BOLD));
    }

    /// A REPORTED session that also carries a failed task shows the failure ALONE on
    /// its pinned row: the reported status is only that row's last-resort fallback,
    /// and a standing failure outranks it just as it outranks the turn marker.
    ///
    /// Pinned over an EMPTY transcript — the one shape where the fallback is what
    /// the row would otherwise draw — so the premise proves `bg done` really was in
    /// line for the row before the failure took it.
    #[test]
    fn a_reported_session_with_a_failed_task_shows_the_failure_alone() {
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
                state: Some("done".to_string()),
                status: None,
                pid: None,
                started_at_ms: None,
            },
        );
        app.set_reported_agents(reported, None);
        assert_eq!(
            inner_rows(&mut app, width, height)[0],
            "bg done",
            "premise: with no failure, the reported status is what the row shows"
        );

        app.sessions[0].failed_task = Some(failed_task(SHORT_FAILURE, None));
        let sentence = format!("background task failed: {SHORT_FAILURE}");
        let rows = inner_rows(&mut app, width, height);
        assert_eq!(
            rows[0], sentence,
            "the failure must stand ALONE: no reported status and no separator beside it"
        );
        // Nothing of the status survives in its own color either: every cell the
        // sentence covers wears the failure's.
        let buffer = preview_buffer(&mut app, width, height);
        for (i, ch) in sentence.chars().enumerate() {
            let x = 1 + u16::try_from(i).expect("a banner shorter than the pane");
            let cell = buffer.cell((x, 1)).expect("a drawn banner cell");
            assert_eq!(cell.symbol(), ch.to_string());
            assert_eq!(
                cell.fg, FAILED_TASK_COLOR,
                "column {x} of the failure sentence"
            );
        }
    }

    // --- the banner's age (`startedAt`, as of the last poll) ---------------

    /// The capture's real `startedAt` (`claude 2.1.278`), so the banner is measured at
    /// the magnitude the wire actually sends.
    const STARTED_AT: i64 = 1_790_152_789_592;

    /// 46m59s after [`STARTED_AT`]: the stalled `claude -p` child the age exists for,
    /// one second short of the next minute, so an age that rounded would read `47m`.
    const POLLED_46M_LATER: i64 = STARTED_AT + (46 * 60 + 59) * 1_000;

    /// A real pid from the `claude 2.1.278` capture: put on a record, it makes the
    /// agent LIVE for the pinned row ([`reports_live_process`]).
    const LIVE_PID: u32 = 29628;

    /// `session` joined to the REPORTED record `agent`, in a map the poller stamped
    /// at `reported_at_ms`.
    fn reported_app(session: Session, agent: ReportedAgent, reported_at_ms: Option<i64>) -> App {
        let mut app = App::new(vec![session], Scope::All, PathBuf::from("/tmp/launch"));
        let mut reported = HashMap::new();
        reported.insert("sess-normal-1".to_string(), agent);
        app.set_reported_agents(reported, reported_at_ms);
        app
    }

    /// `session` reported as an INTERACTIVE record mid-turn (`live busy`, the shape
    /// every interactive record had in the capture), carrying `started_at_ms` and NO
    /// `pid`, in a map the poller stamped at `reported_at_ms`.
    fn aged_app(session: Session, started_at_ms: Option<i64>, reported_at_ms: Option<i64>) -> App {
        let mut agent = ReportedAgent::fixture("interactive", None, Some("busy"));
        agent.started_at_ms = started_at_ms;
        reported_app(session, agent, reported_at_ms)
    }

    /// [`aged_app`] over the sample session with an EMPTY transcript: with no turn
    /// marker to pin, the row falls through to the reported-status FALLBACK, the one
    /// shape where the status and its age are the row's whole text.
    fn aged_banner_app(started_at_ms: Option<i64>, reported_at_ms: Option<i64>) -> App {
        aged_app(empty_sample_session(), started_at_ms, reported_at_ms)
    }

    /// The sample session's REAL transcript, whose turn markers are in view, joined
    /// to a LIVE `live busy` record — one carrying [`LIVE_PID`] — with
    /// `started_at_ms`, in a map stamped at `reported_at_ms`: the shape where the
    /// pinned row names the turn at the top of the viewport and, when the age is
    /// known, the agent's status and age ride after it.
    fn live_marker_app(started_at_ms: Option<i64>, reported_at_ms: Option<i64>) -> App {
        let mut agent =
            ReportedAgent::fixture("interactive", None, Some("busy")).with_pid(LIVE_PID);
        agent.started_at_ms = started_at_ms;
        reported_app(sample_session(), agent, reported_at_ms)
    }

    /// A BACKGROUND record in `state` as the capture reports every finished or parked
    /// session: a `startedAt` the poll can measure ([`STARTED_AT`], against a map
    /// stamped [`POLLED_46M_LATER`]) and NO `pid`.
    fn pidless_aged_record(state: &str) -> ReportedAgent {
        let mut agent = ReportedAgent::fixture("background", Some(state), None);
        agent.started_at_ms = Some(STARTED_AT);
        agent
    }

    /// The sample session with an EMPTY transcript, so the pinned row can only be the
    /// reported-status FALLBACK.
    fn empty_sample_session() -> Session {
        let mut session = sample_session();
        session.file = empty_transcript_fixture();
        session
    }

    /// The turn marker the sample transcript pins at [`BANNER_PANE`] in the default
    /// bottom-anchored view — the row every session of that transcript leads with
    /// when there is no failure to state.
    const PINNED_SAMPLE_MARKER: &str = "\u{25cf} claude \u{b7} 10:00";

    /// A reported record with a `startedAt` states its AGE, measured against the
    /// poll's stamp, so a wedged child reads `live busy · 46m` instead of looking
    /// exactly like a healthy one. Drawn over a transcript with no marker to pin, so
    /// the row is the reported-status fallback and the aged status is its whole text.
    ///
    /// Read off the DRAWN row, because the row is what the user sees. The render has
    /// no clock and no probe to consult (this board's live probe panics under test),
    /// so the age can only have come from the POLLED map and the stamp it arrived
    /// with.
    #[test]
    fn the_status_banner_shows_the_reported_age_as_of_the_last_poll() {
        let (width, height) = BANNER_PANE;
        let mut app = aged_banner_app(Some(STARTED_AT), Some(POLLED_46M_LATER));

        let rows = inner_rows(&mut app, width, height);
        assert_eq!(
            rows[0], "live busy \u{b7} 46m",
            "the banner must state how long ago claude says the session started, \
             as of the poll that reported it"
        );
    }

    /// An AGED record that also carries a failed task draws the failure ALONE on its
    /// one banner row. A standing failure outranks every other line the row can show,
    /// the reported status and its age included, so neither of them — nor the
    /// separator that would join them to it — survives beside the failure sentence.
    ///
    /// Read off the DRAWN row. The failure is undated so the whole row fits
    /// [`BANNER_PANE`] and can be compared whole.
    #[test]
    fn an_aged_banner_with_a_failed_task_draws_the_failure_alone() {
        let (width, height) = BANNER_PANE;
        let mut app = aged_banner_app(Some(STARTED_AT), Some(POLLED_46M_LATER));
        assert_eq!(
            inner_rows(&mut app, width, height)[0],
            format!("live busy{BANNER_AGE_SEPARATOR}46m"),
            "premise: with no failure, the aged status is what the row shows"
        );

        app.sessions[0].failed_task = Some(failed_task(SHORT_FAILURE, None));
        let rows = inner_rows(&mut app, width, height);
        assert_eq!(
            rows[0],
            format!("background task failed: {SHORT_FAILURE}"),
            "one row: the failed task alone — no status, no age, no separator"
        );
    }

    /// With no age to state, the banner is EXACTLY today's: every cell of the pane
    /// (text and style, via `preview_buffer`) matches a board whose record never
    /// carried a `startedAt`, so no dangling separator, no `0s`, no restyled span.
    ///
    /// Three ways to have nothing to state, each drawn on its own board: the record
    /// has no `startedAt`; the map arrived with no stamp; the start lies after the
    /// stamp (claude's clock and this board's disagree).
    #[test]
    fn a_banner_with_no_age_to_state_is_exactly_todays_banner() {
        let (width, height) = BANNER_PANE;
        let mut baseline = aged_banner_app(None, None);
        assert_eq!(
            inner_rows(&mut baseline, width, height)[0],
            "live busy",
            "the baseline must really draw today's banner, or matching it proves nothing"
        );
        let todays = preview_buffer(&mut baseline, width, height);

        for (case, started_at_ms, reported_at_ms) in [
            ("no startedAt on the record", None, Some(POLLED_46M_LATER)),
            ("no stamp on the map", Some(STARTED_AT), None),
            (
                "a start 1 ms after the stamp",
                Some(POLLED_46M_LATER + 1),
                Some(POLLED_46M_LATER),
            ),
        ] {
            let mut app = aged_banner_app(started_at_ms, reported_at_ms);
            assert_eq!(
                preview_buffer(&mut app, width, height),
                todays,
                "{case}: the pane must be exactly today's, banner included"
            );
        }
    }

    /// While THIS board's quick reply to the row is in flight, the age is drawn
    /// NOWHERE: the pinned banner yields to the inline `cooking…` tail, and the tail
    /// does not borrow the age either.
    ///
    /// The age is for the case the banner is all a user gets, which is a child this
    /// board did not dispatch. This board's own in-flight reply already has its
    /// indicator, and one fact is told once. The record is LIVE with a known age, so
    /// without the send the pinned row WOULD carry the age.
    #[test]
    fn an_in_flight_reply_draws_no_age_beside_its_cooking_tail() {
        use super::super::app::Sending;

        let (width, height) = BANNER_PANE;
        let mut app = live_marker_app(Some(STARTED_AT), Some(POLLED_46M_LATER));
        app.sending = vec![Sending {
            session_id: "sess-normal-1".to_string(),
            message: "1a".to_string(),
            baseline_msg_count: 0,
        }];

        let rows = inner_rows(&mut app, width, height);
        assert!(
            rows.iter().any(|row| row.contains(REPLY_COOKING_LABEL)),
            "the inline tail must be on screen, or this test proves nothing: {rows:?}"
        );
        assert!(
            !rows.iter().any(|row| row.contains("46m")),
            "an in-flight reply's pane must not repeat the age: {rows:?}"
        );
    }

    // --- a live agent's status and age beside the pinned turn marker ------------

    /// The pinned row's "is this agent live?" is the record's `pid` ALONE. Every
    /// shape below is live with a pid and not live without one, and nothing else on
    /// the record moves the answer: not the qualifier's bucket (an `idle` agent with a
    /// pid is live although `agents::is_active` calls it resting, and a `done` one with
    /// a pid is live too), not the `kind`, and not a `startedAt` — every shape carries
    /// one, as every record in the capture did.
    #[test]
    fn reports_live_process_is_the_records_pid_alone() {
        for (kind, state, status) in [
            ("interactive", None, Some("busy")),
            ("interactive", None, Some("idle")),
            ("background", Some("working"), None),
            ("background", Some("blocked"), None),
            ("background", Some("done"), None),
            ("background", Some("stopped"), None),
            ("background", Some("failed"), None),
            ("background", None, None),
        ] {
            let mut agent = ReportedAgent::fixture(kind, state, status);
            agent.started_at_ms = Some(STARTED_AT);
            assert!(
                !reports_live_process(&agent),
                "{kind} {state:?}/{status:?} with a startedAt and NO pid must not be live"
            );
            assert!(
                reports_live_process(&agent.with_pid(LIVE_PID)),
                "{kind} {state:?}/{status:?} with a pid must be live"
            );
        }
    }

    /// A FINISHED session — `done`, `stopped` or `failed` — pins its turn marker
    /// ALONE, although its record carries a `startedAt` the poll can measure: claude
    /// reports no process for it (no `pid`), so there is no running agent whose status
    /// and age belong beside the turn being read.
    ///
    /// The premise draws the SAME record over an empty transcript, where the row is
    /// the reported-status FALLBACK: that line still states the status and its age,
    /// which proves the age really is known here and that the fallback is untouched
    /// by the live test.
    #[test]
    fn a_finished_session_with_a_known_age_pins_the_turn_marker_alone() {
        let (width, height) = BANNER_PANE;
        for state in ["done", "stopped", "failed"] {
            let mut fallback = reported_app(
                empty_sample_session(),
                pidless_aged_record(state),
                Some(POLLED_46M_LATER),
            );
            assert_eq!(
                inner_rows(&mut fallback, width, height)[0],
                format!("bg {state}{BANNER_AGE_SEPARATOR}46m"),
                "premise ({state}): the age is known, and the fallback still states it"
            );

            let mut app = reported_app(
                sample_session(),
                pidless_aged_record(state),
                Some(POLLED_46M_LATER),
            );
            assert_eq!(
                inner_rows(&mut app, width, height)[0],
                PINNED_SAMPLE_MARKER,
                "a {state} record with a startedAt and no pid must pin the turn marker \
                 alone: no status, no age, no separator"
            );
        }
    }

    /// A PARKED session — `blocked`, waiting on the user — pins its turn marker ALONE
    /// as well: its record carries a `startedAt` the poll can measure but no `pid`, so
    /// no running agent's status and age belong beside the turn. The premise proves
    /// the age is known, exactly as for a finished session.
    #[test]
    fn a_blocked_session_with_a_known_age_pins_the_turn_marker_alone() {
        let (width, height) = BANNER_PANE;
        let mut fallback = reported_app(
            empty_sample_session(),
            pidless_aged_record("blocked"),
            Some(POLLED_46M_LATER),
        );
        assert_eq!(
            inner_rows(&mut fallback, width, height)[0],
            format!("bg needs input{BANNER_AGE_SEPARATOR}46m"),
            "premise: the age is known, and the fallback still states it"
        );

        let mut app = reported_app(
            sample_session(),
            pidless_aged_record("blocked"),
            Some(POLLED_46M_LATER),
        );
        assert_eq!(
            inner_rows(&mut app, width, height)[0],
            PINNED_SAMPLE_MARKER,
            "a blocked record with a startedAt and no pid must pin the turn marker alone"
        );
    }

    /// Every record with a `pid` and a known age pins the marker, then the banner
    /// separator, then `<status> · <age>` — whatever its qualifier says. Pinned over
    /// the two live shapes the capture holds (`busy` and `idle`) and over a `done`
    /// record that carries a pid, so neither `agents::is_active` (which calls `idle`
    /// and `done` resting) nor any `classify` bucket can stand in for the pid.
    #[test]
    fn a_record_with_a_pid_and_a_known_age_pins_the_marker_then_its_status_and_age() {
        let (width, height) = BANNER_PANE;
        for (kind, state, status, phrase) in [
            ("interactive", None, Some("busy"), "live busy"),
            ("interactive", None, Some("idle"), "live idle"),
            ("background", Some("done"), None, "bg done"),
        ] {
            let mut agent = ReportedAgent::fixture(kind, state, status).with_pid(LIVE_PID);
            agent.started_at_ms = Some(STARTED_AT);
            let mut app = reported_app(sample_session(), agent, Some(POLLED_46M_LATER));
            assert_eq!(
                inner_rows(&mut app, width, height)[0],
                format!(
                    "{PINNED_SAMPLE_MARKER}{HEADER_SEPARATOR}{phrase}{BANNER_AGE_SEPARATOR}46m"
                ),
                "{phrase}: a record with a pid and a known age must pin the marker, then \
                 its status and age"
            );
        }
    }

    /// A LIVE agent — its record carries a `pid` — with a known age keeps the pinned
    /// turn marker AND states its status and age on the same row: the marker first,
    /// then the banner separator, then `<status> · <age>` in the board's Cyan + BOLD.
    /// Neither feature gives way to the other on a pane wide enough for both.
    ///
    /// Read off the DRAWN row and its cells, in the default bottom-anchored view over
    /// a transcript that really has turns.
    #[test]
    fn a_live_agent_pins_its_turn_marker_then_its_status_and_age() {
        let (width, height) = BANNER_PANE;
        let mut app = live_marker_app(Some(STARTED_AT), Some(POLLED_46M_LATER));
        let aged_status = format!("live busy{BANNER_AGE_SEPARATOR}46m");

        let rows = inner_rows(&mut app, width, height);
        assert_eq!(
            rows[0],
            format!("{PINNED_SAMPLE_MARKER}{HEADER_SEPARATOR}{aged_status}"),
            "one row: the turn marker first, the separator, then the status and its age"
        );

        // The appended status wears the banner's own style, not the marker's.
        let buffer = preview_buffer(&mut app, width, height);
        let status_x = 1 + u16::try_from(
            PINNED_SAMPLE_MARKER.chars().count() + HEADER_SEPARATOR.chars().count(),
        )
        .expect("a short prefix");
        for (i, ch) in aged_status.chars().enumerate() {
            let x = status_x + u16::try_from(i).expect("a short status");
            let cell = buffer.cell((x, 1)).expect("a drawn banner cell");
            assert_eq!(cell.symbol(), ch.to_string(), "column {x} of the status");
            assert_eq!(cell.fg, Color::Cyan, "column {x}: a NAMED color, never RGB");
            assert!(cell.modifier.contains(Modifier::BOLD), "column {x}: BOLD");
        }
    }

    /// A LIVE agent — its record carries a `pid` — whose age is not known pins the
    /// turn marker ALONE — no status, no separator — because the suffix states an age
    /// and there is none to state. Each way of having no age is drawn on its own board.
    #[test]
    fn a_live_agent_with_no_known_age_pins_the_turn_marker_alone() {
        let (width, height) = BANNER_PANE;
        for (case, started_at_ms, reported_at_ms) in [
            ("no startedAt on the record", None, Some(POLLED_46M_LATER)),
            ("no stamp on the map", Some(STARTED_AT), None),
            (
                "a start 1 ms after the stamp",
                Some(POLLED_46M_LATER + 1),
                Some(POLLED_46M_LATER),
            ),
        ] {
            let mut app = live_marker_app(started_at_ms, reported_at_ms);
            assert!(
                app.reported_agent("sess-normal-1")
                    .is_some_and(reports_live_process),
                "{case}: the session must really be reported AND live, or this is the \
                 unreported or the not-live case"
            );
            assert_eq!(
                inner_rows(&mut app, width, height)[0],
                PINNED_SAMPLE_MARKER,
                "{case}: with no age to state the row is exactly the turn marker"
            );
        }
    }

    /// An UNREPORTED session pins the turn marker ALONE, even while the poll's map
    /// (stamped, so ages CAN be stated) reports ANOTHER session that is live with a
    /// known age: the status beside the marker is the SELECTED session's, and there
    /// is none.
    #[test]
    fn an_unreported_session_pins_the_turn_marker_alone() {
        let (width, height) = BANNER_PANE;
        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        let mut elsewhere =
            ReportedAgent::fixture("interactive", None, Some("busy")).with_pid(LIVE_PID);
        elsewhere.started_at_ms = Some(STARTED_AT);
        let mut reported = HashMap::new();
        reported.insert("sess-somewhere-else".to_string(), elsewhere);
        app.set_reported_agents(reported, Some(POLLED_46M_LATER));
        assert!(
            app.reported_agent("sess-normal-1").is_none(),
            "the selected session must really be unreported"
        );

        let rows = inner_rows(&mut app, width, height);
        assert_eq!(
            rows[0], PINNED_SAMPLE_MARKER,
            "an unreported session's row is exactly the turn marker: {rows:?}"
        );
    }

    /// A LIVE agent that also carries a failed task pins the failure ALONE, over a
    /// transcript whose turn markers are in view: the failure outranks the marker AND
    /// the status and age a live agent's marker carries, so none of them is drawn.
    #[test]
    fn a_live_agent_with_a_failed_task_pins_the_failure_alone() {
        let (width, height) = BANNER_PANE;
        let mut app = live_marker_app(Some(STARTED_AT), Some(POLLED_46M_LATER));
        assert_eq!(
            inner_rows(&mut app, width, height)[0],
            format!("{PINNED_SAMPLE_MARKER}{HEADER_SEPARATOR}live busy{BANNER_AGE_SEPARATOR}46m"),
            "premise: with no failure, the row names the turn marker and the live status"
        );

        app.sessions[0].failed_task = Some(failed_task(SHORT_FAILURE, None));
        let rows = inner_rows(&mut app, width, height);
        assert_eq!(
            rows[0],
            format!("background task failed: {SHORT_FAILURE}"),
            "the failure alone: no marker, no status, no age, no separator"
        );
    }

    // --- the pinned row's precedence: a standing failure outranks the turn ------

    /// Whether a drawn preview row is a TURN MARKER — the line a turn opens with,
    /// led by one of the two turn glyphs — rather than body text.
    fn is_turn_marker_row(row: &str) -> bool {
        row.starts_with("\u{25b6} you") || row.starts_with("\u{25cf} claude")
    }

    /// A standing failure OUTRANKS the turn marker on the pinned row.
    ///
    /// Pinned on the shape where the sticky header would otherwise draw: a
    /// transcript with turn markers IN VIEW beneath the row, whose top-of-viewport
    /// marker resolves, on a session claude ALSO reports — so neither the marker
    /// nor the status may be what the row shows. It must be the failure sentence
    /// alone, every cell of it in [`FAILED_TASK_COLOR`] at the banner's BOLD
    /// weight, read off the drawn buffer.
    #[test]
    fn a_failed_task_outranks_the_turn_marker_on_the_pinned_row() {
        let (width, height) = BANNER_PANE;
        let mut app = banner_app(Some("blocked"));
        app.sessions[0].failed_task = Some(failed_task(SHORT_FAILURE, Some(FAILED_AT)));

        let rows = inner_rows(&mut app, width, height);
        assert!(
            rows[1..].iter().any(|row| is_turn_marker_row(row)),
            "premise: a turn marker must be IN VIEW beneath the pinned row: {rows:?}"
        );
        let offset = usize::try_from(app.preview_scroll).expect("a small resolved offset");
        assert!(
            app.preview_marker_at(width - 2, offset).is_some(),
            "premise: a turn marker owns the top of the viewport, so the sticky \
             header WOULD draw here without the failure"
        );

        let sentence = format!("background task failed at 2023-11-14 22:13: {SHORT_FAILURE}");
        assert_eq!(
            rows[0], sentence,
            "a standing failure must outrank the turn marker on the pinned row"
        );
        let buffer = preview_buffer(&mut app, width, height);
        for (i, ch) in sentence.chars().enumerate() {
            let x = 1 + u16::try_from(i).expect("a banner shorter than the pane");
            let cell = buffer.cell((x, 1)).expect("a drawn banner cell");
            assert_eq!(cell.symbol(), ch.to_string());
            assert_eq!(
                cell.fg, FAILED_TASK_COLOR,
                "column {x} of the pinned failure must wear FAILED_TASK_COLOR"
            );
            assert!(
                cell.modifier.contains(Modifier::BOLD),
                "column {x} of the pinned failure must be BOLD"
            );
        }
    }

    /// The SAME transcript with NO failed task pins the turn marker again: the
    /// failure is a precedence OVER the sticky header, never a replacement of it.
    ///
    /// The premise holds the failure on the row first, so the return is observed
    /// on the very `App` that showed it. The marker that comes back is then held to
    /// a twin that never carried a failure, so it is whatever the sticky header
    /// draws there rather than a guessed string.
    #[test]
    fn the_turn_marker_returns_to_the_pinned_row_without_a_failed_task() {
        let (width, height) = BANNER_PANE;
        let mut app = banner_app(Some("blocked"));
        app.sessions[0].failed_task = Some(failed_task(SHORT_FAILURE, Some(FAILED_AT)));
        let rows = inner_rows(&mut app, width, height);
        assert!(
            rows[0].starts_with(FAILED_TASK_BANNER_LEAD),
            "premise: the failure holds the pinned row first: {rows:?}"
        );

        app.sessions[0].failed_task = None;
        let rows = inner_rows(&mut app, width, height);
        assert!(
            is_turn_marker_row(&rows[0]),
            "with no failed task the turn marker must return to the pinned row: {rows:?}"
        );
        let mut never_failed = banner_app(Some("blocked"));
        assert_eq!(
            rows[0],
            inner_rows(&mut never_failed, width, height)[0],
            "it must be the very marker a never-failed twin pins there"
        );
    }

    // --- sticky banner: the turn marker in every scroll state, including
    // bottom-anchored ----------------------------------------------------------

    /// With no failed background task standing (a failure outranks it — see
    /// `a_failed_task_outranks_the_turn_marker_on_the_pinned_row`), the pinned
    /// banner row always shows the marker `Line` of whichever turn
    /// sits at the TOP of the viewport — in EVERY scroll state, including the
    /// default bottom-anchored one (re-armed on every selection change), not
    /// only once the user scrolls away. It live-updates as they keep
    /// scrolling, and keeps tracking the (different) turn now at the top once
    /// they return to the bottom (`End`, which re-arms `preview_follow_bottom`).
    ///
    /// Row positions are looked up directly off the SAME cache `render_preview`
    /// draws from (`app.preview_text`), rather than hardcoded, so this cannot
    /// silently start proving nothing if the fixture's wording ever changes.
    /// The pane is 80 columns wide (none of this short fixture's lines
    /// soft-wrap at that width, so a content row and its visual row coincide
    /// exactly) and only 5 rows tall — short enough that `max_offset` reaches
    /// the transcript's LAST turn, so the scroll journey below exercises real,
    /// reachable offsets rather than ones the clamp would shorten first.
    #[test]
    fn scrolling_away_from_the_bottom_swaps_the_banner_to_the_turn_marker_under_it() {
        let width = 80u16;
        let height = 5u16;
        let inner_width = width - 2;
        let mut app = banner_app(Some("blocked"));

        // Bottom-anchored (the default): the pinned row already shows the
        // turn marker under the top of the viewport, exactly as it will once
        // the user scrolls away — `friendly_status` no longer renders here in
        // any state.
        assert!(
            app.preview_follow_bottom,
            "a freshly selected session must start bottom-anchored"
        );
        let rows = inner_rows(&mut app, width, height);
        assert_eq!(
            rows[0], "\u{25b6} you \u{b7} 10:01",
            "bottom-anchored must show the turn marker under the top of the viewport"
        );

        // Locate this fixture's two `you` turns and its one `claude` turn by
        // their marker's own span (never a second span, which could be an
        // annotation or a body line quoting the same words).
        let rows_with_first_span = |app: &mut App, needle: &str| -> Vec<usize> {
            app.preview_text(inner_width)
                .lines
                .iter()
                .enumerate()
                .filter(|(_, l)| {
                    l.spans
                        .first()
                        .is_some_and(|s| s.content.as_ref() == needle)
                })
                .map(|(i, _)| i)
                .collect()
        };
        let you_rows = rows_with_first_span(&mut app, "\u{25b6} you");
        let claude_rows = rows_with_first_span(&mut app, "\u{25cf} claude");
        assert_eq!(you_rows.len(), 2, "the fixture has exactly two `you` turns");
        assert_eq!(
            claude_rows.len(),
            1,
            "the fixture has exactly one `claude` turn"
        );
        assert!(
            you_rows[0] < claude_rows[0] && claude_rows[0] < you_rows[1],
            "the turns must run you -> claude -> you, or the scroll journey below \
             proves nothing about ownership: you={you_rows:?} claude={claude_rows:?}"
        );

        // Scroll away to the FIRST `you` turn's own row: the pinned row must
        // swap to ITS marker.
        app.preview_follow_bottom = false;
        app.preview_scroll = u32::try_from(you_rows[0]).expect("a small test offset");
        let rows = inner_rows(&mut app, width, height);
        assert!(
            rows[0].starts_with("\u{25b6} you"),
            "scrolled to the first `you` turn, the pinned row must show its \
             marker: {:?}",
            rows[0]
        );
        assert!(!rows[0].contains("bg needs input"));

        // Scroll further to the `claude` turn: the pinned row updates LIVE to
        // the new turn under it.
        app.preview_scroll = u32::try_from(claude_rows[0]).expect("a small test offset");
        let rows = inner_rows(&mut app, width, height);
        assert!(
            rows[0].starts_with("\u{25cf} claude"),
            "scrolled to the claude turn, the pinned row must follow: {:?}",
            rows[0]
        );

        // Scroll to a BODY row of that same claude turn (one row past its own
        // marker, still short of the next `you` marker): the pinned row must
        // still read the claude turn's marker — ownership by the turn, not by
        // the exact marker row.
        app.preview_scroll = u32::try_from(claude_rows[0] + 1).expect("a small test offset");
        assert!(
            app.preview_scroll < u32::try_from(you_rows[1]).expect("a small test offset"),
            "the probed body row must still belong to the claude turn"
        );
        let rows = inner_rows(&mut app, width, height);
        assert!(
            rows[0].starts_with("\u{25cf} claude"),
            "a body row still belongs to the turn whose marker precedes it: {:?}",
            rows[0]
        );

        // Scroll to the SECOND `you` turn.
        app.preview_scroll = u32::try_from(you_rows[1]).expect("a small test offset");
        let rows = inner_rows(&mut app, width, height);
        assert!(
            rows[0].starts_with("\u{25b6} you"),
            "scrolled to the second `you` turn, the pinned row must follow: {:?}",
            rows[0]
        );

        // Back to the bottom (`End`): follow-bottom re-arms and the banner
        // reverts to the SAME marker the default bottom-anchored state opened
        // with (the first `you` turn, unchanged by the scroll journey above).
        app.preview_bottom();
        assert!(app.preview_follow_bottom);
        let rows = inner_rows(&mut app, width, height);
        assert_eq!(
            rows[0], "\u{25b6} you \u{b7} 10:01",
            "returning to the bottom must revert the banner to the top-of-viewport marker"
        );
    }

    /// A reported session whose transcript renders to EMPTY has no marker to
    /// swap to, so even a (synthetic) non-bottom-anchored state must fall back
    /// to `friendly_status` rather than blanking the pinned row.
    #[test]
    fn an_empty_transcript_has_no_marker_so_the_banner_stays_friendly_status() {
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
                pid: None,
                started_at_ms: None,
            },
        );
        app.set_reported_agents(reported, None);
        assert!(
            app.preview_text(width - 2).lines.is_empty(),
            "the fixture must render an EMPTY transcript for this test to reach \
             the seam it targets"
        );

        // Not a reachable state through real scroll keys (there is nothing to
        // scroll), but forcing it proves the fallback rather than assuming it.
        app.preview_follow_bottom = false;

        let rows = inner_rows(&mut app, width, height);
        assert_eq!(
            rows[0], "bg needs input",
            "no marker exists to pin to, so the banner must fall back to \
             friendly_status: {rows:?}"
        );
    }

    /// A reported `App` over a transcript WRITTEN BY THE TEST, so a banner case can
    /// pick the transcript shape it needs (a wrap, a length) instead of taking
    /// whatever `sample_session`'s fixture happens to have.
    ///
    /// The file is handed straight to a synthetic `Session` and lives outside the
    /// discovery root, so it can never disturb `store`'s discovered/session counts
    /// (the same discipline `empty_transcript_fixture` documents).
    fn banner_app_over(jsonl: &str, tag: &str) -> App {
        banner_app_over_labelled(jsonl, tag, &sample_session().label)
    }

    /// [`banner_app_over`] with the row's LABEL chosen as well, so a name-only query
    /// for a word in the TRANSCRIPT can still keep the row on the board.
    ///
    /// The filter runs before any preview exists, and its haystack is built once at
    /// construction — so a label miss deselects the session and leaves the pane with
    /// nothing to draw at all, and a label set afterwards would never reach the index.
    /// Same discipline as [`markable_session`].
    fn banner_app_over_labelled(jsonl: &str, tag: &str, label: &str) -> App {
        let dir = unique_temp_dir(tag);
        let file = dir.join("sess-normal-1.jsonl");
        std::fs::write(&file, jsonl).expect("write temp transcript");
        let mut session = sample_session();
        session.file = file;
        session.label = label.to_string();
        let mut app = App::new(vec![session], Scope::All, PathBuf::from("/tmp/launch"));
        let mut reported = HashMap::new();
        reported.insert(
            "sess-normal-1".to_string(),
            ReportedAgent {
                kind: "background".to_string(),
                id: None,
                state: Some("blocked".to_string()),
                status: None,
                pid: None,
                started_at_ms: None,
            },
        );
        app.set_reported_agents(reported, None);
        app
    }

    /// The content rows whose line STARTS with `needle` as its own first span — a
    /// turn's marker row, never a body line that merely quotes the same words.
    fn marker_rows(app: &mut App, inner_width: u16, needle: &str) -> Vec<usize> {
        app.preview_text(inner_width)
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| {
                l.spans
                    .first()
                    .is_some_and(|s| s.content.as_ref() == needle)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// What the pinned banner SHOULD read once the pane has resolved to wrapped row
    /// `offset`, derived WITHOUT going through `marker_at_top`: the last turn marker
    /// whose own FIRST SCREEN ROW — read off the cached prefix map via
    /// `App::preview_rows_above` — is at or before `offset`, flattened to its drawn
    /// text.
    ///
    /// An INDEPENDENT oracle on purpose. Asserting against a hardcoded string makes a
    /// test that has to be re-guessed whenever a fixture's wording moves; asserting
    /// against `marker_at_top` itself would let a bug in the lookup satisfy its own
    /// expectation. This walks the map the other way — forward over every marker —
    /// and so agrees with the binary search only when the search is right.
    ///
    /// The set it recognises is EXACTLY the two turn glyphs — a first span of
    /// `▶ you` or `● claude`. A `# `-led summary head is deliberately OUTSIDE
    /// it: `render_record`'s `Some("summary")` arm emits its whole `# {summary}` text
    /// as that line's single first span, so equality against the two glyphs can never
    /// reach it, and this oracle reads a summary-led transcript as beginning at its
    /// first `▶ you` turn instead.
    ///
    /// Safe today only because nothing drives that shape: every caller of this oracle
    /// builds its `App` through `banner_app_over`, and the JSONL those callers
    /// generate holds `user` and `assistant` records only — never a `summary` one.
    /// Offset 0 itself IS already probed, by
    /// `the_banner_names_the_opening_turn_at_the_ctrl_t_jump_endpoint`; it is the
    /// summary-LED transcript that no test pairs with it. (`sample_session`'s fixture
    /// does lead with a summary, but the tests over it go through `banner_app`, which
    /// never reaches this oracle.)
    ///
    /// So a failure that appears when probing offset 0 on a summary-led transcript is
    /// THIS ORACLE's blind spot, not a production bug: the pane correctly pins the
    /// summary head, which owns content row 0. Teach the filter that head — do NOT
    /// "fix" `marker_owning_row` or the `Some("summary")` arm to agree with it.
    ///
    /// The path this shadows is covered where it is produced:
    /// `RenderedPreview::markers` collects that head at `content_row: 0`, and
    /// `collected_markers_address_their_rendered_rows_summary_head_included` in
    /// `store::preview` holds it to the row it renders on.
    fn banner_should_show(app: &mut App, inner_width: u16, offset: usize) -> String {
        let lines = app.preview_text(inner_width).lines;
        let markers: Vec<(usize, String)> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| {
                l.spans.first().is_some_and(|s| {
                    matches!(s.content.as_ref(), "\u{25b6} you" | "\u{25cf} claude")
                })
            })
            .map(|(i, l)| (i, l.spans.iter().map(|s| s.content.as_ref()).collect()))
            .collect();
        let mut owning: Option<&(usize, String)> = None;
        for candidate in &markers {
            let first_row = app
                .preview_rows_above(inner_width, candidate.0)
                .expect("a marker row inside the transcript");
            if first_row <= offset {
                owning = Some(candidate);
            }
        }
        // Above every marker, the opening turn still owns the pinned row.
        owning
            .or_else(|| markers.first())
            .map(|(_, text)| text.clone())
            .expect("the transcript must hold at least one turn marker")
    }

    /// Render at `(width, height)`, then assert the pinned row names the turn that
    /// owns the offset the pane ACTUALLY resolved to.
    ///
    /// Reading the resolved offset back out of `App::preview_scroll` rather than
    /// trusting the requested one is what makes this usable at the bottom of a
    /// transcript: `clamp_preview_offset` caps a request past `content_h -
    /// inner_height`, so a probe near the tail is drawn at a SMALLER offset than it
    /// asked for — and the banner must agree with where the pane landed, not with
    /// where the test pointed.
    fn assert_banner_tracks(app: &mut App, width: u16, height: u16, requested: usize) {
        app.preview_scroll = u32::try_from(requested).expect("a small test offset");
        let rows = inner_rows(app, width, height);
        let resolved = usize::try_from(app.preview_scroll).expect("a small resolved offset");
        let expected = banner_should_show(app, width - 2, resolved);
        assert_eq!(
            rows[0], expected,
            "asked for row {requested}, pane resolved to {resolved}; the banner must \
             name the turn owning THAT row"
        );
        assert!(
            !rows[0].contains("bg needs input"),
            "the reported status must never render in the preview pane"
        );
    }

    /// How many rendered lines the preview's tail cap used to keep before it was
    /// deleted. Named here ONLY so the banner test below can prove its transcript
    /// reaches past it — nothing in the renderer or the pane knows this number any
    /// more (`store::preview`'s own suite names it separately, for the same reason).
    const FORMER_TAIL_CAP: usize = 600;

    /// How many turns the long-transcript case below writes. Sized so the rendered
    /// transcript comfortably exceeds [`FORMER_TAIL_CAP`] (each turn costs a blank
    /// line, a marker and a body line, so ~3 rows a turn).
    const LONG_TRANSCRIPT_TURNS: usize = 400;

    /// The pinned banner tracks the top-of-viewport turn on a transcript LONGER than
    /// the tail cap that used to truncate the preview — including for turns that sat
    /// ABOVE the old cut, whose markers the cap would have dropped outright.
    ///
    /// This is the regression the rebase onto the cap's removal is most likely to
    /// introduce silently. The marker list is rebased by the running line offset
    /// ALONE now; while the cap existed it was additionally shifted up by the cut and
    /// filtered, and a resolution that kept that second rebase — or that kept pinning
    /// markers to a capped `Text` — would still render a plausible banner for the
    /// tail while naming the wrong turn (or nothing) for everything above it.
    ///
    /// Asserted at the TOP of the transcript on purpose: offset 0 is precisely the
    /// region a 600-row cap removed.
    #[test]
    fn the_banner_tracks_the_top_turn_on_a_transcript_past_the_former_tail_cap() {
        let (width, height) = BANNER_PANE;
        let inner_width = width - 2;
        let mut jsonl = String::new();
        for turn in 0..LONG_TRANSCRIPT_TURNS {
            jsonl.push_str(&format!(
                r#"{{"type":"user","sessionId":"sess-normal-1","cwd":"/Users/me/project-alpha","timestamp":"2026-07-04T10:00:00.000Z","message":{{"role":"user","content":"ask number {turn}"}}}}"#
            ));
            jsonl.push('\n');
            jsonl.push_str(&format!(
                r#"{{"type":"assistant","sessionId":"sess-normal-1","cwd":"/Users/me/project-alpha","timestamp":"2026-07-04T10:00:05.000Z","message":{{"role":"assistant","content":"answer number {turn}"}}}}"#
            ));
            jsonl.push('\n');
        }
        let mut app = banner_app_over(&jsonl, "banner-long");

        // The fixture must really outrun the former cap, or this proves nothing.
        let content_lines = app.preview_line_count(inner_width);
        assert!(
            content_lines > FORMER_TAIL_CAP,
            "the transcript must exceed the former {FORMER_TAIL_CAP}-line cap \
             (got {content_lines} lines)"
        );

        let you_rows = marker_rows(&mut app, inner_width, "\u{25b6} you");
        let claude_rows = marker_rows(&mut app, inner_width, "\u{25cf} claude");
        assert_eq!(you_rows.len(), LONG_TRANSCRIPT_TURNS);
        assert_eq!(claude_rows.len(), LONG_TRANSCRIPT_TURNS);
        app.preview_follow_bottom = false;

        // The turns a 600-row cap would have CUT: the opening ones, plus the last turn
        // that still sat above the old cut. Each is probed at the screen row the
        // wrapper starts its marker on, read off the same prefix map the pane draws by.
        let above_the_cut: Vec<usize> = [you_rows[0], claude_rows[0], you_rows[1]]
            .into_iter()
            .map(|line| {
                app.preview_rows_above(inner_width, line)
                    .expect("a marker row inside the transcript")
            })
            .collect();
        assert!(
            above_the_cut.iter().all(|&row| row < FORMER_TAIL_CAP),
            "the probed turns must sit ABOVE the former cut, or the cap's removal is \
             not what is being tested: {above_the_cut:?}"
        );
        for row in above_the_cut {
            assert_banner_tracks(&mut app, width, height, row);
        }

        // A turn in the MIDDLE, and the transcript's tail — so removing the cap did
        // not simply move the breakage to the other end. The tail probe is past the
        // scroll clamp on this short pane, which `assert_banner_tracks` handles by
        // asserting against the offset the pane resolved to.
        for line in [
            claude_rows[LONG_TRANSCRIPT_TURNS / 2],
            *claude_rows.last().expect("a claude turn"),
        ] {
            let row = app
                .preview_rows_above(inner_width, line)
                .expect("a marker row inside the transcript");
            assert_banner_tracks(&mut app, width, height, row);
        }
    }

    /// The banner resolves the top-of-viewport turn through the WRAPPER's own row map,
    /// so a soft-wrapped GFM table row inside a turn cannot slide the banner onto a
    /// neighbouring turn.
    ///
    /// The case that a per-line display-WIDTH model gets wrong: such a model derives a
    /// line's height as `ceil(width / inner_width)`, which disagrees with
    /// `WordWrapper` wherever the wrapper breaks on a word boundary instead — and a
    /// wrapped table cell is exactly that. Every disagreement above the target row
    /// shifts the resolved content row, so the banner names the turn before or after
    /// the real one. Here the expected rows are read from the SAME cached prefix map
    /// the pane draws by (`App::preview_rows_above`), so the assertion is "the banner
    /// agrees with what was painted" rather than a hardcoded guess.
    #[test]
    fn the_banner_tracks_the_top_turn_across_a_wrapped_table_row() {
        // A pane NARROWER than a floor-width grid for this table (3 * 10 + 2 * 3 = 36
        // columns), so `render_table` takes the stacked-record fallback — where a
        // table row is an ORDINARY logical line that the preview's own
        // `Wrap { trim: false }` soft-wraps, and the fixed-width `─` rule between
        // records overflows into several rows. That is the shape this test needs: a
        // GRID fits every cell to the pane and so never soft-wraps, which is why the
        // wide-pane version of this case cannot exercise the mapping at all.
        let (width, height) = (26u16, 12u16);
        let inner_width = width - 2;
        let table = "| column one | column two | column three |\\n\
             | --- | --- | --- |\\n\
             | a deliberately long first cell that must soft wrap several times \
             | a second cell that also runs well past the column width \
             | a third cell of similar length to force more rows |";
        let jsonl = format!(
            "{}\n{}\n",
            format_args!(
                r#"{{"type":"assistant","sessionId":"sess-normal-1","cwd":"/Users/me/project-alpha","timestamp":"2026-07-04T10:00:00.000Z","message":{{"role":"assistant","content":"{table}"}}}}"#
            ),
            r#"{"type":"user","sessionId":"sess-normal-1","cwd":"/Users/me/project-alpha","timestamp":"2026-07-04T10:05:00.000Z","message":{"role":"user","content":"after the table"}}"#
        );
        let mut app = banner_app_over(&jsonl, "banner-table");

        let claude_rows = marker_rows(&mut app, inner_width, "\u{25cf} claude");
        let you_rows = marker_rows(&mut app, inner_width, "\u{25b6} you");
        assert_eq!(claude_rows.len(), 1, "one claude turn carries the table");
        assert_eq!(you_rows.len(), 1, "one user turn follows it");

        let claude_first_row = app
            .preview_rows_above(inner_width, claude_rows[0])
            .expect("the claude marker is inside the transcript");
        let you_first_row = app
            .preview_rows_above(inner_width, you_rows[0])
            .expect("the user marker is inside the transcript");

        // The table must genuinely SOFT-WRAP, or this test degenerates into the
        // unwrapped case the plain banner test already covers — and the width model it
        // is here to rule out would agree with the wrapper. More screen rows than
        // logical lines between the two markers is exactly that property.
        let table_span = you_first_row - claude_first_row;
        let table_lines = you_rows[0] - claude_rows[0];
        assert!(
            table_span > table_lines,
            "the table must soft-wrap at this width, or the test proves nothing \
             (span={table_span} rows over {table_lines} lines)"
        );

        app.preview_follow_bottom = false;

        // EVERY wrapped row of the claude turn — its marker row through the last row
        // of the wrapped table — still belongs to the claude turn, and the row the
        // WRAPPER starts the user turn on (never one a `ceil(width / inner_width)`
        // model would have guessed) is where the banner switches.
        for row in claude_first_row..=you_first_row {
            assert_banner_tracks(&mut app, width, height, row);
        }
    }

    /// The cells of preview row `y` INSIDE the borders, as `(symbol, marked)` — the
    /// drawn text paired with whether [`PREVIEW_MATCH_MODIFIER`] reached each cell.
    ///
    /// Reads the CELLS, not the model: a mark that never reached the terminal is not a
    /// highlight (PATTERNS — assert drawn cells).
    fn row_cells(buffer: &ratatui::buffer::Buffer, y: u16, width: u16) -> Vec<(String, bool)> {
        (1..width - 1)
            .filter_map(|x| buffer.cell((x, y)))
            .map(|cell| {
                (
                    cell.symbol().to_string(),
                    cell.modifier.contains(PREVIEW_MATCH_MODIFIER),
                )
            })
            .collect()
    }

    /// The inner columns a `query` occurrence COVERS on a drawn row, derived from the
    /// row's own symbols and the query string ALONE.
    ///
    /// An INDEPENDENT oracle, in the spirit of [`banner_should_show`]: it locates the
    /// query in what was painted and reports the columns it spans, so neither
    /// `highlight_matched_spans` nor the banner can satisfy its own expectation. Both
    /// assumptions it rests on are ASSERTED rather than trusted — one cell per char
    /// (a wide glyph would shift every column after it) and exactly one occurrence
    /// (a second one would make the answer partial).
    fn query_columns(cells: &[(String, bool)], query: &str) -> Vec<usize> {
        let text: String = cells.iter().map(|(symbol, _)| symbol.as_str()).collect();
        assert!(
            cells.iter().all(|(symbol, _)| symbol.chars().count() == 1),
            "this oracle maps one cell to one char: {text:?}"
        );
        assert_eq!(
            text.matches(query).count(),
            1,
            "the query must occur exactly once on the probed row: {text:?}"
        );
        let at = text
            .find(query)
            .expect("an occurrence that was just counted");
        let first = text[..at].chars().count();
        (first..first + query.chars().count()).collect()
    }

    /// The columns of a drawn row the match emphasis actually reached.
    fn marked_columns(cells: &[(String, bool)]) -> Vec<usize> {
        cells
            .iter()
            .enumerate()
            .filter_map(|(col, (_, marked))| marked.then_some(col))
            .collect()
    }

    /// The drawn text of a row read by [`row_cells`], for assertion messages.
    fn cells_text(cells: &[(String, bool)]) -> String {
        cells
            .iter()
            .map(|(symbol, _)| symbol.as_str())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    /// The word the marked-banner case searches for: it occurs in every `● claude`
    /// turn marker — the very line the pinned banner reuses — and nowhere else on that
    /// line (a bound handle of `@claude` is suppressed and a model id loses its
    /// `claude-` vendor prefix, so neither can add a second occurrence).
    const BANNER_MARKER_QUERY: &str = "claude";

    /// How many turns the marked-banner case writes. Enough that the probed marker
    /// sits well clear of the scroll clamp at the transcript's tail, so the pane
    /// resolves to exactly the offset the probe asks for.
    const MARKED_BANNER_TURNS: usize = 12;

    /// A query that hits a TURN MARKER is marked in the PINNED BANNER exactly as it is
    /// in the transcript row beneath it.
    ///
    /// The banner reuses an already-rendered marker `Line` while the drawn window
    /// re-styles the lines it paints ([`highlight_matched_spans`]) — so reusing the
    /// UNMARKED line put ONE line on screen in TWO appearances: marked in the
    /// transcript, unmarked in the pinned row directly above it. Probed at the offset
    /// that puts the marker on the viewport's TOP row, which is the state where both
    /// copies are drawn at once and the disagreement is visible.
    ///
    /// Both rows are held to [`query_columns`] — an oracle derived from the drawn
    /// symbols and the query alone — rather than to each other, so the agreement
    /// cannot be satisfied by two surfaces being equally wrong. The transcript row is
    /// asserted FIRST: without its marks there would be nothing for the banner to
    /// agree with, and the test would pin nothing.
    #[test]
    fn the_banner_marks_a_marker_line_query_exactly_as_the_transcript_does() {
        let (width, height) = BANNER_PANE;
        let inner_width = width - 2;
        let mut jsonl = String::new();
        for turn in 0..MARKED_BANNER_TURNS {
            jsonl.push_str(&format!(
                r#"{{"type":"user","sessionId":"sess-normal-1","cwd":"/Users/me/project-alpha","timestamp":"2026-07-04T10:00:00.000Z","message":{{"role":"user","content":"ask number {turn}"}}}}"#
            ));
            jsonl.push('\n');
            jsonl.push_str(&format!(
                r#"{{"type":"assistant","sessionId":"sess-normal-1","cwd":"/Users/me/project-alpha","timestamp":"2026-07-04T10:00:05.000Z","message":{{"role":"assistant","content":"answer number {turn}"}}}}"#
            ));
            jsonl.push('\n');
        }
        // Through the real query funnel: it is what compiles the per-atom finders the
        // marks are derived from, so a query assigned straight to the field would mark
        // nothing. The label carries the word too, or the filter would drop the only
        // row and leave the pane with nothing to draw.
        let mut app = banner_app_over_labelled(
            &jsonl,
            "banner-marked",
            &format!("{BANNER_MARKER_QUERY} answered every ask"),
        );
        app.push_query_str(BANNER_MARKER_QUERY);
        assert!(
            app.selected.is_some(),
            "the query must keep the session on the board, or nothing is previewed"
        );

        // An EARLY claude turn, probed at the screen row the WRAPPER starts its marker
        // on — read off the same cached prefix map the pane draws by.
        let claude_rows = marker_rows(&mut app, inner_width, "\u{25cf} claude");
        assert_eq!(claude_rows.len(), MARKED_BANNER_TURNS);
        let probe = app
            .preview_rows_above(inner_width, claude_rows[1])
            .expect("a marker row inside the transcript");
        // One frame FIRST, to consume the match jump the query change armed: it is a
        // one-shot that would otherwise override the probe's offset on exactly the
        // frame being measured.
        let _ = preview_buffer(&mut app, width, height);
        app.preview_follow_bottom = false;
        app.preview_scroll = u32::try_from(probe).expect("a small test offset");

        let drawn = preview_buffer(&mut app, width, height);
        assert_eq!(
            usize::try_from(app.preview_scroll).expect("a small resolved offset"),
            probe,
            "the pane must resolve to the probed row, or the marker is not its top row"
        );
        // Row 0 inside the borders is the pinned banner; row 1 is the transcript's
        // first drawn row (see `preview_split`), which at this offset is the very
        // marker line the banner reused.
        let banner = row_cells(&drawn, 1, width);
        let top = row_cells(&drawn, 2, width);
        assert_eq!(
            cells_text(&banner),
            cells_text(&top),
            "the pinned row and the transcript's first row must be the same line here"
        );

        let expected = query_columns(&top, BANNER_MARKER_QUERY);
        assert_eq!(
            marked_columns(&top),
            expected,
            "the transcript must mark the query on the marker line, or the banner has \
             nothing to agree with: {:?}",
            cells_text(&top)
        );
        assert_eq!(
            marked_columns(&banner),
            expected,
            "and the pinned banner must mark the same columns — one line, ONE \
             appearance: {:?}",
            cells_text(&banner)
        );
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
        // two passes into two rects. A pane that reserves NO banner must still
        // paint exactly the single `Paragraph::new(text).block(block)` it replaced
        // — rebuilt here from ratatui's own widgets and compared cell by cell.
        //
        // Every selected session reserves the row now, so the one banner-less pane
        // that still draws a transcript is an IN-FLIGHT quick reply: its inline
        // echo turns take the banner's place, and the reference is the transcript
        // with that same tail appended.
        use super::super::app::Sending;

        let (width, height) = BANNER_PANE;
        let mut app = banner_app(None);
        app.sending = vec![Sending {
            session_id: "sess-normal-1".to_string(),
            message: "please summarize this".to_string(),
            baseline_msg_count: app.sessions[0].msg_count,
        }];
        assert!(
            preview_banner(&app).is_none(),
            "an in-flight reply must reserve no banner, or this is the banner case"
        );
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
        let mut text = app.preview_text(width - 2);
        text.lines
            .extend(sending_tail(&app, width - 2).expect("the reply is still in flight"));
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
        // cell-by-cell by the scrollbar-geometry tests above — so nothing here is
        // left unasserted.
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
            pid: None,
            started_at_ms: None,
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
                    pid: None,
                    started_at_ms: None,
                },
            );
            sessions.push(session);
        }
        let mut app = App::new(sessions, Scope::All, PathBuf::from("/tmp/launch"));
        app.set_reported_agents(reported, None);
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
    ///
    /// `&mut App` because the query is a [`TextArea`](ratatui_textarea::TextArea)
    /// whose caret style is set per frame; [`render_search`] takes one for that
    /// reason, and [`render`] already holds one.
    fn drawn_search(app: &mut App, width: u16) -> ratatui::buffer::Buffer {
        drawn_search_at(app, width, 1)
    }

    /// [`drawn_search`] into an area of an arbitrary HEIGHT.
    ///
    /// The real board gives this row exactly one line, so every phase test uses
    /// the `1` above. A height of 2 exists for ONE purpose: handing the widget
    /// room it could spill into, so that "the search row stays one row" is a thing
    /// the buffer can actually disagree with rather than a guarantee of the
    /// viewport (see `a_pasted_newline_never_opens_a_second_query_line`).
    fn drawn_search_at(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height))
            .expect("build an in-memory test terminal");
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
    ///
    /// The query is TYPED through [`App::push_query_str`] rather than written into
    /// the widget, so these render tests drive the same seam a paste does and the
    /// caret ends up where the app really leaves it — at the end of the line.
    fn cursor_board(tick: u64) -> App {
        let mut app = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp/launch"));
        app.push_query_str(CURSOR_QUERY);
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

    /// Whether the cell at `x` on the drawn search line carries `REVERSED` — the
    /// caret, now that the query is a [`TextArea`](ratatui_textarea::TextArea).
    ///
    /// The widget owns its caret and pulses it by STYLE: it paints a cell at the
    /// caret column in both phases and differs only in the modifier, so the cell
    /// is read through `modifier` rather than through `symbol()`. Reading the
    /// glyph would be blind to the whole pulse.
    fn cell_is_reversed(buffer: &ratatui::buffer::Buffer, x: u16) -> bool {
        buffer
            .cell((x, 0))
            .expect("the cursor column must be inside the drawn line")
            .modifier
            .contains(Modifier::REVERSED)
    }

    /// The search cursor is drawn in the pulse's VISIBLE phase — asserted as a
    /// REVERSED CELL in the buffer, which is what the widget actually paints.
    ///
    /// Still a BUFFER assertion and not a call-count one, for the original
    /// reason: this cursor once shipped carrying the ANSI blink attribute and
    /// never blinked, so only the cells a terminal would really paint are
    /// evidence. `REVERSED` is a `Modifier` this board already depends on being
    /// honoured — the list's selection highlight is drawn with it.
    #[test]
    fn render_search_draws_the_cursor_in_the_pulses_visible_phase() {
        let mut app = cursor_board(0);
        assert!(blink_visible(app.tick), "tick 0 must be a visible phase");

        let buffer = drawn_search(&mut app, SEARCH_LINE_WIDTH);
        let x = cursor_column(&buffer, SEARCH_LINE_WIDTH);

        assert!(
            cell_is_reversed(&buffer, x),
            "the cell right after the query must be REVERSED in the visible phase; \
             drawn line: {:?}",
            full_row_text(&buffer, 0, SEARCH_LINE_WIDTH)
        );
    }

    /// The other half of the pulse: at `BLINK_TICKS` the caret's cell is drawn
    /// PLAIN. Without this the "pulse" is a permanently-lit cursor.
    #[test]
    fn render_search_hides_the_cursor_in_the_pulses_hidden_phase() {
        let mut app = cursor_board(BLINK_TICKS);
        assert!(
            !blink_visible(app.tick),
            "tick {BLINK_TICKS} must be a hidden phase"
        );

        let buffer = drawn_search(&mut app, SEARCH_LINE_WIDTH);
        let x = cursor_column(&buffer, SEARCH_LINE_WIDTH);

        assert!(
            !cell_is_reversed(&buffer, x),
            "the caret's cell must carry NO reverse-video in the hidden phase; \
             drawn line: {:?}",
            full_row_text(&buffer, 0, SEARCH_LINE_WIDTH)
        );
    }

    /// The anti-shift pin: the query must not move as the cursor pulses.
    ///
    /// Stronger than it could be against the old glyph cursor. That one was the
    /// LAST span on the line, so blanking it and dropping it painted identical
    /// cells and this test could not tell them apart. The widget's caret is a
    /// STYLE toggle over text it draws either way, so the whole row's TEXT is now
    /// assertable as byte-identical across both phases — which subsumes the query
    /// column and pins the caret's own column with it.
    #[test]
    fn render_search_keeps_the_query_column_stable_across_both_pulse_phases() {
        let rows: Vec<(u16, String)> = [0, BLINK_TICKS]
            .into_iter()
            .map(|tick| {
                let buffer = drawn_search(&mut cursor_board(tick), SEARCH_LINE_WIDTH);
                (
                    column_of(&buffer, 0, SEARCH_LINE_WIDTH, CURSOR_QUERY),
                    full_row_text(&buffer, 0, SEARCH_LINE_WIDTH),
                )
            })
            .collect();

        assert_eq!(
            rows[0].0, rows[1].0,
            "the query must start at the SAME column in the visible (tick 0, col {}) \
             and hidden (tick {BLINK_TICKS}, col {}) phases",
            rows[0].0, rows[1].0
        );
        assert_eq!(
            rows[0].1, rows[1].1,
            "and the pulse must not change the row's TEXT at all — it is a style toggle"
        );
    }

    /// Deliver `text` to `app` as ONE bracketed-terminal paste, through the
    /// board's real router.
    ///
    /// Driven through [`update::handle_event`](crate::tui::update::handle_event)
    /// rather than [`App::push_query_str`] on purpose: the single-line guard lives
    /// at the PASTE call site (`update::flatten_for_query`), not inside the
    /// mutator, so handing the mutator a raw newline would pin the WIDGET's
    /// behaviour instead of the board's. The store is a placeholder — a paste
    /// never reloads.
    fn paste_onto_board(app: &mut App, text: &str) {
        let _ = crate::tui::update::handle_event(
            app,
            crate::watch::AppEvent::Input(crossterm::event::Event::Paste(text.to_string())),
            &mut crate::store::SessionStore::new(Path::new("/tmp")),
        );
    }

    /// The SINGLE-LINE invariant, driven down the one path that can break it.
    ///
    /// [`App::query`] reads `query_input.lines()[0]` and nothing else, so a second
    /// line is text the filter cannot see, the search row cannot draw and no key
    /// can delete — swallowed silently rather than visibly wrong. The guard is
    /// load-bearing rather than belt-and-braces: no KEY can produce a newline here
    /// (`Enter` is `Action::Resume`), a paste can, and `TextArea::insert_str` does
    /// split on `\n`.
    ///
    /// Both halves are asserted, because either alone is satisfiable while the
    /// other is broken: the widget holds ONE line, AND the row it paints stays one
    /// row even when handed a second to spill into.
    #[test]
    fn a_pasted_newline_never_opens_a_second_query_line() {
        const ROOM: (u16, u16) = (40, 2);
        let (width, height) = ROOM;

        let mut app = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp/launch"));
        // Every line-ending shape a bracketed paste carries: LF, CRLF, and the lone
        // CR that is the classic embedded newline inside one.
        paste_onto_board(&mut app, "one\ntwo\r\nthree\rfour");

        assert_eq!(
            app.query_input.lines().len(),
            1,
            "a pasted newline must not open a second line; lines: {:?}",
            app.query_input.lines()
        );
        assert_eq!(
            app.query(),
            "one two three four",
            "and nothing pasted may be dropped — each line becomes its own atom"
        );

        let buffer = drawn_search_at(&mut app, width, height);
        assert_eq!(
            full_row_text(&buffer, 0, width),
            "search: one two three four",
            "the WHOLE query draws on the first row"
        );
        assert_eq!(
            full_row_text(&buffer, 1, width),
            "",
            "and nothing spills onto the second row this area deliberately offers"
        );
    }

    /// The clipping defect the migration fixes, pinned at the buffer.
    ///
    /// The old search line assembled a `Line` and drew it as an unscrolled
    /// `Paragraph`, so a query wider than the row was CUT at the right edge — and
    /// the caret, appended after the text, went first. That is reachable rather
    /// than theoretical: one paste puts up to `PASTE_MAX_CHARS` here.
    ///
    /// The widget scrolls horizontally under `WrapMode::None` to follow its caret,
    /// so the row shows the query's TAIL. Asserted as tail-present/head-absent
    /// rather than against a literal row, so a change to the label width or the
    /// terminal size cannot turn this into a transcription test.
    #[test]
    fn a_query_wider_than_the_row_keeps_its_tail_and_the_caret_on_screen() {
        // Ends that name themselves, so a failure says WHICH end survived.
        const LONG_QUERY: &str = "HEAD-abcdefghijklmnopqrstuvwxyz-0123456789-TAIL";
        const NARROW: u16 = 20;

        let mut app = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp/launch"));
        app.push_query_str(LONG_QUERY);
        assert!(
            LONG_QUERY.len() > usize::from(NARROW),
            "premise: the query cannot fit the row, so something HAS to be cut"
        );
        assert!(
            blink_visible(app.tick),
            "premise: the caret is in a lit phase"
        );

        let buffer = drawn_search_at(&mut app, NARROW, 1);
        let row = full_row_text(&buffer, 0, NARROW);

        assert!(
            row.ends_with("TAIL"),
            "the row must draw the query's TAIL, which is where the caret is: {row:?}"
        );
        assert!(
            !row.contains("HEAD"),
            "and the head must have scrolled off — a row still showing it is a row \
             that clipped the caret instead: {row:?}"
        );
        assert!(
            (0..NARROW).any(|x| cell_is_reversed(&buffer, x)),
            "the caret itself must stay on screen; it is what the old Paragraph lost \
             first: {row:?}"
        );
    }

    /// A long query stays ONE visual row: it scrolls, it does not WRAP.
    ///
    /// The sibling above pins that the row shows the query's TAIL, and it passes
    /// under a wrap mode too — wrapping also keeps the caret in view, by scrolling
    /// the viewport DOWN to the caret's wrapped row instead of sideways. So the
    /// tail assertion alone does not pin `WrapMode::None`; this does.
    ///
    /// Why it matters beyond tidiness: the board hands this row exactly ONE line
    /// ([`render`]'s layout), so a wrapped query has rows that no viewport will
    /// ever show — the same swallowed-text failure as a second `lines()` entry,
    /// reached by a different route. Asserted with the deliberate extra row
    /// [`drawn_search_at`] exists to give, so the buffer can disagree rather than
    /// the viewport making it true by construction.
    #[test]
    fn a_query_wider_than_the_row_scrolls_instead_of_wrapping_onto_a_second_row() {
        const LONG_QUERY: &str = "HEAD-abcdefghijklmnopqrstuvwxyz-0123456789-TAIL";
        const NARROW: u16 = 20;

        let mut app = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp/launch"));
        app.push_query_str(LONG_QUERY);
        assert!(
            LONG_QUERY.len() > usize::from(NARROW - SEARCH_LABEL_WIDTH),
            "premise: the query is wider than the column left for it, so a wrapping \
             widget WOULD need a second row"
        );

        let buffer = drawn_search_at(&mut app, NARROW, 2);

        assert_eq!(
            full_row_text(&buffer, 1, NARROW),
            "",
            "a query wider than the row must SCROLL, not wrap: text on the second \
             row means the one-line area the board really gives this widget would \
             hide it"
        );
        assert!(
            full_row_text(&buffer, 0, NARROW).ends_with("TAIL"),
            "and the one row it does occupy is still the tail — a blank second row \
             must not be bought by drawing nothing at all"
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

    /// Whether row `y` of `buffer` carries a REVERSED cell anywhere on it — how
    /// the search line's caret is read now that the query is a
    /// [`TextArea`](ratatui_textarea::TextArea) and the pulse toggles a style
    /// rather than swapping a glyph.
    ///
    /// Safe to ask of the SEARCH row specifically: the only other `REVERSED` on
    /// this board is the list's selection highlight, which is drawn in the body,
    /// never on this one-row line.
    fn row_has_reversed_cell(buffer: &ratatui::buffer::Buffer, y: u16, width: u16) -> bool {
        (0..width).any(|x| {
            buffer
                .cell((x, y))
                .is_some_and(|cell| cell.modifier.contains(Modifier::REVERSED))
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
    /// The two are read through DIFFERENT properties because they live on
    /// different surfaces, not because they pulse differently — both are now
    /// STYLE-ONLY toggles that leave their line's text byte-identical. The caret
    /// gains and loses `REVERSED` (the widget owns it and never swaps a glyph),
    /// while the dot holds its glyph and swaps COLOR, which is what keeps a URL
    /// sharing its row from flickering (see `pulse_color`). So "in phase" here
    /// reads: the caret is reversed exactly when the dot carries its BASE color,
    /// and plain exactly when the dot carries its dim partner.
    #[test]
    fn the_search_cursor_and_an_active_badge_dot_pulse_in_phase() {
        let (width, height) = FULL_BOARD_SIZE;

        for (tick, on) in [(0, true), (BLINK_TICKS, false)] {
            let mut app = badge_board();
            app.tick = tick;

            let buffer = drawn_board(&mut app, width, height);
            // `render`'s layout is header(1) | body(fill) | search(1) | help(1).
            let search_y = height - 2;
            let cursor_drawn = row_has_reversed_cell(&buffer, search_y, width);
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
            "h hidden",
            "r reload",
            "y copy session ID",
        ] {
            assert!(
                text.contains(needle),
                "the which-key hint must list {needle:?}; drawn: {text:?}"
            );
        }
    }

    /// The hint's own column budget: its LONGEST form must still fit an
    /// 80-column terminal, since the help row is truncated rather than wrapped
    /// and the tail carries the `y copy session ID` verb.
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
        let sel = modal_list_row("planner", None, Some("plans work"), true);
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
        let unsel = modal_list_row("planner", None, None, false);
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

    /// What each model-picker row draws after its label, stated directly: a SET
    /// effort on any model row (highlighted or not) in the pick's magenta, the dim
    /// unset stop on the HIGHLIGHTED model row only, and nothing at all on the
    /// `default` row or an agent row, highlighted or not.
    #[test]
    fn modal_effort_span_shows_a_set_effort_and_marks_the_highlighted_unset_stop() {
        let text = |span: Option<Span<'static>>| span.map(|s| s.content.into_owned());
        let high = ModalAction::SetModel(Some(ModelPick {
            model: "opus".to_string(),
            effort: Some("high"),
        }));
        let unset = ModalAction::SetModel(Some(ModelPick::new("opus")));

        for selected in [true, false] {
            let span = modal_effort_span(&high, selected).expect("a set effort always shows");
            assert_eq!(span.content, " · high");
            assert_eq!(
                span.style.fg,
                Some(Color::Magenta),
                "drawn like the header pick"
            );
        }
        let marker = modal_effort_span(&unset, true).expect("the highlighted unset stop shows");
        assert_eq!(marker.content, " · default effort");
        assert!(marker.style.add_modifier.contains(Modifier::DIM));
        assert_eq!(
            text(modal_effort_span(&unset, false)),
            None,
            "unset, not highlighted"
        );

        for (action, what) in [
            (ModalAction::SetModel(None), "the default row"),
            (
                ModalAction::New(Some("planner".to_string())),
                "an agent row",
            ),
            (ModalAction::New(None), "the no-agent row"),
        ] {
            for selected in [true, false] {
                assert_eq!(
                    text(modal_effort_span(&action, selected)),
                    None,
                    "{what} never draws an effort (selected: {selected})"
                );
            }
        }
    }

    /// The model picker as DRAWN: its prompt names `←`/`→`, the highlighted model
    /// row shows its effort inline, and the `default` row stays as it was.
    #[test]
    fn the_model_picker_draws_the_highlighted_rows_effort_and_names_the_arrows() {
        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        crate::tui::compose::open_background(&mut app, None);
        app.open_model_picker();
        let on_default = drawn_screen(&mut app, 80, 24);
        assert!(
            on_default.contains("(←/→ effort)"),
            "the prompt names the keys that set the effort:\n{on_default}"
        );
        assert!(
            !on_default.contains("default effort") && !on_default.contains(" · high"),
            "the default row carries no effort, and no other row has one set:\n{on_default}"
        );

        app.modal_next(); // fable, unset
        let unset = drawn_screen(&mut app, 80, 24);
        assert!(
            unset.contains("› fable · default effort"),
            "the highlighted row marks the unset stop:\n{unset}"
        );
        app.adjust_modal_effort(true);
        app.adjust_modal_effort(true);
        app.adjust_modal_effort(true);
        let high = drawn_screen(&mut app, 80, 24);
        assert!(
            high.contains("› fable · high"),
            "the highlighted row shows its effort inline:\n{high}"
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

    /// The two pickers share the `List` layout but not their verbs, so each draws
    /// its OWN footer. The model picker's names the effort arrows and `Enter set`,
    /// and must not borrow the agent picker's `Enter draft` / `^O`: `Enter` drafts
    /// nothing there, and `Ctrl-O` is inert.
    #[test]
    fn the_model_picker_draws_its_own_footer_not_the_agent_pickers() {
        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        crate::tui::compose::open_background(&mut app, None);
        app.open_model_picker();
        let drawn = drawn_screen(&mut app, 80, 24);
        assert!(
            drawn.contains("↑/↓ choose · ←/→ effort · Enter set · Esc cancel"),
            "the model picker's footer names its own keys:\n{drawn}"
        );
        assert!(
            !drawn.contains("Enter draft") && !drawn.contains("^O"),
            "the model picker must not advertise the agent picker's verbs:\n{drawn}"
        );
    }

    /// The agent picker's footer, pinned whole: giving the model picker a footer of
    /// its own must leave this one exactly as it was.
    #[test]
    fn the_agent_picker_keeps_its_draft_and_interactive_footer() {
        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        app.open_agent_picker(vec![DefinedAgent {
            name: "planner".to_string(),
            description: None,
        }]);
        let drawn = drawn_screen(&mut app, 80, 24);
        assert!(
            drawn.contains("↑/↓ choose · Enter draft · ^O interactive · Esc cancel"),
            "the agent picker's footer names both of its verbs:\n{drawn}"
        );
        assert!(
            !drawn.contains("Enter set"),
            "the agent picker must not advertise the model picker's verb:\n{drawn}"
        );
    }

    /// Both `Row` modals keep the button-strip footer they drew before the footer
    /// moved onto the modal.
    #[test]
    fn the_row_modals_keep_the_confirm_footer() {
        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        let footer = "←/→ choose · Enter confirm · Esc cancel";

        app.open_live_choice("sb-running".to_string());
        let live = drawn_screen(&mut app, 80, 24);
        assert!(
            live.contains(footer),
            "the running-session choice keeps its footer:\n{live}"
        );

        app.close_modal();
        app.open_delete_confirm();
        assert!(app.modal.is_some(), "the delete confirm opens");
        let delete = drawn_screen(&mut app, 80, 24);
        assert!(
            delete.contains(footer),
            "the delete confirm keeps its footer:\n{delete}"
        );
    }

    /// Flatten a whole rendered board to one searchable string, rows joined by
    /// newlines. The `List`-modal viewport tests below look for a SELECTED row's
    /// `› label` — a two-token needle the highlight glyph makes specific to the
    /// modal — so they need the screen, not one row.
    fn drawn_screen(app: &mut App, width: u16, height: u16) -> String {
        let buffer = drawn_board(app, width, height);
        (0..height)
            .map(|y| full_row_text(&buffer, y, width))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The `› ` needle that says a choice is BOTH drawn and highlighted — the same
    /// marker [`modal_list_row`] gives a selected row.
    fn selected_needle(app: &App) -> String {
        let modal = app.modal.as_ref().expect("a modal is open");
        format!("\u{203a} {}", modal.choices[modal.selected].label)
    }

    /// The viewport's whole point, swept over terminal height the way
    /// `the_disclosing_delete_confirm_costs_one_row_and_keeps_cancel_default` sweeps
    /// it: a `List` modal's SELECTED row is drawn at every height that has room for
    /// a list row at all, not only on a terminal tall enough for the whole list.
    ///
    /// The picker's LAST row is selected, reached the way a user reaches it — one
    /// `modal_prev` off the pre-highlight, which `cycle_modal` wraps to the end — so
    /// the row under test is the one furthest from where a top-down draw starts. That
    /// is exactly the row the old render lost: `centered_rect` clamps the box and
    /// nothing scrolled, so on a short terminal the tail simply was not painted.
    ///
    /// The needle is derived from the modal rather than hard-coded, so the sweep
    /// keeps testing the last row whatever the alias set is — which now varies at
    /// RUNTIME, since the picker offers what the installed `claude` accepts and
    /// falls back to the `app::MODEL_ALIASES` seed only until that probe lands. This
    /// case is the SEED one (no probe delivered); the probed one is pinned below.
    #[test]
    fn a_short_terminal_still_draws_the_model_pickers_selected_row() {
        /// Terminal heights the sweep covers. Seven is the shortest box that has a
        /// single list row at all (one message row + `MODAL_LIST_CHROME_ROWS` +
        /// `MODAL_BORDER_ROWS` = six rows of chrome), and twenty is comfortably
        /// taller than the whole picker, so the sweep spans "scrolled to one row" to
        /// "not scrolled at all".
        const SWEEP: std::ops::RangeInclusive<u16> = 7..=20;

        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        crate::tui::compose::open_background(&mut app, None);
        app.open_model_picker();
        // Wrap onto the last alias — the row a top-down draw reaches last.
        app.modal_prev();
        let needle = selected_needle(&app);
        let last = app.modal.as_ref().expect("the picker is open").selected;
        assert_eq!(
            last,
            app.modal
                .as_ref()
                .expect("the picker is open")
                .choices
                .len()
                - 1,
            "the fixture must be on the LAST row"
        );

        for height in SWEEP {
            let screen = drawn_screen(&mut app, 80, height);
            assert!(
                screen.contains(&needle),
                "a {height}-row terminal must still draw the selected {needle:?}; \
                 screen:\n{screen}"
            );
        }
    }

    /// The same sweep against the PROBED set, with `opusplan` pinned by name.
    ///
    /// Two things make this a different claim from the seed sweep above rather than
    /// a copy of it. The probed list is TEN rows (nine aliases plus the synthetic
    /// clear row) against the seed's six, so every terminal in the sweep below
    /// sixteen rows genuinely scrolls. And `opusplan` — the alias this whole feature
    /// exists to surface, since `claude --help` hides it — is LAST in the binary's
    /// own array order, which the picker offers verbatim. It is therefore the FIRST
    /// row a top-down draw with no viewport would lose, which is exactly why its
    /// reachability is worth pinning by name and not only by "the selected row".
    #[test]
    fn the_probed_pickers_last_row_opusplan_is_reachable_on_a_short_terminal() {
        /// Terminal heights the sweep covers — the seed sweep's range, so the two
        /// cases are compared over the same terminals. Seven is the shortest box
        /// with a single list row at all; twenty is taller than the whole picker.
        const SWEEP: std::ops::RangeInclusive<u16> = 7..=20;
        /// The alias array the installed `claude 2.1.233` accepts, in wire order —
        /// stated here so this test needs no 290 MB binary. `opusplan` last is the
        /// property under test, not an incidental detail of the fixture.
        const PROBED: [&str; 9] = [
            "sonnet",
            "opus",
            "haiku",
            "fable",
            "best",
            "sonnet[1m]",
            "opus[1m]",
            "fable[1m]",
            "opusplan",
        ];

        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        app.set_model_aliases(PROBED.iter().map(|a| (*a).to_string()).collect());
        crate::tui::compose::open_background(&mut app, None);
        app.open_model_picker();
        // Wrap onto the last alias — the row a top-down draw reaches last.
        app.modal_prev();

        let modal = app.modal.as_ref().expect("the picker is open");
        assert_eq!(
            modal.choices.len(),
            PROBED.len() + 1,
            "the probed picker is nine aliases plus the synthetic clear row"
        );
        assert_eq!(
            modal.choices[modal.selected].label, "opusplan",
            "opusplan must be the LAST row — that is what makes it the first one a \
             viewport-less draw loses"
        );
        let needle = selected_needle(&app);

        for height in SWEEP {
            let screen = drawn_screen(&mut app, 80, height);
            assert!(
                screen.contains(&needle),
                "a {height}-row terminal must still reach {needle:?}; screen:\n{screen}"
            );
        }
    }

    /// A probe that returns MORE aliases than the window holds must SCROLL, not clip.
    ///
    /// This stopped being hypothetical when the row count became upstream data:
    /// nothing in snapback bounds how many aliases a future `claude` ships, and the
    /// picker offers every one of them. Same shape as the agent-picker cap test, but
    /// it has to be stated for the model picker too — that one is bounded by the
    /// user's own agent files, this one by another program's release notes.
    #[test]
    fn a_probe_longer_than_the_cap_scrolls_the_model_picker_rather_than_clipping() {
        /// Aliases the stated probe returns — comfortably past
        /// `MODAL_LIST_MAX_ROWS`, so the CAP is what bounds the box.
        const ALIASES: usize = 20;
        /// A terminal tall enough to draw all of them, so nothing here is the
        /// terminal clamp in disguise.
        const TALL: u16 = 40;

        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        app.set_model_aliases((0..ALIASES).map(|i| format!("model-{i:02}")).collect());
        crate::tui::compose::open_background(&mut app, None);
        app.open_model_picker();
        // +1 for the synthetic default row, which sends no model.
        let total = ALIASES + 1;
        let off_window = total - usize::from(MODAL_LIST_MAX_ROWS);
        let last = format!("model-{:02}", ALIASES - 1);

        // Opened at the top: the tail is off-window and the LOWER spacer says how
        // much of it there is.
        let screen = drawn_screen(&mut app, 80, TALL);
        assert!(
            !screen.contains(&last),
            "the cap must stop the box growing with the probe's answer; screen:\n{screen}"
        );
        assert!(
            screen.contains(&format!("{MODAL_MORE_BELOW} {off_window} more")),
            "and the spacer must disclose how many aliases are below; screen:\n{screen}"
        );

        // Wrap onto the last alias: it must be REACHABLE rather than clipped away.
        app.modal_prev();
        let needle = selected_needle(&app);
        let screen = drawn_screen(&mut app, 80, TALL);
        assert!(
            screen.contains(&needle),
            "the window must follow the selection onto {last}; screen:\n{screen}"
        );
        assert!(
            screen.contains(&format!("{MODAL_MORE_ABOVE} {off_window} more")),
            "and disclose the aliases now above it; screen:\n{screen}"
        );
    }

    /// The other half: a list LONGER than the cap is bounded on a tall terminal
    /// too, and says so.
    ///
    /// The agent picker draws one row per user-defined agent, so it is unbounded by
    /// data — twenty agents on a forty-row terminal used to draw a twenty-six-row box
    /// over a board it is meant to overlay. `MODAL_LIST_MAX_ROWS` caps the window and
    /// the spacer rows carry the count that is off-window, so the rows beyond it are
    /// discoverable rather than merely absent.
    #[test]
    fn a_long_agent_picker_is_capped_and_says_how_many_rows_are_off_window() {
        /// Agents in the fixture — comfortably past `MODAL_LIST_MAX_ROWS`, so the cap
        /// is the thing under test rather than the terminal's height.
        const AGENTS: usize = 20;
        /// A terminal tall enough to draw all of them, so nothing here is the
        /// terminal clamp in disguise.
        const TALL: u16 = 40;

        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        app.open_agent_picker(
            (0..AGENTS)
                .map(|i| DefinedAgent {
                    name: format!("agent-{i:02}"),
                    description: None,
                })
                .collect(),
        );
        // +1 for the synthetic "default (no agent)" row.
        let total = AGENTS + 1;
        let off_window = total - usize::from(MODAL_LIST_MAX_ROWS);

        // A marker is `<arrow> <count> more`, so ask for it by SHAPE over every
        // count it could carry. A bare `<arrow> ` needle would also match the
        // picker's own `↑/↓ choose` footer and quietly never be able to go red.
        let marker = |screen: &str, arrow: &str| {
            (1..=total).any(|n| screen.contains(&format!("{arrow} {n} more")))
        };

        // Opened at the top: the tail is off-window and the LOWER spacer says how
        // much of it there is.
        let screen = drawn_screen(&mut app, 80, TALL);
        assert!(
            !screen.contains("agent-19"),
            "the cap must stop the box growing with the agent count; screen:\n{screen}"
        );
        assert!(
            screen.contains(&format!("{MODAL_MORE_BELOW} {off_window} more")),
            "and the spacer must disclose how many rows are below; screen:\n{screen}"
        );
        assert!(
            !marker(&screen, MODAL_MORE_ABOVE),
            "nothing is above the window at the top; screen:\n{screen}"
        );

        // Wrap onto the last agent: the window follows the selection, and now it is
        // the HEAD of the list that is off-window.
        app.modal_prev();
        let needle = selected_needle(&app);
        let screen = drawn_screen(&mut app, 80, TALL);
        assert!(
            screen.contains(&needle),
            "the window must follow the selection to the last row; screen:\n{screen}"
        );
        assert!(
            screen.contains(&format!("{MODAL_MORE_ABOVE} {off_window} more")),
            "and disclose the rows now above it; screen:\n{screen}"
        );
        assert!(
            !marker(&screen, MODAL_MORE_BELOW),
            "nothing is below the window at the bottom; screen:\n{screen}"
        );
    }

    /// The offset is KEPT between frames rather than re-derived, which is what makes
    /// the modal scroll like the board list instead of re-centring per keypress.
    ///
    /// Asserted through `Modal::scroll` after real renders, because that field is the
    /// state the render writes back (`App::scroll`'s idiom) — a test that only read
    /// the screen could not tell a kept offset from a recomputed one that happened to
    /// agree.
    #[test]
    fn the_modal_scroll_offset_is_written_back_and_then_left_alone() {
        /// Six rows of chrome plus four list rows, so the window is four of the
        /// picker's rows and moving within it must not scroll.
        const SHORT: u16 = 10;

        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        crate::tui::compose::open_background(&mut app, None);
        app.open_model_picker();
        let total = app
            .modal
            .as_ref()
            .expect("the picker is open")
            .choices
            .len();
        assert!(
            total > 4,
            "the fixture needs more rows than the 4-row window"
        );

        let scroll_now = |app: &App| app.modal.as_ref().expect("open").scroll;

        // Row 0 selected: nothing above it, so the window sits at the top.
        drawn_screen(&mut app, 80, SHORT);
        assert_eq!(scroll_now(&app), 0, "a top selection needs no scroll");

        // Step down INSIDE the window: the offset must not move.
        app.modal_next();
        drawn_screen(&mut app, 80, SHORT);
        assert_eq!(
            scroll_now(&app),
            0,
            "moving inside the window keeps it still"
        );

        // Step past the bottom: the offset moves by exactly one.
        for _ in 1..4 {
            app.modal_next();
        }
        drawn_screen(&mut app, 80, SHORT);
        assert_eq!(
            scroll_now(&app),
            1,
            "leaving the window scrolls the least that brings the row back"
        );

        // Down to the last row: the offset stops at `len - viewport`.
        for _ in 4..total - 1 {
            app.modal_next();
        }
        drawn_screen(&mut app, 80, SHORT);
        assert_eq!(
            scroll_now(&app),
            total - 4,
            "the bottom pins the window to max_scroll"
        );

        // One more press WRAPS to row 0 — a single keystroke crossing the whole
        // list, which is the move a window that only ever nudged by one would lose.
        app.modal_next();
        drawn_screen(&mut app, 80, SHORT);
        assert_eq!(scroll_now(&app), 0, "the wrap pins the window back to zero");
    }

    // --- the model picker's wrapped `default` row -------------------------

    /// Every terminal row's slice of a modal's INNER columns (the span
    /// `centered_rect` places the box in, minus its borders), trailing blanks
    /// trimmed. Reading only the box's own columns keeps the board's panes out of
    /// the text, so a line that runs INTO the border reads as clipped instead of
    /// running on into whatever the preview drew beside it.
    fn modal_inner_rows(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let buffer = drawn_board(app, width, height);
        let inner_x = usize::from((width - MODAL_WIDTH) / 2 + 1);
        let inner_w = usize::from(MODAL_INNER_WIDTH);
        (0..height)
            .map(|y| {
                full_row_text(&buffer, y, width)
                    .chars()
                    .skip(inner_x)
                    .take(inner_w)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// Whether any `↑ N more` / `↓ N more` marker is on `rows`, asked by SHAPE over
    /// every count it could carry. A bare arrow would also match the picker's own
    /// `↑/↓ choose` footer and could never go red.
    fn any_more_marker(rows: &[String], choices: usize) -> bool {
        let screen = rows.join("\n");
        [MODAL_MORE_ABOVE, MODAL_MORE_BELOW]
            .iter()
            .any(|arrow| (1..=choices).any(|n| screen.contains(&format!("{arrow} {n} more"))))
    }

    /// The claim the wrapped-default-row cases below share, checked on the drawn
    /// screen, for the picker `app` has a compose open for.
    ///
    /// Row 0 draws EXACTLY `expected`: its first line beside `› <label>`, each
    /// further line at the hanging indent under the description's first column,
    /// and the next choice directly beneath the last one. So the whole description
    /// is on screen, and the row is `expected.len()` lines tall.
    ///
    /// Then the box has to COUNT those lines, checked where a user would notice.
    /// At the terminal height the box needs for every choice plus row 0's extra
    /// lines, nothing is off-window. One row shorter, the lower spacer reads
    /// `↓ 1 more`. A box that counted row 0 as a single line would fit one row
    /// sooner, so that marker would never appear.
    fn assert_default_row_wraps_whole(mut app: App, expected: &[&str]) {
        /// Wide enough for the whole 62-column modal, so nothing here is the
        /// terminal's own width clamp.
        const WIDTH: u16 = 80;
        /// Tall enough to draw the whole picker with room to spare.
        const TALL: u16 = 24;

        app.open_model_picker();
        let modal = app.modal.clone().expect("the picker is open");
        assert_eq!(modal.selected, 0, "no pick highlights the default row");
        assert_eq!(
            Some(expected.join(" ")),
            modal.choices[0].description,
            "the fixture must spell row 0's WHOLE description, split where it wraps"
        );

        let rows = modal_inner_rows(&mut app, WIDTH, TALL);
        let screen = rows.join("\n");
        let head = format!("\u{203a} {}  ", modal.choices[0].label);
        let top = rows
            .iter()
            .position(|row| row.starts_with(&head))
            .unwrap_or_else(|| panic!("the selected default row is drawn; screen:\n{screen}"));
        assert_eq!(
            rows[top],
            format!("{head}{}", expected[0]),
            "the first chunk trails the label; screen:\n{screen}"
        );
        let indent = " ".repeat(head.chars().count());
        for (k, line) in expected.iter().enumerate().skip(1) {
            assert_eq!(
                rows[top + k],
                format!("{indent}{line}"),
                "continuation {k} sits at the description's column; screen:\n{screen}"
            );
        }
        assert!(
            rows[top + expected.len()].starts_with(&format!("  {}", modal.choices[1].label)),
            "the next choice follows row 0's last line directly, so the row is {} \
             lines tall; screen:\n{screen}",
            expected.len()
        );

        // The arithmetic: every choice is one line except row 0, which is as many
        // as its wrap produced.
        let list_lines = modal.choices.len() + expected.len() - 1;
        let message_rows = wrap_message(&modal.message, MODAL_INNER_WIDTH).len();
        let fits = u16::try_from(list_lines + message_rows).expect("a small modal")
            + MODAL_LIST_CHROME_ROWS
            + MODAL_BORDER_ROWS;
        let last = format!("  {}", modal.choices[modal.choices.len() - 1].label);

        let rows = modal_inner_rows(&mut app, WIDTH, fits);
        let screen = rows.join("\n");
        assert!(
            rows.iter().any(|row| row.starts_with(&last))
                && !any_more_marker(&rows, modal.choices.len()),
            "a {fits}-row terminal fits the whole picker, row 0's {} lines included; \
             screen:\n{screen}",
            expected.len()
        );
        let rows = modal_inner_rows(&mut app, WIDTH, fits - 1);
        let screen = rows.join("\n");
        assert!(
            screen.contains(&format!("{MODAL_MORE_BELOW} 1 more")),
            "one row shorter, the last choice is off-window and the spacer says so; \
             screen:\n{screen}"
        );
    }

    /// A board with a `Ctrl-N` DRAFT compose open and `settings_model` read from the
    /// user's settings — the picker the draft cases below open.
    fn draft_board(settings_model: Option<&str>) -> App {
        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        app.set_settings_model(settings_model.map(ToOwned::to_owned));
        crate::tui::compose::open_background(&mut app, None);
        app
    }

    /// A draft whose settings name `opus[1m]`: the row NAMES the value in its label
    /// (`default (opus[1m]) (settings)`), which leaves 27 columns beside it, and the
    /// explanation wraps onto three lines under it — `wrap_message`'s greedy
    /// whole-word fill of those 27 columns.
    #[test]
    fn the_model_pickers_default_row_wraps_a_settings_model_onto_three_lines() {
        assert_default_row_wraps_whole(
            draft_board(Some("opus[1m]")),
            &[
                "no --model \u{b7} a new session",
                "starts on the model your",
                "claude settings name",
            ],
        );
    }

    /// With no settings value the label is a bare `default`, which leaves 49
    /// columns, and the explanation still needs a second line — being SHORTER than
    /// the valued rows is what makes a hard-coded row height fail one case or
    /// another.
    #[test]
    fn the_model_pickers_default_row_wraps_even_with_no_settings_model() {
        assert_default_row_wraps_whole(
            draft_board(None),
            &[
                "no --model \u{b7} your claude settings name no model,",
                "so claude uses its own default",
            ],
        );
    }

    /// A full dated model id MOVES the wrap points: the label grows with the value,
    /// leaving 11 columns beside it, so the explanation falls one or two words to a
    /// line. The row's height is whatever the wrap produced — eight lines here — and
    /// the box counts all of them.
    #[test]
    fn a_long_settings_model_moves_the_default_rows_wrap_points() {
        assert_default_row_wraps_whole(
            draft_board(Some("claude-opus-4-1-20250805")),
            &[
                "no --model",
                "\u{b7} a new",
                "session",
                "starts on",
                "the model",
                "your claude",
                "settings",
                "name",
            ],
        );
    }

    /// A REPLY's default row names the session's own model — read off the preview
    /// the board drew, the newest answering model in the transcript — and wraps its
    /// explanation, effort caveat included, under that longer label.
    #[test]
    fn a_replys_default_row_names_the_session_model_and_wraps_under_it() {
        let mut app = App::new(
            vec![preview_fixture_row(
                "sbv-switch",
                "sess-model-switch-1.jsonl",
            )],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        crate::tui::compose::open(&mut app, "sbv-switch".to_string(), None);
        // The frame that shows the compose renders the preview first; the picker
        // reads the session's model off that render.
        let _ = drawn_board(&mut app, 80, 24);
        app.open_model_picker();
        assert_eq!(
            app.modal.take().expect("the picker is open").choices[0].label,
            "session's model (Sonnet 5)"
        );
        assert_default_row_wraps_whole(
            app,
            &[
                "no --model \u{b7} claude restores",
                "the model this session last",
                "answered with; the effort",
                "comes from your settings",
            ],
        );
    }

    /// Only the model picker's `default` row wraps. An agent picker row keeps its
    /// user-written blurb on ONE line, clipped at the border, however long it is.
    /// The box also counts each agent choice as one line.
    #[test]
    fn the_agent_pickers_rows_stay_one_line_however_long_the_description() {
        /// Far wider than the room beside the label, and its tail is a word that
        /// appears nowhere else on the board. Drawing it anywhere would mean the
        /// description wrapped instead of clipping.
        const LONG: &str = "Plans the work in careful detail before anyone writes a \
                            line of code and then hands off a checklist";
        /// Wide enough for the whole modal.
        const WIDTH: u16 = 80;
        /// Tall enough to draw the whole picker.
        const TALL: u16 = 24;

        let mut app = App::new(
            vec![sample_session()],
            Scope::All,
            PathBuf::from("/tmp/launch"),
        );
        app.open_agent_picker(vec![
            DefinedAgent {
                name: "planner".to_string(),
                description: Some(LONG.to_string()),
            },
            DefinedAgent {
                name: "reviewer".to_string(),
                description: None,
            },
        ]);
        let modal = app.modal.clone().expect("the picker is open");

        let rows = modal_inner_rows(&mut app, WIDTH, TALL);
        let screen = rows.join("\n");
        let planner = rows
            .iter()
            .position(|row| row.starts_with("  planner  Plans the work"))
            .unwrap_or_else(|| panic!("the planner row is drawn; screen:\n{screen}"));
        assert!(
            rows[planner + 1].starts_with("  reviewer"),
            "the next agent follows directly: the long blurb stayed on its row; \
             screen:\n{screen}"
        );
        assert!(
            !screen.contains("checklist"),
            "the blurb's tail is clipped at the border, not wrapped; screen:\n{screen}"
        );

        // The arithmetic: one line per choice, so the box fits exactly that many.
        let message_rows = wrap_message(&modal.message, MODAL_INNER_WIDTH).len();
        let fits = u16::try_from(modal.choices.len() + message_rows).expect("a small modal")
            + MODAL_LIST_CHROME_ROWS
            + MODAL_BORDER_ROWS;
        let rows = modal_inner_rows(&mut app, WIDTH, fits);
        let screen = rows.join("\n");
        assert!(
            rows.iter().any(|row| row.starts_with("  reviewer"))
                && !any_more_marker(&rows, modal.choices.len()),
            "a {fits}-row terminal fits every one-line agent row; screen:\n{screen}"
        );
        let rows = modal_inner_rows(&mut app, WIDTH, fits - 1);
        let screen = rows.join("\n");
        assert!(
            screen.contains(&format!("{MODAL_MORE_BELOW} 1 more")),
            "one row shorter, exactly one agent row is off-window; screen:\n{screen}"
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
    /// grows 10 → 11. `centered_rect` CLAMPS that height and `render_modal` draws a
    /// `Row` top-down with no vertical scroll — only the `List` layout scrolls
    /// ([`modal_list_window`]), and deliberately so: a button strip is fixed chrome
    /// the user cannot page through, where a picker's rows are data — so the extra
    /// row pushes the button strip
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
                let mut modal = Modal {
                    title: "delete session".to_string(),
                    message: message.clone(),
                    layout: ModalLayout::Row,
                    footer: super::super::app::MODAL_ROW_FOOTER,
                    choices: ["Delete this", lineage_label, "Cancel"]
                        .into_iter()
                        .map(|label| ModalChoice {
                            label: label.to_string(),
                            description: None,
                            wrap_description: false,
                            action: ModalAction::Cancel,
                        })
                        .collect(),
                    selected: 2,
                    session_id: None,
                    scroll: 0,
                };
                let mut terminal =
                    Terminal::new(TestBackend::new(w, 24)).expect("build a test terminal");
                terminal
                    .draw(|frame| render_modal(frame, &mut modal))
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
    /// the exact shape of the user's flicker report. Search-mode `the`, which
    /// every fixture label carries, so the query marks the rows without cutting
    /// any of them.
    fn linked_label_board() -> App {
        linked_label_board_with_query("the")
    }

    /// [`linked_label_board`] over an arbitrary query — the seam its two callers
    /// differ at, since one of them wants NO query (see
    /// [`url_on_the_pulsing_row`]).
    ///
    /// The query is typed through [`App::push_query_str`], the same seam a paste
    /// goes through, rather than written into the widget: a raw write would leave
    /// the matcher's pattern and the filtered list describing the EMPTY query
    /// while the search line drew a full one, so the marks under test would be
    /// drawn from state the board can never actually be in.
    fn linked_label_board_with_query(query: &str) -> App {
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
                    pid: None,
                    started_at_ms: None,
                },
            );
            sessions.push(session);
        }
        let mut app = App::new(sessions, Scope::All, PathBuf::from("/tmp/launch"));
        app.set_reported_agents(reported, None);
        // The URL row is the selected one, so its badge and its URL share a row
        // AND its preview pane is populated from the fixture on disk.
        app.selected = Some("sess-url".to_string());
        app.push_query_str(query);
        // A typed query ARMS the pane's jump onto its first match, and the very
        // next frame spends it. These fixtures are drawn twice (a same-tick
        // control, and both pulse phases), so the board has to be handed over
        // SETTLED — exactly as it would be by the time the next key arrives — or
        // the first render of a pair would differ from the second for a reason
        // that has nothing to do with the pulse.
        let _ = app.take_preview_match_jump();
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
    ///
    /// Built with NO query, unlike its sibling: the label below is rewritten after
    /// the board exists, so a live query would mark substrings of text the filter
    /// never saw, and the marks are beside the point here. The query is chosen at
    /// CONSTRUCTION rather than cleared afterwards because the query is a
    /// [`TextArea`](ratatui_textarea::TextArea) the board only ever appends to and
    /// deletes from — there is no clear-query mutator to reach for, deliberately
    /// (it is out of scope), and a test must not be the one caller that needs one.
    fn url_on_the_pulsing_row() -> App {
        let mut app = linked_label_board_with_query("");
        let working = app
            .sessions
            .iter_mut()
            .find(|s| s.session_id == "sess-working")
            .expect("the pulsing fixture row");
        working.label = format!("Assess {FIXTURE_URL} rather than rolling our own");
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
            background: false,
            has_agent_name: false,
            has_agent_setting: false,
            failed_task: None,
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

    /// The [`lineage_board`] fixture with the #80811 shape applied to the pair it
    /// already holds: the `bg` fork (newer, so it HEADS the lineage) carries an
    /// agent name with no binding, while the `ancestor` it forked from — the
    /// lineage ROOT — still carries one. The lone session is left untouched.
    fn downgraded_lineage_board() -> App {
        let mut sessions = vec![
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
        // The root: the FOREGROUND original, bound to an agent. It omits
        // `sessionKind` (DOMAIN.md) — the badge reads the root's binding, never its
        // kind, so modelling the real shape costs nothing and pins nothing false.
        sessions[0].background = false;
        sessions[0].has_agent_name = true;
        sessions[0].has_agent_setting = true;
        // The fork: named, but the binding is gone.
        sessions[1].background = true;
        sessions[1].has_agent_name = true;
        sessions[1].has_agent_setting = false;
        // The lone row matches the BARE signature and must STILL not be flagged —
        // it is its own lineage root, so it lost nothing.
        sessions[2].background = true;
        sessions[2].has_agent_name = true;
        sessions[2].has_agent_setting = false;
        App::new(sessions, Scope::All, PathBuf::from("/tmp/launch"))
    }

    /// The badge, read off the DRAWN cells: it lands on the downgraded fork's row
    /// in the board's caution color, and it says only what was observed.
    #[test]
    fn render_list_badges_a_background_fork_that_lost_its_binding() {
        let mut app = downgraded_lineage_board();
        let (width, height) = LINEAGE_BOARD_SIZE;
        let buffer = drawn_list(&mut app, width, height);

        // The fork heads the folded lineage, so it draws the shared label.
        let head = row_of(&buffer, width, height, LINEAGE_LABEL);
        let text = row_text(&buffer, head, width);
        assert!(
            text.contains(AGENT_UNBOUND_MARKER.trim()),
            "the downgraded fork must wear the badge: {text:?}"
        );

        // Style read off the buffer, not off the span we built: a marker the List
        // restyles away is a marker the user never sees.
        let needle = AGENT_UNBOUND_MARKER.trim();
        let x = column_of(&buffer, head, width, needle);
        for (i, ch) in needle.chars().enumerate() {
            let cell = buffer
                .cell((
                    x + u16::try_from(i).expect("a marker shorter than a row"),
                    head,
                ))
                .expect("a drawn marker cell");
            assert_eq!(cell.symbol(), ch.to_string());
            assert_eq!(
                cell.fg,
                Color::Yellow,
                "the badge is a NAMED-ANSI caution color, never dim and never RGB"
            );
            assert!(
                !cell.modifier.contains(Modifier::DIM),
                "a real defect is not a footnote"
            );
        }
    }

    /// The over-flag guard, drawn: the lone background row matches the BARE
    /// signature in every respect and must carry NO badge, because it is its own
    /// lineage root and therefore lost nothing.
    #[test]
    fn render_list_does_not_badge_a_lone_background_session() {
        let mut app = downgraded_lineage_board();
        let (width, height) = LINEAGE_BOARD_SIZE;
        let buffer = drawn_list(&mut app, width, height);

        let lone = row_of(&buffer, width, height, LONE_LABEL);
        let text = row_text(&buffer, lone, width);
        assert!(
            !text.contains(AGENT_UNBOUND_MARKER.trim()),
            "a lineage of one cannot have lost a binding to itself: {text:?}"
        );
    }

    /// The board with nothing wrong on it draws no badge anywhere — the guard
    /// against a marker that is really just always on.
    #[test]
    fn render_list_badges_nothing_on_an_ordinary_board() {
        let mut app = lineage_board();
        let (width, height) = LINEAGE_BOARD_SIZE;
        let buffer = drawn_list(&mut app, width, height);

        for row in 0..height {
            assert!(
                !row_text(&buffer, row, width).contains(AGENT_UNBOUND_MARKER.trim()),
                "no session here lost a binding, so no row may claim one"
            );
        }
    }

    /// Wide enough for the badge beside the `(+N)` it shares the row with, but
    /// NOT for the whole label — the only width at which the badge's share of the
    /// [`fit_label`] reservation is observable.
    ///
    /// The fixture's epoch timestamps draw in full (`1970-01-01 00:03`, sixteen
    /// columns) where a real same-day row draws five, so this is a markedly
    /// harsher row than the board's ordinary one.
    const BADGE_TIGHT_WIDTH: u16 = 60;

    /// The badge is reserved BEFORE the label, exactly as `(+N)` is: at a width
    /// that cuts the label, the LABEL is what gives way and the badge survives.
    #[test]
    fn render_list_keeps_the_badge_when_the_pane_is_too_narrow_for_the_label() {
        let mut app = downgraded_lineage_board();
        let (_, height) = LINEAGE_BOARD_SIZE;
        let width = BADGE_TIGHT_WIDTH;
        let buffer = drawn_list(&mut app, width, height);

        let head = row_of(&buffer, width, height, AGENT_UNBOUND_MARKER.trim());
        let text = row_text(&buffer, head, width);

        assert!(
            !text.contains(LINEAGE_LABEL),
            "the fixture must be too narrow for the whole label, or it proves \
             nothing about the reservation: {text:?}"
        );
        assert!(
            text.contains("(+1)") && text.contains(AGENT_UNBOUND_MARKER.trim()),
            "both trailing markers outrank the label they sit beside: {text:?}"
        );
        assert!(
            text.chars().count() <= usize::from(width),
            "the row must never overrun its own width: {text:?}"
        );
    }

    /// The other end of the same discipline: at a width that cannot fit the badge
    /// even with NO label, it is DROPPED whole rather than clipped. A half-drawn
    /// `[unboun` asserts nothing, and a marker that overruns would push the row's
    /// existing content off the edge.
    #[test]
    fn render_list_drops_the_badge_rather_than_clipping_it() {
        let mut app = downgraded_lineage_board();
        let (_, height) = LINEAGE_BOARD_SIZE;
        let width = LINEAGE_NARROW_WIDTH;
        let buffer = drawn_list(&mut app, width, height);

        // Found by the `(+N)`, which still fits: the badge is what gave way.
        let head = row_of(&buffer, width, height, "(+1)");
        let text = row_text(&buffer, head, width);

        assert!(
            !text.contains('['),
            "no fragment of the badge may survive the drop: {text:?}"
        );
        assert!(
            text.contains("(+1)"),
            "the fold marker still fits and must still be drawn: {text:?}"
        );
        assert!(
            text.chars().count() <= usize::from(width),
            "the row must never overrun its own width: {text:?}"
        );
    }

    /// A lineage whose #80811 fork sits in a CHILD row, EXPANDED so that row is
    /// drawn. A downgraded member is not always its lineage's head: give the
    /// lineage a THIRD, NEWER member and the fork is pushed beneath it.
    fn downgraded_fork_in_a_child_row_board() -> App {
        // A member NEWER than the downgraded fork, so IT takes the head (D1) and
        // the fork lands in a child row. Same shapes as the fixture's own members,
        // distinct so every row stays addressable.
        const SUCCESSOR_ID: &str = "3a7bd110-9999-aaaa-bbbb-cccccccccccc";
        const SUCCESSOR_TS: i64 = 300;
        const SUCCESSOR_MSGS: usize = 58;

        let mut sessions = vec![
            lineage_session(
                ANCESTOR_ID,
                "fork-root",
                LINEAGE_LABEL,
                ANCESTOR_TS,
                ANCESTOR_MSGS,
            ),
            lineage_session(BG_ID, "fork-root", LINEAGE_LABEL, BG_TS, BG_MSGS),
            lineage_session(
                SUCCESSOR_ID,
                "fork-root",
                LINEAGE_LABEL,
                SUCCESSOR_TS,
                SUCCESSOR_MSGS,
            ),
        ];
        // The lineage ROOT — oldest, foreground, still bound. It is what makes the
        // fork's missing binding a LOSS rather than a shape it never had.
        sessions[0].has_agent_name = true;
        sessions[0].has_agent_setting = true;
        // The downgraded fork: named, binding gone. The one row that must be badged.
        sessions[1].background = true;
        sessions[1].has_agent_name = true;
        sessions[1].has_agent_setting = false;
        // The newest member is left BARE on purpose: it heads the lineage carrying
        // nothing of its own, so a marker found on the fork's row cannot have come
        // from the head row's block.

        let mut app = App::new(sessions, Scope::All, PathBuf::from("/tmp/launch"));
        app.expand_selected();
        app
    }

    /// The badge on a CHILD row. The badge is a fact about the SESSION, not about
    /// where the fold put it, so it must follow the fork down — read, as above,
    /// off the DRAWN cells rather than off a span.
    #[test]
    fn render_list_badges_a_downgraded_fork_sitting_in_a_child_row() {
        let mut app = downgraded_fork_in_a_child_row_board();
        let (width, height) = LINEAGE_BOARD_SIZE;
        let buffer = drawn_list(&mut app, width, height);

        // The fork is addressable by its id — what a child row draws INSTEAD of
        // the label it shares with its head.
        let child = row_of(&buffer, width, height, &short_id(BG_ID));
        let text = row_text(&buffer, child, width);

        // The premise, and it has teeth: without it this silently degenerates into
        // the head-row case the moment the fixture's ordering drifts.
        assert!(
            text.contains(CHILD_GUTTER.trim()) && !text.contains(LINEAGE_LABEL),
            "the downgraded fork must really be drawn as a CHILD here, or this \
             pins the head row all over again: {text:?}"
        );
        assert!(
            text.contains(AGENT_UNBOUND_MARKER.trim()),
            "the badge follows the session down into the fold, never only the row \
             that happens to head it: {text:?}"
        );

        // Style read off the buffer, not off the span we built: a marker the List
        // restyles away is a marker the user never sees.
        let needle = AGENT_UNBOUND_MARKER.trim();
        let x = column_of(&buffer, child, width, needle);
        for (i, ch) in needle.chars().enumerate() {
            let cell = buffer
                .cell((
                    x + u16::try_from(i).expect("a marker shorter than a row"),
                    child,
                ))
                .expect("a drawn marker cell");
            assert_eq!(cell.symbol(), ch.to_string());
            assert_eq!(
                cell.fg,
                Color::Yellow,
                "the badge is a NAMED-ANSI caution color, never dim and never RGB"
            );
            assert!(
                !cell.modifier.contains(Modifier::DIM),
                "a real defect is not a footnote"
            );
        }
    }

    // --- failed background task: the row marker -----------------------------

    /// Assert that `needle` is drawn on row `y` in the failure color, never DIM —
    /// read off the buffer's cells, not off the span that was built.
    fn assert_failed_marker_cells(buffer: &ratatui::buffer::Buffer, y: u16, width: u16) {
        let needle = FAILED_TASK_MARKER.trim();
        let x = column_of(buffer, y, width, needle);
        for (i, ch) in needle.chars().enumerate() {
            let cell = buffer
                .cell((
                    x + u16::try_from(i).expect("a marker shorter than a row"),
                    y,
                ))
                .expect("a drawn marker cell");
            assert_eq!(cell.symbol(), ch.to_string());
            assert_eq!(
                cell.fg,
                Color::Red,
                "the marker is the NAMED-ANSI failure color, never RGB"
            );
            assert!(
                !cell.modifier.contains(Modifier::DIM),
                "an unanswered failure is not a footnote"
            );
        }
    }

    /// The marker lands on the row whose session carries a failed task, in the
    /// failure color — and on NO other row, the guard against a marker that is
    /// really just always on.
    #[test]
    fn render_list_marks_a_session_whose_background_task_failed() {
        let mut app = lineage_board();
        let lone = app
            .sessions
            .iter()
            .position(|s| s.session_id == LONE_ID)
            .expect("the lone session is on the board");
        app.sessions[lone].failed_task = Some(failed_task(SHORT_FAILURE, Some(FAILED_AT)));
        let (width, height) = LINEAGE_BOARD_SIZE;
        let buffer = drawn_list(&mut app, width, height);

        let row = row_of(&buffer, width, height, LONE_LABEL);
        assert!(
            row_text(&buffer, row, width).contains(FAILED_TASK_MARKER.trim()),
            "the flagged session must wear the marker"
        );
        assert_failed_marker_cells(&buffer, row, width);
        for other in (0..height).filter(|&y| y != row) {
            assert!(
                !row_text(&buffer, other, width).contains(FAILED_TASK_MARKER.trim()),
                "only the flagged session may claim a failed task (row {other})"
            );
        }
    }

    /// A background fork copies its parent's transcript, so a CHILD row can
    /// carry an inherited failure until its own first prompt. The marker is a
    /// fact about that file, so it follows the session into the fold.
    #[test]
    fn render_list_marks_a_failed_task_on_a_child_row() {
        let mut app = lineage_board();
        let ancestor = app
            .sessions
            .iter()
            .position(|s| s.session_id == ANCESTOR_ID)
            .expect("the ancestor is on the board");
        app.sessions[ancestor].failed_task = Some(failed_task(SHORT_FAILURE, None));
        app.expand_selected();
        let (width, height) = LINEAGE_BOARD_SIZE;
        let buffer = drawn_list(&mut app, width, height);

        let child = row_of(&buffer, width, height, &short_id(ANCESTOR_ID));
        let text = row_text(&buffer, child, width);
        assert!(
            text.contains(CHILD_GUTTER.trim()) && !text.contains(LINEAGE_LABEL),
            "the flagged ancestor must really be drawn as a CHILD, or this pins the \
             head row all over again: {text:?}"
        );
        assert!(
            text.contains(FAILED_TASK_MARKER.trim()),
            "the marker follows the session into the fold: {text:?}"
        );
        assert_failed_marker_cells(&buffer, child, width);
    }

    /// Wide enough for `(+1)` and the failed-task marker beside the timestamp,
    /// but NOT for the `[unbound]` badge as well: the one width at which the
    /// markers' priority is observable.
    const ONE_MARKER_WIDTH: u16 = 50;

    /// When a row has room for only one of the two defect markers, the failed
    /// task — the one that wants the user — is the one kept, and the other is
    /// dropped WHOLE. Neither may overrun the row.
    #[test]
    fn render_list_keeps_the_failed_marker_when_only_one_marker_fits() {
        let mut app = downgraded_lineage_board();
        let fork = app
            .sessions
            .iter()
            .position(|s| s.session_id == BG_ID)
            .expect("the downgraded fork is on the board");
        app.sessions[fork].failed_task = Some(failed_task(SHORT_FAILURE, None));
        let (full_width, height) = LINEAGE_BOARD_SIZE;

        // The premise: with room, the fork's head row wears BOTH.
        let wide = drawn_list(&mut app, full_width, height);
        let head = row_of(&wide, full_width, height, "(+1)");
        let text = row_text(&wide, head, full_width);
        assert!(
            text.contains(FAILED_TASK_MARKER.trim()) && text.contains(AGENT_UNBOUND_MARKER.trim()),
            "the wide board must draw both markers, or this pins nothing: {text:?}"
        );

        let width = ONE_MARKER_WIDTH;
        let narrow = drawn_list(&mut app, width, height);
        let head = row_of(&narrow, width, height, "(+1)");
        let text = row_text(&narrow, head, width);
        assert!(
            text.contains(FAILED_TASK_MARKER.trim()),
            "the failed-task marker outranks the #80811 badge: {text:?}"
        );
        // The kept marker must END the row. A badge pushed after it anyway runs
        // off the pane's edge and is clipped to whatever fragment fits (`[un`,
        // `[u`, ...), so no needle for the fragment can be trusted. And a
        // character count of the row cannot catch it either: `row_text` reads at
        // most `width - 2` cells, so it can never exceed `width`.
        assert!(
            text.ends_with(FAILED_TASK_MARKER.trim()),
            "the badge that did not fit is dropped whole, never clipped at the \
             edge, so nothing may follow the kept marker: {text:?}"
        );
    }

    /// At a width that cannot fit the marker even with NO label, it is DROPPED
    /// whole rather than clipped: a half-drawn `[task fai` asserts nothing.
    #[test]
    fn render_list_drops_the_failed_marker_rather_than_clipping_it() {
        let mut app = lineage_board();
        let fork = app
            .sessions
            .iter()
            .position(|s| s.session_id == BG_ID)
            .expect("the fork is on the board");
        app.sessions[fork].failed_task = Some(failed_task(SHORT_FAILURE, None));
        let (_, height) = LINEAGE_BOARD_SIZE;
        let width = LINEAGE_NARROW_WIDTH;
        let buffer = drawn_list(&mut app, width, height);

        // Found by the `(+N)`, which still fits: the marker is what gave way.
        // No width check on top: `row_text` reads at most `width - 2` cells, so a
        // count of it can never exceed `width`. A marker pushed past the edge
        // shows up as its clipped `[` fragment instead, which is what this pins.
        let head = row_of(&buffer, width, height, "(+1)");
        let text = row_text(&buffer, head, width);
        assert!(
            !text.contains('['),
            "no fragment of the marker may survive the drop: {text:?}"
        );
    }

    /// Wide enough for the flagged ANCESTOR's child row to draw its id and its
    /// `  6 msgs`, but NOT the failed-task marker after them — and wide enough
    /// that a marker pushed anyway would show at least its `[`.
    ///
    /// That child row spends 39 columns before the marker (gutter, the fixture's
    /// sixteen-column epoch timestamp, the gap, the id, the turn count), so the
    /// marker would take it to 54 and its `[` sits at column 42. This width gives
    /// the row 46 drawable columns: between the two.
    const CHILD_NO_MARKER_WIDTH: u16 = 50;

    /// The child-row twin of
    /// `render_list_drops_the_failed_marker_rather_than_clipping_it`: at a width
    /// the marker does not fit, a CHILD row drops it whole while the fields
    /// before it still draw — never a clipped `[task fai` at the edge.
    #[test]
    fn render_list_drops_the_failed_marker_on_a_child_row_rather_than_clipping_it() {
        let mut app = lineage_board();
        let ancestor = app
            .sessions
            .iter()
            .position(|s| s.session_id == ANCESTOR_ID)
            .expect("the ancestor is on the board");
        app.sessions[ancestor].failed_task = Some(failed_task(SHORT_FAILURE, None));
        app.expand_selected();
        let (full_width, height) = LINEAGE_BOARD_SIZE;

        // The premise: with room, the ancestor's CHILD row wears the marker.
        let wide = drawn_list(&mut app, full_width, height);
        let child = row_of(&wide, full_width, height, &short_id(ANCESTOR_ID));
        let text = row_text(&wide, child, full_width);
        assert!(
            text.contains(CHILD_GUTTER.trim()) && text.contains(FAILED_TASK_MARKER.trim()),
            "the wide board must draw the marker on a CHILD row, or this pins \
             nothing: {text:?}"
        );

        let width = CHILD_NO_MARKER_WIDTH;
        let narrow = drawn_list(&mut app, width, height);
        let child = row_of(&narrow, width, height, &short_id(ANCESTOR_ID));
        let text = row_text(&narrow, child, width);
        assert!(
            text.contains(child_msgs(ANCESTOR_MSGS).trim()),
            "the turn count still fits, so the marker is what gave way: {text:?}"
        );
        assert!(
            !text.contains('['),
            "no fragment of the marker may survive the drop: {text:?}"
        );
    }

    /// Wide enough for the #80811 fork's child row to fit ONE defect marker after
    /// its id and `  171 msgs`, but not both.
    ///
    /// That child row spends 41 columns before its markers: the failed-task
    /// marker takes it to 56, the `[unbound]` badge to 52, the pair to 67. This
    /// width gives the row 60 drawable columns, so either marker fits alone and
    /// whichever is decided first is the one drawn.
    const CHILD_ONE_MARKER_WIDTH: u16 = 64;

    /// The child-row twin of
    /// `render_list_keeps_the_failed_marker_when_only_one_marker_fits`: a child
    /// row with room for only one defect marker keeps the failed task, the one
    /// that wants the user, and drops the badge WHOLE.
    #[test]
    fn render_list_keeps_the_failed_marker_on_a_child_row_when_only_one_marker_fits() {
        let mut app = downgraded_fork_in_a_child_row_board();
        let fork = app
            .sessions
            .iter()
            .position(|s| s.session_id == BG_ID)
            .expect("the downgraded fork is on the board");
        app.sessions[fork].failed_task = Some(failed_task(SHORT_FAILURE, None));
        let (full_width, height) = LINEAGE_BOARD_SIZE;

        // The premise: with room, the fork's CHILD row wears BOTH.
        let wide = drawn_list(&mut app, full_width, height);
        let child = row_of(&wide, full_width, height, &short_id(BG_ID));
        let text = row_text(&wide, child, full_width);
        assert!(
            text.contains(CHILD_GUTTER.trim()) && !text.contains(LINEAGE_LABEL),
            "the fork must really be drawn as a CHILD here, or this pins the head \
             row all over again: {text:?}"
        );
        assert!(
            text.contains(FAILED_TASK_MARKER.trim()) && text.contains(AGENT_UNBOUND_MARKER.trim()),
            "the wide board must draw both markers, or this pins nothing: {text:?}"
        );

        let width = CHILD_ONE_MARKER_WIDTH;
        let narrow = drawn_list(&mut app, width, height);
        let child = row_of(&narrow, width, height, &short_id(BG_ID));
        let text = row_text(&narrow, child, width);
        assert!(
            text.contains(FAILED_TASK_MARKER.trim()),
            "the failed-task marker outranks the #80811 badge on a child row too: \
             {text:?}"
        );
        // Ends the row, for the reason the head-row twin gives: a badge pushed
        // anyway is clipped to a fragment no fixed needle can predict — here
        // only `[u` would survive the edge.
        assert!(
            text.ends_with(FAILED_TASK_MARKER.trim()),
            "the badge that did not fit is dropped whole, never clipped at the \
             edge, so nothing may follow the kept marker: {text:?}"
        );
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
                    pid: None,
                    started_at_ms: None,
                },
            );
        }
        let mut app = App::new(sessions, Scope::All, PathBuf::from("/tmp/launch"));
        app.set_reported_agents(reported, None);
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

    /// How many short turns lead the `Ctrl-T`/`Ctrl-E` endpoint transcript, before
    /// the long final turn. Enough that the top and bottom viewports cannot overlap.
    const ENDPOINT_LEAD_TURNS: usize = 6;

    /// A transcript for the jump-endpoint cases: [`ENDPOINT_LEAD_TURNS`] short
    /// user/assistant pairs, then ONE assistant turn whose body outruns the pane.
    ///
    /// That last turn is what makes the bottom endpoint's claim sharp. With a short
    /// tail the final viewport opens some rows before the last marker and the banner
    /// would name whichever turn happens to straddle that row — true, but it would
    /// pass just as well if the lookup were off by a turn. A tail TALLER than the
    /// viewport puts the clamped bottom offset strictly INSIDE the last turn, so
    /// "the banner names the last turn" is the only correct answer.
    fn endpoint_jsonl() -> String {
        let mut jsonl = String::new();
        for turn in 0..ENDPOINT_LEAD_TURNS {
            jsonl.push_str(&format!(
                r#"{{"type":"user","sessionId":"sess-normal-1","cwd":"/Users/me/project-alpha","timestamp":"2026-07-04T10:00:00.000Z","message":{{"role":"user","content":"ask number {turn}"}}}}"#
            ));
            jsonl.push('\n');
            jsonl.push_str(&format!(
                r#"{{"type":"assistant","sessionId":"sess-normal-1","cwd":"/Users/me/project-alpha","timestamp":"2026-07-04T10:00:05.000Z","message":{{"role":"assistant","content":"answer number {turn}"}}}}"#
            ));
            jsonl.push('\n');
        }
        // The tall final turn: many separate lines, so its height comes from the
        // transcript's own line count rather than from a wrap this pane might undo.
        let tall = (0..BANNER_PANE.1 * 2)
            .map(|line| format!("tail line {line}"))
            .collect::<Vec<_>>()
            .join("\\n");
        jsonl.push_str(&format!(
            r#"{{"type":"assistant","sessionId":"sess-normal-1","cwd":"/Users/me/project-alpha","timestamp":"2026-07-04T10:10:00.000Z","message":{{"role":"assistant","content":"{tall}"}}}}"#
        ));
        jsonl.push('\n');
        jsonl
    }

    /// `Ctrl-T` (and `Home`, its twin) parks the pinned banner on the OPENING turn.
    ///
    /// `Action::PreviewTop` drives `preview_scroll` straight to 0, which is the one
    /// offset that lands ABOVE every marker — the blank row leading the first turn —
    /// so the banner's turn comes from `marker_owning_row`'s `.or_else(markers.first())`
    /// fall-through rather than from its reverse scan. This asserts the DRAWN row at
    /// that endpoint, because the fall-through compiles and renders perfectly while
    /// naming nothing (or the wrong turn) if it is ever dropped. The keypress half —
    /// `Ctrl-T` reaching `Action::PreviewTop` at all — is
    /// `update::tests::preview_scroll_keys_act_regardless_of_query`.
    #[test]
    fn the_banner_names_the_opening_turn_at_the_ctrl_t_jump_endpoint() {
        let (width, height) = BANNER_PANE;
        let inner_width = width - 2;
        let mut app = banner_app_over(&endpoint_jsonl(), "banner-ctrl-t");

        // The transcript must overflow the pane, or top and bottom are one place and
        // neither endpoint is being tested.
        let content_h = content_height(&mut app, width);
        assert!(
            content_h > usize::from(height),
            "the transcript must outrun the pane (got {content_h} rows in {height})"
        );

        let you_rows = marker_rows(&mut app, inner_width, "\u{25b6} you");
        let first_marker_row = app
            .preview_rows_above(inner_width, you_rows[0])
            .expect("the opening marker is inside the transcript");
        // The fall-through only runs when row 0 sits ABOVE the first marker. If the
        // transcript ever stops leading with a blank row, this test would silently
        // start exercising the reverse scan instead.
        assert!(
            first_marker_row > 0,
            "the transcript must open with a row above its first marker, or the \
             fall-through this pins is not the path taken (marker at row \
             {first_marker_row})"
        );

        app.preview_top();
        let rows = inner_rows(&mut app, width, height);
        let resolved = usize::try_from(app.preview_scroll).expect("a small resolved offset");
        assert_eq!(resolved, 0, "Ctrl-T resolves to the very first row");
        assert_eq!(
            rows[0],
            banner_should_show(&mut app, inner_width, resolved),
            "at the top endpoint the banner must name the OPENING turn"
        );
    }

    /// `Ctrl-E` (and `End`, its twin) parks the pinned banner on the turn owning the
    /// FINAL viewport's top row — here the last turn, whose body fills that viewport.
    ///
    /// `Action::PreviewBottom` names no offset at all: it re-arms
    /// `preview_follow_bottom`, and the row the pane lands on is whatever
    /// `clamp_preview_offset` derives as `content_h - viewport_h`. So the banner has to
    /// agree with a number no keypress ever stated, which is the half that can silently
    /// name the wrong turn. The keypress half — `Ctrl-E` reaching
    /// `Action::PreviewBottom` — is
    /// `update::tests::preview_scroll_keys_act_regardless_of_query`.
    #[test]
    fn the_banner_names_the_tail_turn_at_the_ctrl_e_jump_endpoint() {
        let (width, height) = BANNER_PANE;
        let inner_width = width - 2;
        let mut app = banner_app_over(&endpoint_jsonl(), "banner-ctrl-e");

        let content_h = content_height(&mut app, width);
        assert!(
            content_h > usize::from(height),
            "the transcript must outrun the pane (got {content_h} rows in {height})"
        );

        let claude_rows = marker_rows(&mut app, inner_width, "\u{25cf} claude");
        let last_marker_row = app
            .preview_rows_above(
                inner_width,
                *claude_rows.last().expect("a closing assistant turn"),
            )
            .expect("the closing marker is inside the transcript");

        app.preview_bottom();
        let rows = inner_rows(&mut app, width, height);
        let resolved = usize::try_from(app.preview_scroll).expect("a small resolved offset");

        // The clamp must produce a REAL bottom offset, and it must land inside the last
        // turn — that is what makes "the banner names the last turn" the only right
        // answer here rather than one turn among several plausible ones.
        assert!(resolved > 0, "Ctrl-E must resolve past the top row");
        assert!(
            resolved >= last_marker_row,
            "the final viewport must open INSIDE the last turn for this to pin the \
             tail (resolved {resolved}, last marker opens at {last_marker_row})"
        );
        assert_eq!(
            rows[0],
            banner_should_show(&mut app, inner_width, resolved),
            "at the bottom endpoint the banner must name the turn owning THAT row"
        );

        // And the two endpoints really are different turns, so neither assertion above
        // could be passing by naming the same marker in both places.
        let mut at_top = banner_app_over(&endpoint_jsonl(), "banner-ctrl-e-top");
        at_top.preview_top();
        let top_rows = inner_rows(&mut at_top, width, height);
        assert_ne!(
            rows[0], top_rows[0],
            "the top and bottom endpoints must pin DIFFERENT turns"
        );
    }
}
