//! The compose zone: multiline input + its key dispatch.
//!
//! This module owns the MULTILINE `ratatui_textarea` editor (plus the
//! `App::compose` field, whose type is [`ComposeState`]) — one of exactly TWO
//! sites that reference the crate. The other is the board's ONE-LINE search
//! query, [`App::query_input`](super::app::App::query_input), edited by `App`'s
//! query mutators and drawn by `view::render_search`. Each site owns its own
//! configuration and its own key routing, and neither reads the other's; the
//! pairing is not a `memchr`-style single-module confinement, and `Cargo.toml`
//! states the same two-site boundary as the blast radius of a version bump.
//!
//! The difference that matters between the two: this editor IS a keyboard owner,
//! so it may forward raw keys to `TextArea::input`. The board is not, and must
//! never do so (PATTERNS.md §10 — the widget's own map would steal `Ctrl-C`,
//! `Ctrl-K`, `Ctrl-X` and `Tab` from the board).
//!
//! The compose zone is a modal — while it is open it owns the keyboard, exactly
//! like the running-session and agent-pick overlays — and leaves by submitting
//! (`Enter`), running interactively (`Ctrl-O`), or cancelling (`Esc`). `Ctrl-L`
//! opens the model picker OVER it, for this one compose only (see
//! [`ComposeState::model`]); the picker returns to the same draft either way.
//!
//! It is also the only installer of the PANE-level twin,
//! [`App::draft`](super::app::App::draft): [`open_background`] opens the editor and
//! the placeholder card together (via `App::open_compose`), and every exit closes
//! both through `App::close_compose`. `Enter` on a BACKGROUND draft is the one
//! exception, and a deliberate one — `App::dispatch_draft` closes the editor but
//! leaves the card up, in flight, until `AppEvent::BgLaunchFinished` lands.
//!
//! ONE editor and ONE key router serve TWO drafts, distinguished by
//! [`ComposeTarget`] rather than by parallel state:
//!
//! * [`ComposeTarget::Reply`] — the quick reply `Ctrl-R` opens on a selected
//!   session. `Enter` sends a one-shot `claude -p -r` ([`crate::send`]).
//! * [`ComposeTarget::NewBackgroundAgent`] — the draft `Ctrl-N` opens: via `Enter`
//!   on the agent picker's highlighted row, or directly when no agents are defined.
//!   `Enter` starts a BACKGROUND agent with the draft as its first prompt, and
//!   `Ctrl-O` runs it interactively instead (the one action that leaves the board)
//!   — the same verb the picker's own `Ctrl-O` names, so "open interactive claude"
//!   reads identically on both surfaces.
//!
//! The pure DECISION is [`compose_key_to_action`], unit-tested like
//! [`super::update::key_to_action`] and free of any `TextArea` reference. The
//! impure edits (insert a newline, forward a keystroke to the widget) and both
//! hand-offs live in [`handle_compose_key`], a thin driver over that decision and
//! over the pure cores in [`crate::send`] / [`crate::resume`].
//!
//! BOTH drafts carry the same pick list ([`CompletionState`]), through the same
//! editor, router and driver: `/` first in the draft lists the folder's skills and
//! commands, `@` at the start of a word lists files and folders and, at the top
//! level, agents. The one per-target difference is WHERE the list reads from, and
//! [`completion_source`] alone decides it. While the list is showing the router
//! claims `Enter`/`Tab` (pick), `Up`/`Down` (highlight) and `Esc` (close the list,
//! never the draft); otherwise every key decodes as before. Its bounded reads run
//! from the key/paste handler and the `CatalogFetched` arm only
//! ([`refresh_completion`]), never the render path; claude's own list for the
//! folder is fetched off the UI thread, requested by [`take_catalog_fetch`].
//!
//! [`insert_paste`] is the ONE entry point that is not a keypress: a terminal paste
//! arrives as whole TEXT (`super::update::handle_paste`) and goes straight into the
//! editor, bypassing the key router entirely. That bypass is the point — routed as
//! keystrokes, a pasted newline reached the `Enter` = Send arm above.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_textarea::{CursorMove, TextArea, WrapMode};

use crate::resume::ModelPick;
use crate::send::{self, BgLaunchRequest, SendPlan, SendRequest};
use crate::store::skills::{read_listing, Listing};

use super::app::{App, NewSessionDraft};
use super::complete::{
    at_candidates, completion_context, filter_commands, list_dir, move_highlight, replacement,
    settle_highlight, split_path_query, Candidate, Context, DirEntryInfo, Trigger,
};
use super::update::Outcome;

/// Status shown when Send is pressed on an empty / whitespace-only reply buffer:
/// the compose zone stays open (nothing was sent), so this is a gentle nudge
/// rather than a refusal.
pub(crate) const COMPOSE_EMPTY_HINT: &str = "nothing to send — type a message first";

/// The [`COMPOSE_EMPTY_HINT`] of the background draft: `Enter` on an empty buffer
/// keeps the pane open rather than launching. Its own const because the nudge is
/// different in kind — a background agent started with no prompt would sit there
/// doing nothing, so this is not "you forgot to type", it is "there would be
/// nothing for it to do".
const COMPOSE_EMPTY_BG_HINT: &str = "nothing to run — a background agent needs a first message";

/// Status when the composed session vanished from the store between opening the
/// compose zone and pressing Send (e.g. its file was removed).
const COMPOSE_SESSION_GONE: &str = "that session is no longer loaded — nothing sent";

/// What an open compose buffer is addressed to — the ONE fork the shared editor
/// and key router branch on.
///
/// Modeled as an enum rather than as optional fields beside each other so the two
/// drafts cannot be half-built: a reply always has a session, a background launch
/// never does, and no state can claim both. Each variant carries exactly what its
/// own submit needs and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposeTarget {
    /// A quick reply to an EXISTING session (`Ctrl-R`).
    Reply {
        /// Stable `session_id` the reply is addressed to (the row that was
        /// selected when `Ctrl-R` opened compose) — STABLE-ID STATE, re-resolved
        /// to the authoritative `(cwd, session_id)` from inside the file at Send
        /// time and never trusted as a live path.
        session_id: String,
        /// Short agent-view job id to `claude stop` before sending — set when the
        /// target is a held (`done`/`needs input`) background agent that must be
        /// deregistered first (see [`crate::send::reply_gate`]). `None` for a
        /// plain in-place reply.
        stop_job: Option<String>,
    },
    /// A first prompt for a BRAND-NEW background agent (`Enter` on the new-session
    /// agent picker, or `Ctrl-N` itself when no agents are defined). There is no
    /// session id — claude mints one — so this variant structurally cannot pretend
    /// to address a row on the board.
    NewBackgroundAgent {
        /// The picker row's agent name, or `None` for its "default (no agent)"
        /// row. Nothing else about the pick is retained: the transcript's own
        /// `agent-setting` record is what answers "which agent was this?" later.
        agent: Option<String>,
    },
}

/// The open compose zone: what it is addressed to and the live editor buffer.
///
/// Modeled as explicit `App` state (a sibling of the other overlay states such
/// as `modal` and `pending_stop`) so the compose modal is a small, inspectable
/// piece of state.
pub struct ComposeState {
    /// What this draft is addressed to — a reply, or a new background agent.
    pub target: ComposeTarget,
    /// The multiline editor buffer, shared by BOTH targets. One of exactly TWO
    /// `ratatui_textarea` values in the program — the other is the board's
    /// one-line [`App::query_input`](super::app::App::query_input), which is a
    /// separate buffer with its own configuration and is never routed through
    /// this one (see the module doc).
    pub textarea: TextArea<'static>,
    /// The model (and optional effort) picked for THIS compose with `Ctrl-L`, or
    /// `None` for its default — no `--model` at all, so a reply normally keeps the
    /// model the session last answered with and a draft starts on the settings' model.
    ///
    /// SCOPED TO ONE COMPOSE, by construction: it lives on the compose state, so it
    /// is born `None` with every [`ComposeState::new`] and dies with the compose
    /// (`App::close_compose`). Nothing global holds it and nothing on disk ever
    /// learns it — a pick is a request for one message, and claude itself keeps a
    /// session on the model a reply ran with (it normally restores that model on the
    /// next `-r`). Written only by `App::set_compose_model`, from the picker's confirm;
    /// read by the submit paths below, which emit `--model`/`--effort` ONLY when it
    /// is `Some`, and by the compose box's `model:` label.
    pub model: Option<ModelPick>,
    /// The `/` and `@` pick list of this draft, reply and background draft alike
    /// (see [`CompletionState`]).
    pub completion: CompletionState,
}

/// The compose pick list of either draft: what is showing, plus the per-draft
/// caches that keep its filesystem reads bounded. Born empty with every
/// [`ComposeState::new`] and dead with the compose — nothing global, nothing
/// persisted. Claude's own list for a folder is NOT kept here: it outlives the
/// draft, in `App::catalogs`.
#[derive(Debug, Default)]
pub struct CompletionState {
    /// The rows on offer. DERIVED by [`refresh_completion`] and `Some` ONLY when
    /// non-empty: an empty list would still claim `Enter` and `Esc` (see
    /// [`compose_key_to_action`]), so "no match" must mean "no list", never an
    /// empty one.
    pub visible: Option<Vec<Candidate>>,
    /// The highlighted row of `visible`.
    pub highlight: usize,
    /// The token the caret ends, whether or not its list is showing.
    pub context: Option<Context>,
    /// The token `Esc` closed the list for: `(trigger, row, start column)`. The
    /// list stays closed while the caret is still in that same token.
    dismissed: Option<(Trigger, usize, usize)>,
    /// A REPLY's `@` agents until a catalog lands, read from its session transcript
    /// by [`read_listing`]: `None` = not read yet, an empty listing = read and
    /// nothing found. Read at most once per draft, only while no catalog has landed
    /// for the folder, only for a top-level `@` (an `@` with a folder part lists no
    /// agent, and `/` lists the catalog alone), and never for a background draft
    /// (it has no transcript).
    transcript: Option<Listing>,
    /// Whether this compose has had its ONE ask for a catalog fetch
    /// ([`take_catalog_fetch`]). Born `false` with every compose, so the next
    /// compose on a folder whose fetch failed asks again, while this one never does.
    catalog_requested: bool,
    /// Folder listings keyed by the resolved folder. Read at most once per folder.
    dirs: HashMap<PathBuf, Vec<DirEntryInfo>>,
}

/// What one refresh of the list reads from, borrowed for that refresh alone: the
/// target's folder, its transcript (a reply's only) and the folder's cached
/// catalog, if one has landed. Borrowed, so a catalog is never cloned per
/// keystroke.
struct SourceView<'a> {
    cwd: &'a Path,
    transcript: Option<&'a Path>,
    catalog: Option<&'a Listing>,
}

impl CompletionState {
    /// Recompute the list for the caret at `cursor` in `lines`. `source` is what
    /// this draft's list reads from ([`completion_source`]), or `None` for a reply
    /// whose session left the store, which shows no list.
    ///
    /// `/` lists the folder's `catalog` alone, and nothing until it lands: only
    /// claude's own list knows which skills and commands its `/` menu hides, while
    /// a transcript's `skill_listing` is the MODEL's list and carries no flag to
    /// tell them apart (`docs/agents/CLAUDE_CLI.md`, "The `initialize` control
    /// handshake"). A top-level `@` takes its agents from the catalog once it has
    /// landed — it REPLACES the reply's transcript, never merges with it — else
    /// from that transcript, else none.
    ///
    /// The ONLY reads the pick list makes happen here: the transcript listing (once
    /// per reply draft, and only for a top-level `@`) and one folder listing
    /// (once per folder, capped by `COMPLETION_MAX_DIR_ENTRIES`). Bounded
    /// synchronous reads on the key/paste path are the same shape as
    /// `defined_agents::discover_agents` on `Ctrl-N` and `send::plan_send` on
    /// `Enter` (PATTERNS.md section 6); the render path only ever reads the cached
    /// `visible` and performs no I/O.
    fn refresh(
        &mut self,
        lines: &[String],
        cursor: (usize, usize),
        source: Option<SourceView<'_>>,
    ) {
        let Some(source) = source else {
            // The ask is per compose, not per source: a session that leaves the
            // store and comes back must not buy this compose a second fetch.
            *self = Self {
                catalog_requested: self.catalog_requested,
                ..Self::default()
            };
            return;
        };
        let Some(ctx) = completion_context(lines, cursor) else {
            self.context = None;
            self.dismissed = None;
            self.visible = None;
            return;
        };
        let key = (ctx.trigger, ctx.row, ctx.start);
        let start_changed = self.context.as_ref().map(|c| (c.trigger, c.row, c.start)) != Some(key);
        self.context = Some(ctx.clone());
        if self.dismissed != Some(key) {
            self.dismissed = None;
        } else {
            self.visible = None;
            return;
        }
        let candidates = match ctx.trigger {
            Trigger::Slash => source.catalog.map_or_else(Vec::new, |listing| {
                filter_commands(&listing.commands, &ctx.query)
            }),
            Trigger::At => {
                let (dir_part, _) = split_path_query(&ctx.query);
                let dir = source.cwd.join(dir_part);
                let entries = self
                    .dirs
                    .entry(dir.clone())
                    .or_insert_with(|| list_dir(&dir));
                // Only a top-level `@` lists agents (`complete::at_candidates`), so
                // only then may the transcript be read.
                let agents = if dir_part.is_empty() {
                    active_listing(&mut self.transcript, &source)
                        .map_or(&[][..], |listing| listing.agents.as_slice())
                } else {
                    &[]
                };
                at_candidates(entries, agents, &ctx.query)
            }
        };
        if candidates.is_empty() {
            self.visible = None;
            return;
        }
        self.highlight = settle_highlight(self.highlight, candidates.len(), start_changed);
        self.visible = Some(candidates);
    }
}

/// The listing a top-level `@` takes its agents from: `source`'s catalog when one
/// has landed, else the reply's transcript — read into `cache` on first use, and
/// only when `source` has a transcript — else none. `/` never asks it: it lists
/// the catalog alone. Takes the cache field alone, so the caller keeps its other
/// fields borrowable.
fn active_listing<'a>(
    cache: &'a mut Option<Listing>,
    source: &SourceView<'a>,
) -> Option<&'a Listing> {
    if let Some(catalog) = source.catalog {
        return Some(catalog);
    }
    let file = source.transcript?;
    Some(cache.get_or_insert_with(|| read_listing(file)))
}

/// Where a compose's pick list reads from: the folder whose files, folders and
/// catalog it offers, and the transcript its `@` agents come from until that
/// catalog lands — a reply's alone, since a background draft has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletionSource {
    /// The folder the target's `claude` child runs in: the key of its catalog, and
    /// the root an `@` path resolves against.
    pub cwd: PathBuf,
    /// A reply's session file, read for its `@` agents only; `None` for a
    /// background draft, which has none.
    pub transcript: Option<PathBuf>,
}

/// THE one place a compose target's pick-list source is decided; both
/// [`refresh_completion`] and [`take_catalog_fetch`] ask it, so the two drafts
/// share every other line of the list.
///
/// * A reply reads its session's `cwd` and file, taken from the `Session` the
///   store parsed out of the file itself (AGENTS.md AUTHORITATIVE-FROM-FILE), or
///   nothing once the session has left the store.
/// * A background draft reads [`App::launch_dir`], the directory
///   `send::plan_bg_launch` and `resume::check_new` run its child in, and has no
///   transcript.
pub(crate) fn completion_source(app: &App, target: &ComposeTarget) -> Option<CompletionSource> {
    match target {
        ComposeTarget::Reply { session_id, .. } => {
            app.session_by_id(session_id).map(|s| CompletionSource {
                cwd: s.cwd.clone(),
                transcript: Some(s.file.clone()),
            })
        }
        ComposeTarget::NewBackgroundAgent { .. } => Some(CompletionSource {
            cwd: app.launch_dir.clone(),
            transcript: None,
        }),
    }
}

impl ComposeState {
    /// Open a fresh compose buffer for `target`, configured for plain multiline
    /// input. The single constructor, so both drafts get an identically-configured
    /// editor and the widget setup lives in exactly one place — and so every
    /// compose starts on its default model ([`model`](Self::model) `None`), however
    /// the last one ended.
    #[must_use]
    pub fn new(target: ComposeTarget) -> Self {
        let mut textarea = TextArea::default();
        // No current-line underline: the compose box is a plain multiline field,
        // not a code editor. Styled via a ratatui `Style` (TERMINAL-SAFE STYLING).
        textarea.set_cursor_line_style(ratatui::style::Style::default());
        // Soft-wrap long lines at word boundaries (grapheme fallback for a word
        // wider than the box) so a long sentence stays visible instead of scrolling
        // off to the right.
        textarea.set_wrap_mode(WrapMode::WordOrGlyph);
        Self {
            target,
            textarea,
            model: None,
            completion: CompletionState::default(),
        }
    }

    /// Open a fresh REPLY buffer for `session_id`. `stop_job` carries the job id to
    /// stop first (the stop-then-reply path).
    #[must_use]
    pub fn new_reply(session_id: String, stop_job: Option<String>) -> Self {
        Self::new(ComposeTarget::Reply {
            session_id,
            stop_job,
        })
    }

    /// Open a fresh BACKGROUND-AGENT draft buffer for `agent` (`None` = the
    /// picker's default / no-agent row).
    #[must_use]
    pub fn new_background(agent: Option<String>) -> Self {
        Self::new(ComposeTarget::NewBackgroundAgent { agent })
    }

    /// How many SCREEN rows the draft currently occupies — its soft-wrapped height,
    /// which is what the auto-growing box has to be sized from.
    ///
    /// ASKED OF THE WIDGET, never modeled by the view. The editor word-wraps
    /// ([`WrapMode::WordOrGlyph`], set in [`ComposeState::new`]) and expands tabs to
    /// [`TextArea::tab_length`]; any second implementation of that in the renderer is
    /// a DIFFERENT function of the same text — a character-packing `ceil(width /
    /// inner)` model, for instance, always counts at or below word wrap, by more the
    /// longer the words — and the box then under-grows and the editor scrolls its own
    /// first row out of view. There is no public row-count API at the pinned `=0.9.2`
    /// (`screen_lines_count` is `pub(crate)`), so the count is PROBED through the
    /// public one: park a throwaway clone's cursor on the last character of the last
    /// logical line and read the screen row it landed on. [`CursorMove::Bottom`] and
    /// [`CursorMove::End`] are both DATA-line moves (they resolve a `DataCursor` and
    /// map it forward), so the pair lands on the last screen row of the last logical
    /// line whatever the wrap mode, and `row + 1` is the row COUNT.
    ///
    /// The clone is a WHOLE `TextArea` copy — the draft's lines, its undo history (50
    /// edits at the crate's default) and its already-built screen map — not a cheap
    /// handle. The two moves on it then RE-WRAP NOTHING: `move_cursor` only resolves
    /// the cursor against that copied map, which the editor rebuilds on an EDIT and on
    /// a RENDER whose area changed, never on a move. So the cost is one copy of a
    /// human-typed draft per frame, on the redraw path and only while the compose zone
    /// is open — affordable, not free. Do NOT "optimize" it into a shared cursor move:
    /// the probe must not disturb where the user's caret actually is.
    ///
    /// The logical line count is a FLOOR (`max`): a draft can never need fewer rows
    /// than it has lines, so a probe that ever degrades still cannot report a box
    /// shorter than the un-wrapped text.
    ///
    /// CAVEAT, by design: the widget builds its screen map from the width it was LAST
    /// RENDERED at, so a terminal resize (or a `Shift-←`/`→` layout step) leaves this one frame
    /// stale. Edits refresh the map immediately, and the next redraw — which the
    /// resize itself triggers — self-corrects, so the box settles a frame later rather
    /// than wrongly. Before the editor has EVER been rendered its area is still zero
    /// wide and the map is built unwrapped, so this reports logical lines; the draft
    /// is empty on that frame, which is exactly one row either way.
    #[must_use]
    pub fn screen_rows(&self) -> usize {
        let mut probe = self.textarea.clone();
        probe.move_cursor(CursorMove::Bottom);
        probe.move_cursor(CursorMove::End);
        let probed = probe.screen_cursor().row.saturating_add(1);
        probed.max(self.textarea.lines().len())
    }
}

/// A decoded intent from one keypress while the compose zone owns the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeAction {
    /// Submit the buffer (bare `Enter`) — send a reply, or launch the background
    /// agent, depending on the open [`ComposeTarget`].
    Send,
    /// Insert a newline (`Ctrl-J` primary, `Alt+Enter` guaranteed fallback,
    /// `Shift+Enter` opportunistic — see [`compose_key_to_action`]).
    Newline,
    /// Forward the keystroke to the text editor (ordinary typing / editing).
    Forward,
    /// Cancel compose and return to the board (`Esc`).
    Cancel,
    /// Run the draft INTERACTIVELY instead of in the background (`Ctrl-O`).
    ///
    /// Only [`ComposeTarget::NewBackgroundAgent`] acts on this; on a
    /// [`ComposeTarget::Reply`] it is INERT (there is no interactive launch to
    /// escape to — a reply addresses a session that already exists). The decision
    /// stays target-free here so the pure router remains a plain key → intent map;
    /// [`handle_compose_key`] is where the target decides whether the intent is
    /// actionable. `Ctrl-O` is unbound in `ratatui_textarea`, so claiming it costs
    /// the reply editor nothing it previously did.
    OpenInteractive,
    /// Open the model picker for THIS compose (`Ctrl-L`) — both targets act on it,
    /// since a reply and a draft each carry their own pick
    /// ([`ComposeState::model`]).
    ///
    /// `Ctrl-L` because it is the one control letter left that nothing on the path
    /// to this router claims:
    ///
    /// * `ratatui_textarea` `=0.9.2` does not bind it. `TextArea::input`'s map
    ///   (`textarea.rs`, the `match` in `input`) binds `Ctrl-` + `m h d k j w n p f b
    ///   a e u r y x c v` and nothing else, and falls through to `_ => false`, so
    ///   the key edits nothing today and claiming it costs the editor nothing —
    ///   exactly the `Ctrl-O` argument above.
    /// * No terminal aliases it. crossterm 0.29 decodes its byte, `0x0C`, through
    ///   the plain `0x01..=0x1A` control arm as `Char('l')` + `CONTROL`, where
    ///   `Ctrl-M`/`Ctrl-I`/`Ctrl-H` arrive as Enter/Tab/Backspace and `Ctrl-[` as
    ///   Esc. It is not flow control (`Ctrl-S`/`Ctrl-Q`), a job-control signal
    ///   (`Ctrl-Z`, `Ctrl-C`), or a key this router already owns (`Enter`, `Esc`,
    ///   `Ctrl-J`, `Ctrl-O`).
    /// * Of the control letters that survive both of those — `g`, `l`, `t` — the
    ///   Zellij multiplexer's default keymap takes `Ctrl-G` (lock) and `Ctrl-T`
    ///   (tabs) before the program ever sees them; `Ctrl-L` is the one it leaves
    ///   alone. The board binds no `Ctrl-L` either, so the key means one thing.
    PickModel,
    /// Jump the transcript to its top (`Ctrl-T`, the board's key).
    ///
    /// Acts only on a [`ComposeTarget::Reply`], whose compose previews the real
    /// transcript; a draft shows a placeholder card, so there the key falls back to
    /// the editor ([`handle_compose_key`]). `Ctrl-T` is unbound in the widget.
    PreviewTop,
    /// Jump the transcript to its bottom and re-follow the newest turn (`Ctrl-E`).
    ///
    /// Same target rule as [`ComposeAction::PreviewTop`]. `Ctrl-E` IS the widget's
    /// end-of-line, so on a reply it is taken from the editor on purpose (the caret
    /// still reaches line end with `End`/`Ctrl-F`); a draft keeps it.
    PreviewBottom,
    /// Scroll the transcript a page up (`PgUp`, the board's key). Reply-only, like
    /// [`ComposeAction::PreviewTop`]; the editor loses its caret page-up there.
    PreviewPageUp,
    /// A page down (`PgDn`). Same rule as [`ComposeAction::PreviewPageUp`].
    PreviewPageDown,
    /// A quarter page up (`Ctrl-U`). On a reply it is taken from the editor's
    /// delete-to-line-head on purpose; a draft keeps it.
    PreviewHalfUp,
    /// A quarter page down (`Ctrl-D`). On a reply it is taken from the editor's
    /// delete-char on purpose; a draft keeps it.
    PreviewHalfDown,
    /// Insert the highlighted pick-list row (`Enter` or `Tab`, list open only).
    AcceptCompletion,
    /// Move the pick-list highlight up (`Up`, list open only).
    CompletionPrev,
    /// Move the pick-list highlight down (`Down`, list open only).
    CompletionNext,
    /// Close the pick list, keeping the draft (`Esc`, list open only).
    CloseCompletion,
}

/// Map a keypress to a [`ComposeAction`]. PURE and free of any `TextArea`
/// reference, so it is unit-testable exactly like [`super::update::key_to_action`].
///
/// The chords own the two directions `ratatui_textarea` cannot separate for us —
/// its `input`/`input_without_shortcuts` both treat `Enter` as a newline — so the
/// router intercepts `Enter` (Send) and the newline chords BEFORE anything reaches
/// the widget, and forwards only the remainder:
///
/// * `Enter` (no modifier) → **Send**.
/// * `Ctrl-J` → **Newline** (primary). In raw mode — always, for the TUI —
///   crossterm 0.29 delivers `Ctrl-J` as `Char('j')`+`CONTROL`: the `\n`/0x0A byte
///   skips the `!is_raw_mode_enabled()` `Enter` arm and falls to the
///   `0x01..=0x1A` control-char arm (`0x0A - 0x01 + b'a' == 'j'`); crossterm's own
///   Issue-#371 comment documents this. Both `'j'` and `'J'` are matched for the
///   kitty path's sake.
/// * `Alt+Enter` → **Newline** (GUARANTEED fallback; needs no keyboard protocol).
/// * `Shift+Enter` → **Newline** (opportunistic). On the legacy path `Shift+Enter`
///   arrives as a bare `Enter` (indistinguishable), so this only fires if the
///   terminal INDEPENDENTLY reports `Enter`+`SHIFT` (e.g. a kitty protocol the user
///   enabled). snapback enables NO kitty protocol (AGENTS.md TERMINAL SAFETY treats
///   a leftover level as corruption), so under its own setup this arm is dead and
///   `Alt+Enter` remains the guaranteed newline.
/// * `Ctrl-O` → **OpenInteractive** (the background draft's escape hatch; inert on
///   a reply — see [`ComposeAction::OpenInteractive`]).
/// * `Ctrl-L` → **PickModel** (this compose's model picker, on both targets — see
///   [`ComposeAction::PickModel`] for why the key is free).
/// * `Ctrl-T` / `Ctrl-E` / `Home` / `End` → **PreviewTop** / **PreviewBottom**,
///   `PgUp` / `PgDn` → **PreviewPageUp** / **PreviewPageDown**, `Ctrl-U` / `Ctrl-D`
///   → **PreviewHalfUp** / **PreviewHalfDown**: exactly the board's transcript
///   scroll keys (`update::key_to_action` — the `Ctrl` letters whatever else is
///   held, the named keys only WITHOUT `Ctrl`). A reply acts on them, a draft
///   forwards them to the editor.
/// * `Esc` → **Cancel** (dismiss compose, not the app).
/// * everything else → **Forward** to the editor.
///
/// While the `/` / `@` pick list is showing (`list_open`), and only then, the keys
/// it needs are claimed AFTER the chords above (which keep their meaning): bare
/// `Enter` and `Tab` → **AcceptCompletion**, `Up`/`Down` → **CompletionPrev** /
/// **CompletionNext**, `Esc` → **CloseCompletion** (the list, never the draft).
/// With `list_open == false` every key decodes exactly as listed above.
#[must_use]
pub fn compose_key_to_action(key: KeyEvent, list_open: bool) -> ComposeAction {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Char('j' | 'J') if ctrl => ComposeAction::Newline,
        KeyCode::Char('o' | 'O') if ctrl => ComposeAction::OpenInteractive,
        // Unbound in `ratatui_textarea` =0.9.2 and aliased by no terminal: the
        // full argument is on `ComposeAction::PickModel`. Both cases, for the kitty
        // path's sake, like the arms above.
        KeyCode::Char('l' | 'L') if ctrl => ComposeAction::PickModel,
        KeyCode::Char('t' | 'T') if ctrl => ComposeAction::PreviewTop,
        KeyCode::Char('e' | 'E') if ctrl => ComposeAction::PreviewBottom,
        KeyCode::Char('u' | 'U') if ctrl => ComposeAction::PreviewHalfUp,
        KeyCode::Char('d' | 'D') if ctrl => ComposeAction::PreviewHalfDown,
        // The board binds these named keys only outside its `ctrl` block (where
        // `Ctrl-Home` etc. are ignored), so `Ctrl-Home` stays the editor's here.
        KeyCode::Home if !ctrl => ComposeAction::PreviewTop,
        KeyCode::End if !ctrl => ComposeAction::PreviewBottom,
        KeyCode::PageUp if !ctrl => ComposeAction::PreviewPageUp,
        KeyCode::PageDown if !ctrl => ComposeAction::PreviewPageDown,
        KeyCode::Enter if alt || shift => ComposeAction::Newline,
        KeyCode::Enter | KeyCode::Tab if list_open => ComposeAction::AcceptCompletion,
        KeyCode::Up if list_open => ComposeAction::CompletionPrev,
        KeyCode::Down if list_open => ComposeAction::CompletionNext,
        KeyCode::Esc if list_open => ComposeAction::CloseCompletion,
        KeyCode::Enter => ComposeAction::Send,
        KeyCode::Esc => ComposeAction::Cancel,
        _ => ComposeAction::Forward,
    }
}

/// Open the REPLY compose zone for `session_id`, BRINGING BACK a hidden preview
/// (a 1:0 board opens at 1:1 — see [`App::open_compose`]), since the compose zone
/// docks in the preview pane, or falls back to a full-width bottom bar on a short
/// terminal — the renderer decides. `stop_job` is the job id to
/// `claude stop` before sending, or `None` for a plain in-place reply. The reply
/// gate (and, for a waiting agent, the stop confirmation) has already run at the
/// call site (`Ctrl-R` in `update`).
pub fn open(app: &mut App, session_id: String, stop_job: Option<String>) {
    // No draft card: a reply previews the REAL session it is addressed to.
    app.open_compose(ComposeState::new_reply(session_id, stop_job), None);
}

/// Open the BACKGROUND-AGENT draft pane for `agent` (`None` = the picker's
/// "default (no agent)" row), bringing back a hidden preview exactly like [`open`].
///
/// The DEFAULT destination of `Ctrl-N`, reached two ways (`update`): the agent
/// picker's `Enter` confirm, which has already closed the picker — the draft pane
/// replaces it as the keyboard owner — and the no-agent fast path, which had no
/// picker to close.
///
/// Installs the editor AND the pane-level [`NewSessionDraft`] card together, so
/// the preview shows a PLACEHOLDER for the session about to exist rather than the
/// transcript of whichever row happened to be selected.
pub fn open_background(app: &mut App, agent: Option<String>) {
    app.open_compose(
        ComposeState::new_background(agent.clone()),
        Some(NewSessionDraft {
            agent,
            launch_id: None,
        }),
    );
}

/// Apply one keypress while the compose zone owns the keyboard.
///
/// Newline inserts into the editor; Forward hands the keystroke to the editor's
/// FULL key handler ([`TextArea::input`]); Cancel clears the compose state; Send
/// resolves the buffer through [`submit_compose`] (a reply send, or a background
/// launch); OpenInteractive escalates a background draft to the interactive
/// hand-off; and PickModel opens the model picker over this compose
/// (`App::open_model_picker`), which leaves the draft — text and pick alike —
/// exactly as it was until the picker's `Enter` writes a new pick into it.
///
/// Forward uses `input` (not `input_without_shortcuts`) so arrows/Home/End actually
/// MOVE the caret and word-delete works — `input_without_shortcuts` handles only
/// insert/delete of single characters, so with it the caret cannot move at all.
/// Using the full handler is safe because the router has already claimed the keys
/// with editor meaning we override: `Enter` (Send) and the newline chords never
/// reach `input`, so it never mistakes `Enter` for a newline.
pub fn handle_compose_key(app: &mut App, key: KeyEvent) -> Outcome {
    // Compose owns the keyboard while open; clear any transient status first, the
    // same way `update::dispatch` clears before a board action. The Enter that sets
    // a nudge still leaves it visible, and the next keystroke clears it.
    app.clear_status();
    let list_open = app
        .compose
        .as_ref()
        .is_some_and(|c| c.completion.visible.is_some());
    match compose_key_to_action(key, list_open) {
        ComposeAction::Newline => {
            if let Some(compose) = app.compose.as_mut() {
                compose.textarea.insert_newline();
            }
            refresh_completion(app);
            Outcome::Continue
        }
        ComposeAction::Forward => {
            if let Some(compose) = app.compose.as_mut() {
                compose.textarea.input(key);
            }
            refresh_completion(app);
            Outcome::Continue
        }
        ComposeAction::AcceptCompletion => {
            if let Some(compose) = app.compose.as_mut() {
                accept_completion(compose);
            }
            refresh_completion(app);
            Outcome::Continue
        }
        ComposeAction::CompletionPrev => {
            step_highlight(app, false);
            refresh_completion(app);
            Outcome::Continue
        }
        ComposeAction::CompletionNext => {
            step_highlight(app, true);
            refresh_completion(app);
            Outcome::Continue
        }
        ComposeAction::CloseCompletion => {
            if let Some(compose) = app.compose.as_mut() {
                let state = &mut compose.completion;
                state.dismissed = state.context.as_ref().map(|c| (c.trigger, c.row, c.start));
                state.visible = None;
            }
            refresh_completion(app);
            Outcome::Continue
        }
        ComposeAction::Cancel => {
            // Esc drops the editor AND the draft card together — one teardown, so a
            // cancelled draft can never leave a placeholder pane behind.
            app.close_compose();
            Outcome::Continue
        }
        ComposeAction::Send => submit_compose(app),
        ComposeAction::OpenInteractive => open_interactive(app),
        // The transcript the reply previews is the SELECTED row's, which is the reply's
        // target (STABLE-ID STATE: nothing here moves the selection), so the board's
        // own jump applies; the caret and text are not touched. A draft has no
        // transcript on screen, so the key reaches the editor as it always did.
        scroll @ (ComposeAction::PreviewTop
        | ComposeAction::PreviewBottom
        | ComposeAction::PreviewPageUp
        | ComposeAction::PreviewPageDown
        | ComposeAction::PreviewHalfUp
        | ComposeAction::PreviewHalfDown) => {
            let replying = matches!(
                app.compose.as_ref().map(|c| &c.target),
                Some(ComposeTarget::Reply { .. })
            );
            if !replying {
                if let Some(compose) = app.compose.as_mut() {
                    compose.textarea.input(key);
                }
            } else {
                // The board's own methods, so follow-bottom behaves identically.
                match scroll {
                    ComposeAction::PreviewTop => app.preview_top(),
                    ComposeAction::PreviewBottom => app.preview_bottom(),
                    ComposeAction::PreviewPageUp => app.preview_page_up(),
                    ComposeAction::PreviewPageDown => app.preview_page_down(),
                    ComposeAction::PreviewHalfUp => app.preview_half_up(),
                    _ => app.preview_half_down(),
                }
            }
            Outcome::Continue
        }
        ComposeAction::PickModel => {
            // A modal over the compose: it takes the keyboard until `Enter`/`Esc`,
            // and the compose underneath is untouched either way.
            app.open_model_picker();
            Outcome::Continue
        }
    }
}

/// Insert a terminal PASTE into the open draft, at the caret.
///
/// Lives here, next to [`handle_compose_key`], because this module owns every
/// `ratatui_textarea` reference — and because the insert is the whole fix. A paste
/// is TEXT, so it goes in through [`TextArea::insert_str`], which splits the string
/// on `\n` and inserts a real multi-line chunk. Routing it as keystrokes instead is
/// what made an ordinary Cmd+V destructive: the router maps a bare `Enter` to
/// [`ComposeAction::Send`], so the first embedded newline SENT the draft's first
/// line and dropped the rest onto the board.
///
/// There is deliberately no [`Outcome`] here. Every compose exit
/// (Send / OpenInteractive / Cancel) is reachable only from
/// [`compose_key_to_action`], so a paste STRUCTURALLY cannot submit, launch, or
/// close the draft — it can only edit the buffer. `text` is already normalized and
/// capped by `update::accept_paste`; a no-op on an empty string, and on a paste that
/// arrives with no draft open (the caller checks, but the `if let` keeps this total).
pub fn insert_paste(app: &mut App, text: &str) {
    if let Some(compose) = app.compose.as_mut() {
        compose.textarea.insert_str(text);
    }
    // A pasted `/` or `@` opens the list like a typed one; still no `Outcome`.
    refresh_completion(app);
}

/// Recompute the open draft's pick list from its editor, reply and background
/// draft alike, from the source [`completion_source`] names and the folder's
/// cached catalog; a reply whose session left the store gets no list.
///
/// Its bounded reads ([`CompletionState::refresh`]: the transcript listing, once
/// per reply draft and only for a top-level `@`, and folder listings, once
/// per folder on both targets) run from the key and paste handlers and from
/// `update::dispatch`'s `CatalogFetched` arm — which calls this so a catalog that
/// lands mid-draft reaches an open list — and never from the render path.
///
/// Only the source's two paths are cloned out (clone-then-mutate); the catalog is
/// BORROWED, `app.catalogs` beside `app.compose` as disjoint fields, so no list is
/// copied per keystroke.
pub(crate) fn refresh_completion(app: &mut App) {
    let Some(compose) = app.compose.as_ref() else {
        return;
    };
    let source = completion_source(app, &compose.target);
    let catalog = source.as_ref().and_then(|s| app.catalogs.get(&s.cwd));
    let Some(compose) = app.compose.as_mut() else {
        return;
    };
    let view = source.as_ref().map(|s| SourceView {
        cwd: &s.cwd,
        transcript: s.transcript.as_deref(),
        catalog,
    });
    compose.completion.refresh(
        compose.textarea.lines(),
        cursor_pos(&compose.textarea),
        view,
    );
}

/// The folder whose catalog the driver should fetch now, if any: `tui::run_inner`
/// asks after every handled event and spawns `claude_catalog::spawn_fetch` on an
/// answer, so the decision stays here, pure and tested, and no key handler ever
/// spawns.
///
/// Each compose ASKS ONCE, the first time it is asked with a source: it is marked
/// `catalog_requested` whatever the answer, and the answer is the source's `cwd`
/// only when that folder is neither cached in `app.catalogs` nor already in
/// `app.catalogs_in_flight` — which it then joins. A compose that finds the fetch
/// already running waits on it rather than asking again when it fails.
///
/// Retry policy: a failed fetch is never cached (`App::finish_catalog_fetch`), so
/// the NEXT compose on that folder asks again, while one compose never asks twice —
/// at most one spawn per compose opening, never a storm on a missing `claude`.
pub(crate) fn take_catalog_fetch(app: &mut App) -> Option<PathBuf> {
    let compose = app.compose.as_ref()?;
    if compose.completion.catalog_requested {
        return None;
    }
    let cwd = completion_source(app, &compose.target)?.cwd;
    if let Some(compose) = app.compose.as_mut() {
        compose.completion.catalog_requested = true;
    }
    if app.catalogs.contains_key(&cwd) || !app.catalogs_in_flight.insert(cwd.clone()) {
        return None;
    }
    Some(cwd)
}

/// The caret as a plain `(row, col)` in CHARACTERS (the widget wraps it in `DataCursor`).
fn cursor_pos(textarea: &TextArea<'static>) -> (usize, usize) {
    let c = textarea.cursor();
    (c.0, c.1)
}

/// Move the pick-list highlight one row, wrapping.
fn step_highlight(app: &mut App, forward: bool) {
    if let Some(compose) = app.compose.as_mut() {
        let len = compose.completion.visible.as_ref().map_or(0, Vec::len);
        compose.completion.highlight = move_highlight(compose.completion.highlight, len, forward);
    }
}

/// Replace the typed query with the highlighted pick: move the caret back over the
/// query, delete it, insert the [`replacement`]. Caret columns are CHARACTERS, so
/// this counts characters, never bytes. The token ends at the caret by
/// construction (`completion_context`), so whatever follows is whitespace or
/// nothing.
fn accept_completion(compose: &mut ComposeState) {
    let state = &compose.completion;
    let (Some(ctx), Some(visible)) = (state.context.as_ref(), state.visible.as_ref()) else {
        return;
    };
    let Some(candidate) = visible.get(state.highlight) else {
        return;
    };
    let dir_part = match ctx.trigger {
        Trigger::Slash => "",
        Trigger::At => split_path_query(&ctx.query).0,
    };
    let (row, col) = cursor_pos(&compose.textarea);
    let next_is_whitespace = compose
        .textarea
        .lines()
        .get(row)
        .is_some_and(|l| l.chars().nth(col).is_some());
    let text = replacement(ctx.trigger, dir_part, candidate, next_is_whitespace);
    let query_chars = ctx.query.chars().count();
    for _ in 0..query_chars {
        compose.textarea.move_cursor(CursorMove::Back);
    }
    compose.textarea.delete_str(query_chars);
    compose.textarea.insert_str(text);
}

/// The open draft as its submit paths need it: the text, what it is addressed to,
/// and the model picked for it (`None` = its default, no `--model`). Cloned out so
/// the borrow of the app ends before anything mutates it — the clone-then-mutate
/// discipline every other handler here follows.
struct Draft {
    /// The editor buffer, lines joined with `\n`.
    message: String,
    /// What the draft is addressed to.
    target: ComposeTarget,
    /// The compose's own model pick ([`ComposeState::model`]).
    model: Option<ModelPick>,
}

/// Read the open draft out of the app (see [`Draft`]).
fn draft(app: &App) -> Option<Draft> {
    let compose = app.compose.as_ref()?;
    Some(Draft {
        message: compose.textarea.lines().join("\n"),
        target: compose.target.clone(),
        model: compose.model.clone(),
    })
}

/// Resolve the compose buffer into a driver [`Outcome`], routing on the open
/// [`ComposeTarget`]: a reply sends, a background draft launches — each with the
/// model picked in THIS compose, if any.
fn submit_compose(app: &mut App) -> Outcome {
    let Some(Draft {
        message,
        target,
        model,
    }) = draft(app)
    else {
        return Outcome::Continue;
    };
    match target {
        ComposeTarget::Reply {
            session_id,
            stop_job,
        } => submit_reply(app, message, session_id, stop_job, model.as_ref()),
        ComposeTarget::NewBackgroundAgent { agent } => {
            submit_bg_launch(app, message, agent, model.as_ref())
        }
    }
}

/// Launch the drafted background agent — the `Enter` half of the draft pane.
///
/// It rides the [`crate::send`] family, NOT [`Outcome::Resume`]: there is no
/// terminal teardown, so the board stays up and the result arrives as an
/// [`AppEvent::BgLaunchFinished`](crate::watch::AppEvent::BgLaunchFinished).
/// An empty/whitespace draft keeps the pane open with a gentle nudge (a background
/// agent with no prompt would do nothing); otherwise the launch dir is gated
/// ([`send::plan_bg_launch`]), the argv built, the draft card is marked launching
/// in place, and a [`BgLaunchRequest`] handed to the driver.
///
/// The ONE thing recorded is the AGENT, as the pick `Ctrl-N`'s picker pre-highlights
/// next time — because this is a real launch, and that memory means "the agent of
/// the last new session actually started". It is written past the empty-buffer nudge
/// (a draft that launched nothing is not a start) and before the launch-dir gate, so
/// it survives a refusal, exactly like the picker's `Ctrl-O`.
///
/// Nothing ELSE is recorded about the launch: no virtual/pending row is created and
/// no attempt is made to reconcile the short job id back to a `sessionId`. The new
/// agent reaches the board through the existing watcher → reload path, and its own
/// transcript already records which agent it is.
///
/// The model picked in THIS draft (`model`, from `Ctrl-L`) rides along into the argv
/// beside the agent, and DELIBERATELY outranks any `model:` the agent definition
/// declares — the same precedence [`crate::resume::build_new_argv`] argues for the
/// interactive twin, so the draft means the same thing whichever key launches it.
/// With no pick (`None`) no `--model` is sent and the argv is unchanged: the new
/// session starts on the model the user's `claude` settings name.
fn submit_bg_launch(
    app: &mut App,
    message: String,
    agent: Option<String>,
    model: Option<&ModelPick>,
) -> Outcome {
    if message.trim().is_empty() {
        // Nothing to run: keep the draft pane open so the user can type.
        app.set_status_transient(COMPOSE_EMPTY_BG_HINT);
        return Outcome::Continue;
    }
    app.set_last_new_agent(agent.clone());
    match send::plan_bg_launch(&app.launch_dir) {
        Ok(cwd) => {
            let argv = send::build_bg_launch_argv(agent.as_deref(), model, &message);
            // The editor closes but the CARD stays, marked in flight: there is
            // nothing left to type, yet still no session to preview, so the
            // placeholder reports the launch until THIS launch's `BgLaunchFinished`
            // ends it. The id the card is stamped with rides out on the request so
            // the completion can be matched back to it rather than to whatever
            // surface is open by then.
            let launch_id = app.dispatch_draft();
            Outcome::BgLaunch(BgLaunchRequest {
                launch_id,
                argv,
                cwd,
            })
        }
        Err(refusal) => {
            // Nothing was dispatched, so there is nothing for a card to report.
            app.close_compose();
            app.set_status(refusal);
            Outcome::Continue
        }
    }
}

/// Run the drafted background agent INTERACTIVELY instead (`Ctrl-O`) — the one
/// action in the compose zone that leaves the board.
///
/// Unlike `Enter` this is an ordinary hand-off, and it builds NO argv of its own:
/// it delegates to [`super::update::launch_new_session`] — the same seam the
/// picker's OWN `Ctrl-O` uses — which runs [`crate::resume::check_new`] over the
/// existing `SessionAction::New` / `HandoffCtx` / `argv_for` machinery. The draft
/// becomes `claude [--agent <name>] [--model <alias> [--effort <level>]] <prompt>`
/// through the IDENTICAL teardown → spawn → wait → return round trip as every other
/// `Outcome::Resume`. An EMPTY draft with no pick launches bare (no positional), i.e.
/// exactly what the picker's `Ctrl-O` emits.
///
/// That shared seam is the point of the shared key: `Ctrl-O` means "open
/// interactive claude" on the picker and in the draft alike, so a user who wants
/// the terminal pays one keypress either way.
///
/// This IS a launch — bare draft or not — so it records the agent as the last new
/// session started, the same memory the picker's `Ctrl-O` and the `--bg` submit
/// write. Only real launches do; a draft that is opened and cancelled leaves it
/// alone.
///
/// The prompt AUTO-SUBMITS as the session's first turn — the trailing positional is
/// the only mechanism claude offers (see [`crate::resume::build_new_argv`]), which
/// is why every user-facing string for this key says "run interactively" and never
/// promises a chance to review or edit it inside claude.
///
/// The model picked in THIS draft (`Ctrl-L`) rides along too, as `--model` [and
/// `--effort`] ahead of the prompt, exactly as it would on the background launch:
/// the draft chose its model, and which key starts it does not change that. With no
/// pick the argv carries no model, like the picker's own `Ctrl-O` — which skips the
/// draft and so never has a pick to pass.
///
/// INERT on a [`ComposeTarget::Reply`]: a reply addresses a session that already
/// exists, so there is no new-session launch to escape to.
fn open_interactive(app: &mut App) -> Outcome {
    let Some(Draft {
        message,
        target: ComposeTarget::NewBackgroundAgent { agent },
        model,
    }) = draft(app)
    else {
        return Outcome::Continue; // no interactive launch on the reply target
    };
    // An empty / whitespace draft launches BARE — no positional at all — which is
    // exactly what the picker's own `Ctrl-O` emits.
    let prompt = (!message.trim().is_empty()).then_some(message);
    // The board is about to be torn down for the interactive child, so the card has
    // nothing left to report: close the whole surface.
    app.close_compose();
    app.set_last_new_agent(agent.clone());
    super::update::launch_new_session(app, agent.as_deref(), prompt.as_deref(), model.as_ref())
}

/// Send the drafted quick reply — the `Enter` half of the reply target.
///
/// Guards an empty/whitespace buffer (keep composing, gentle status). Otherwise it
/// re-reads the AUTHORITATIVE `(cwd, session_id)` from inside the file
/// ([`send::plan_send`]) — never the stale in-memory copy — builds the argv,
/// marks the send in flight, clears the compose state, and hands a [`SendRequest`]
/// to the driver as [`Outcome::Send`]. A refusal (deleted worktree / unreadable file)
/// sets a board status and stays on the board.
///
/// The model picked in THIS reply box (`model`, from `Ctrl-L`) rides along into the
/// argv, and this path is the one where it is not merely a convenience:
/// `claude -p` is non-interactive, so the in-session `/model` command cannot reach
/// it and `--model` is the ONLY way to choose a model for a quick reply. With no
/// pick (`None`) the argv is unchanged and carries no `--model`, so claude normally
/// restores the model the session last answered with — the `model: session (…)`
/// the box showed. After a picked reply claude keeps the session on that model by
/// itself (the next `-r` normally restores it); its effort is not kept.
fn submit_reply(
    app: &mut App,
    message: String,
    session_id: String,
    stop_job: Option<String>,
    model: Option<&ModelPick>,
) -> Outcome {
    if message.trim().is_empty() {
        // Nothing to send: keep the compose zone open so the user can type.
        app.set_status_transient(COMPOSE_EMPTY_HINT);
        return Outcome::Continue;
    }

    let (file, baseline_msg_count) = match app.session_by_id(&session_id) {
        Some(session) => (session.file.clone(), session.msg_count),
        None => {
            app.close_compose();
            app.set_status(COMPOSE_SESSION_GONE);
            return Outcome::Continue;
        }
    };

    match send::plan_send(&file) {
        SendPlan::Ready {
            cwd,
            session_id: authoritative_id,
        } => {
            let argv = send::build_send_argv(&authoritative_id, model, &message);
            app.close_compose();
            // Mark the send in flight so the preview echoes the message under a
            // synthetic `▶ you` turn plus a live `cooking…` indicator until the
            // completion event lands. `baseline_msg_count` lets the echo step aside
            // the instant claude writes the real turn to disk. The entry is this
            // session's alone: a reply still in flight to another row keeps its own.
            app.mark_sending(super::app::Sending {
                session_id: authoritative_id.clone(),
                message,
                baseline_msg_count,
            });
            Outcome::Send(SendRequest {
                argv,
                cwd,
                session_id: authoritative_id,
                stop_job,
            })
        }
        SendPlan::Refuse(message) => {
            app.close_compose();
            app.set_status(message);
            Outcome::Continue
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::store::Session;
    use crate::tui::app::Scope;

    /// Rows the tests below draw the editor into. The widget's screen map — and so
    /// [`ComposeState::screen_rows`] — is built from the drawn WIDTH alone, never the
    /// height, so one row seeds everything these assertions need and keeps the
    /// scratch buffer tiny.
    const PROBE_RENDER_ROWS: u16 = 1;

    /// Draw `state`'s editor once at `width` columns, exactly as `render_compose_zone`
    /// does, so its screen map is built at a REAL width.
    ///
    /// Load-bearing setup, not ceremony: until the widget has been rendered its area
    /// is zero wide, the screen map is built with no wrapping at all, and every wrap
    /// assertion below would pass vacuously against the logical line count.
    fn draw_editor_at(state: &ComposeState, width: u16) {
        let area = ratatui::layout::Rect {
            x: 0,
            y: 0,
            width,
            height: PROBE_RENDER_ROWS,
        };
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::Widget::render(&state.textarea, area, &mut buffer);
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn with_mods(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    /// A bare `Enter` sends — the whole reason the router owns `Enter` rather than
    /// forwarding it to the widget (which would insert a newline).
    #[test]
    fn bare_enter_sends() {
        assert_eq!(
            compose_key_to_action(key(KeyCode::Enter), false),
            ComposeAction::Send
        );
    }

    /// `Ctrl-J` is the primary newline chord — and the form crossterm 0.29
    /// actually delivers it as in raw mode: `Char('j')`+`CONTROL`.
    #[test]
    fn ctrl_j_inserts_a_newline() {
        assert_eq!(
            compose_key_to_action(with_mods(KeyCode::Char('j'), KeyModifiers::CONTROL), false),
            ComposeAction::Newline,
        );
        // Uppercase 'J' too, for the kitty path's sake.
        assert_eq!(
            compose_key_to_action(with_mods(KeyCode::Char('J'), KeyModifiers::CONTROL), false),
            ComposeAction::Newline,
        );
    }

    /// `Alt+Enter` is the GUARANTEED newline fallback (needs no keyboard protocol).
    #[test]
    fn alt_enter_inserts_a_newline() {
        assert_eq!(
            compose_key_to_action(with_mods(KeyCode::Enter, KeyModifiers::ALT), false),
            ComposeAction::Newline,
        );
    }

    /// `Shift+Enter` is honored opportunistically: if a terminal reports it
    /// distinctly (a kitty protocol the user enabled), it inserts a newline. We
    /// enable no such protocol ourselves, so under snapback's own setup this arm
    /// never fires — but wiring it is free and correct where it IS delivered.
    #[test]
    fn shift_enter_inserts_a_newline_when_delivered_distinctly() {
        assert_eq!(
            compose_key_to_action(with_mods(KeyCode::Enter, KeyModifiers::SHIFT), false),
            ComposeAction::Newline,
        );
    }

    /// `Esc` cancels compose — never the app (the app-level `Esc`-quits binding is
    /// bypassed while the compose zone owns the keyboard).
    #[test]
    fn esc_cancels() {
        assert_eq!(
            compose_key_to_action(key(KeyCode::Esc), false),
            ComposeAction::Cancel
        );
    }

    /// A plain character is ordinary typing — forwarded to the editor, NOT any
    /// chord. In particular a bare `j` (no Ctrl) types a `j` rather than a newline.
    #[test]
    fn a_plain_char_is_forwarded() {
        assert_eq!(
            compose_key_to_action(key(KeyCode::Char('a')), false),
            ComposeAction::Forward
        );
        assert_eq!(
            compose_key_to_action(key(KeyCode::Char('j')), false),
            ComposeAction::Forward,
            "a bare `j` types a `j`; only Ctrl-J is the newline chord"
        );
    }

    /// Editing keys (Backspace, arrows) forward to the widget, which owns cursor
    /// movement and deletion.
    #[test]
    fn editing_keys_are_forwarded() {
        for code in [
            KeyCode::Backspace,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Up,
            KeyCode::Down,
        ] {
            assert_eq!(
                compose_key_to_action(key(code), false),
                ComposeAction::Forward,
                "{code:?} must forward to the editor"
            );
        }
    }

    /// With the pick list showing, `Enter`/`Tab` pick, `Up`/`Down` move the
    /// highlight and `Esc` closes the list only.
    #[test]
    fn an_open_list_claims_enter_tab_arrows_and_esc() {
        let open = |code| compose_key_to_action(key(code), true);
        assert_eq!(open(KeyCode::Enter), ComposeAction::AcceptCompletion);
        assert_eq!(open(KeyCode::Tab), ComposeAction::AcceptCompletion);
        assert_eq!(open(KeyCode::Up), ComposeAction::CompletionPrev);
        assert_eq!(open(KeyCode::Down), ComposeAction::CompletionNext);
        assert_eq!(open(KeyCode::Esc), ComposeAction::CloseCompletion);
        assert_eq!(open(KeyCode::Char('a')), ComposeAction::Forward);
    }

    /// The chords keep their meaning while the list is open, and a closed list
    /// leaves `Tab`/`Up`/`Down` to the editor.
    #[test]
    fn chords_are_unchanged_by_an_open_list_and_a_closed_list_claims_nothing() {
        for (k, want) in [
            (
                with_mods(KeyCode::Char('j'), KeyModifiers::CONTROL),
                ComposeAction::Newline,
            ),
            (
                with_mods(KeyCode::Enter, KeyModifiers::ALT),
                ComposeAction::Newline,
            ),
            (
                with_mods(KeyCode::Enter, KeyModifiers::SHIFT),
                ComposeAction::Newline,
            ),
            (
                with_mods(KeyCode::Char('o'), KeyModifiers::CONTROL),
                ComposeAction::OpenInteractive,
            ),
        ] {
            assert_eq!(compose_key_to_action(k, true), want);
            assert_eq!(compose_key_to_action(k, false), want);
        }
        for code in [KeyCode::Tab, KeyCode::Up, KeyCode::Down] {
            assert_eq!(
                compose_key_to_action(key(code), false),
                ComposeAction::Forward
            );
        }
    }

    /// `Ctrl-O` decodes to the interactive escape hatch, alongside the existing
    /// chords. A bare `o` is still ordinary typing — only the Ctrl form is claimed.
    #[test]
    fn ctrl_o_runs_interactively() {
        assert_eq!(
            compose_key_to_action(with_mods(KeyCode::Char('o'), KeyModifiers::CONTROL), false),
            ComposeAction::OpenInteractive,
        );
        // Uppercase 'O' too, for the kitty path's sake (like the Ctrl-J arm).
        assert_eq!(
            compose_key_to_action(with_mods(KeyCode::Char('O'), KeyModifiers::CONTROL), false),
            ComposeAction::OpenInteractive,
        );
        assert_eq!(
            compose_key_to_action(key(KeyCode::Char('o')), false),
            ComposeAction::Forward,
            "a bare `o` types an `o`; only Ctrl-O is the interactive chord"
        );
    }

    /// `Ctrl-L` decodes to this compose's model picker, in both cases for the kitty
    /// path's sake, and ONLY with Ctrl: a bare `l` (and a shifted `L`) still types,
    /// since a compose box is a text field first.
    #[test]
    fn ctrl_l_picks_the_model_and_a_bare_l_still_types() {
        assert_eq!(
            compose_key_to_action(with_mods(KeyCode::Char('l'), KeyModifiers::CONTROL), false),
            ComposeAction::PickModel,
        );
        assert_eq!(
            compose_key_to_action(with_mods(KeyCode::Char('L'), KeyModifiers::CONTROL), false),
            ComposeAction::PickModel,
        );
        assert_eq!(
            compose_key_to_action(key(KeyCode::Char('l')), false),
            ComposeAction::Forward,
            "a bare `l` types an `l`; only Ctrl-L opens the picker"
        );
        assert_eq!(
            compose_key_to_action(with_mods(KeyCode::Char('L'), KeyModifiers::SHIFT), false),
            ComposeAction::Forward,
            "a shifted `L` types an `L`"
        );
    }

    /// The key is free for a REASON, and the reason is checked rather than recited:
    /// the pinned `ratatui_textarea` really does nothing with `Ctrl-L`, so routing it
    /// away from the editor takes no editing gesture from the user. Fed to the
    /// widget's own full key map, the key leaves the text and the caret exactly as
    /// they were — while `Ctrl-K` (a key the widget DOES bind) is the control that
    /// proves this probe can see an edit at all.
    #[test]
    fn the_editor_itself_binds_nothing_on_ctrl_l() {
        let mut state = ComposeState::new_background(None);
        state.textarea.insert_str("keep this");
        state.textarea.move_cursor(CursorMove::Head);
        let before = (state.textarea.lines().to_vec(), state.textarea.cursor());

        let modified = state
            .textarea
            .input(with_mods(KeyCode::Char('l'), KeyModifiers::CONTROL));
        assert!(!modified, "Ctrl-L edits nothing in ratatui_textarea");
        assert_eq!(
            (state.textarea.lines().to_vec(), state.textarea.cursor()),
            before,
            "Ctrl-L neither edits the text nor moves the caret"
        );

        let control = state
            .textarea
            .input(with_mods(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert!(
            control && state.textarea.lines() != before.0.as_slice(),
            "control: a key the editor binds (Ctrl-K, delete to line end) does edit"
        );
    }

    /// The target enum is the ONLY thing that distinguishes the two drafts, and it
    /// makes the reply-shaped fields STRUCTURALLY absent from a background draft:
    /// there is no session id (claude has not minted one) and no stop job (nothing
    /// is being resumed, so nothing needs deregistering), rather than `None`s that
    /// a future edit could quietly start filling in.
    #[test]
    fn each_constructor_builds_only_its_own_target() {
        assert_eq!(
            ComposeState::new_reply("sess-1".to_string(), Some("job-1".to_string())).target,
            ComposeTarget::Reply {
                session_id: "sess-1".to_string(),
                stop_job: Some("job-1".to_string()),
            }
        );
        assert_eq!(
            ComposeState::new_reply("sess-1".to_string(), None).target,
            ComposeTarget::Reply {
                session_id: "sess-1".to_string(),
                stop_job: None,
            },
            "a plain reply carries no stop job"
        );
        assert_eq!(
            ComposeState::new_background(Some("planner".to_string())).target,
            ComposeTarget::NewBackgroundAgent {
                agent: Some("planner".to_string())
            }
        );
        // The picker's "default (no agent)" row carries `None`, not a blank name.
        assert_eq!(
            ComposeState::new_background(None).target,
            ComposeTarget::NewBackgroundAgent { agent: None }
        );
    }

    /// An empty draft occupies exactly one screen row — the height the compose box
    /// opens at.
    #[test]
    fn screen_rows_is_one_for_an_empty_draft() {
        let state = ComposeState::new_background(None);
        draw_editor_at(&state, 40);
        assert_eq!(state.screen_rows(), 1);
    }

    /// Every logical line costs a row, wrapping or not — the floor the probe can
    /// never report below.
    #[test]
    fn screen_rows_counts_every_logical_line() {
        let mut state = ComposeState::new_background(None);
        state.textarea.insert_str("one");
        state.textarea.insert_newline();
        state.textarea.insert_str("two");
        state.textarea.insert_newline();
        state.textarea.insert_str("three");
        // Wide enough that nothing here wraps, so only the line count is in play.
        draw_editor_at(&state, 40);
        assert_eq!(state.screen_rows(), 3);

        // A trailing newline is a real (empty) line and gets its own row.
        state.textarea.insert_newline();
        draw_editor_at(&state, 40);
        assert_eq!(state.screen_rows(), 4);
    }

    /// The load-bearing one: the editor WORD-wraps, so a draft of medium words in a
    /// narrow box needs MORE rows than the character-packing `ceil(width / inner)`
    /// model the view used to apply — each row ends early at a word boundary and
    /// leaves its tail unused.
    ///
    /// The gap is exactly the bug this accessor exists to close: sized by the ceil
    /// model the box grew to 3 rows while the editor needed 4, so the editor scrolled
    /// its own first row out of view.
    #[test]
    fn screen_rows_counts_word_wrapped_rows_not_packed_characters() {
        // 4 x 6-char words + 3 spaces = 27 columns, in a 10-column editor.
        let draft = "abcdef abcdef abcdef abcdef";
        let editor_width = 10u16;

        let mut state = ComposeState::new_background(None);
        state.textarea.insert_str(draft);
        draw_editor_at(&state, editor_width);

        // Word wrap fits ONE word plus its space per row: 4 rows.
        assert_eq!(state.screen_rows(), 4);
        // What the old model said: ceil(27 / 10) = 3 — one row short of the truth.
        let packed = draft.len().div_ceil(usize::from(editor_width));
        assert_eq!(packed, 3);
        assert!(
            state.screen_rows() > packed,
            "word wrap must out-count character packing here, or this pins nothing"
        );
    }

    /// A long UNBROKEN word still wraps (the `WordOrGlyph` grapheme fallback), so a
    /// draft with no spaces at all is measured too rather than reported as one row.
    #[test]
    fn screen_rows_counts_a_word_too_long_to_break() {
        let mut state = ComposeState::new_background(None);
        state.textarea.insert_str("x".repeat(25));
        draw_editor_at(&state, 10);
        assert_eq!(state.screen_rows(), 3, "25 columns at width 10 is 3 rows");
    }

    /// Documented degradation, pinned so it stays a known shape rather than a
    /// surprise: before the editor has EVER been drawn its area is zero wide, the
    /// widget wraps nothing, and the probe falls back to the logical line count.
    /// Harmless in the board's own timing (the draft is empty on that one frame) and
    /// the next redraw self-corrects.
    #[test]
    fn screen_rows_degrades_to_logical_lines_before_the_editor_is_ever_drawn() {
        let mut state = ComposeState::new_background(None);
        state.textarea.insert_str("abcdef abcdef abcdef abcdef");
        // Deliberately NOT drawn.
        assert_eq!(state.screen_rows(), 1);
    }

    /// The probe must not move the user's caret: it runs on a throwaway clone, so the
    /// real cursor is exactly where it was.
    #[test]
    fn screen_rows_leaves_the_editors_own_cursor_alone() {
        let mut state = ComposeState::new_background(None);
        state.textarea.insert_str("alpha");
        state.textarea.insert_newline();
        state.textarea.insert_str("bravo");
        state.textarea.move_cursor(CursorMove::Top);
        draw_editor_at(&state, 40);

        let before = state.textarea.cursor();
        let _ = state.screen_rows();
        assert_eq!(
            state.textarea.cursor(),
            before,
            "measuring the draft must not relocate the caret"
        );
    }

    /// Task 3.11: `Enter` on an empty reply buffer sets a transient nudge, and the
    /// NEXT compose keystroke clears it so the compose hint is visible again.
    #[test]
    fn empty_enter_nudge_is_cleared_by_next_compose_keystroke() {
        let mut app = App::new(
            vec![Session {
                file: PathBuf::from("/tmp/s.jsonl"),
                session_id: "s".to_string(),
                cwd: PathBuf::from("/tmp/s"),
                git_branch: Some("main".to_string()),
                timestamp: None,
                repo: "repo".to_string(),
                label: "label s".to_string(),
                root_uuid: None,
                msg_count: 0,
                content_index: String::new(),
                background: false,
                has_agent_name: false,
                has_agent_setting: false,
                failed_task: None,
            }],
            Scope::All,
            PathBuf::from("/tmp"),
        );
        app.open_compose(ComposeState::new_reply("s".to_string(), None), None);

        // Empty buffer: Enter sets the transient nudge.
        let out = handle_compose_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(out, Outcome::Continue));
        assert_eq!(app.status.as_deref(), Some(COMPOSE_EMPTY_HINT));
        assert!(app.status_ttl.is_some());

        // The next keystroke clears the status, mirroring `update::dispatch`.
        let out = handle_compose_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        );
        assert!(matches!(out, Outcome::Continue));
        assert!(
            app.status.is_none(),
            "the next compose keystroke must clear the nudge"
        );
        assert!(app.status_ttl.is_none());
    }

    /// An isolated temp dir for the send fixtures (PATTERNS: never touch the real
    /// `~/.claude/projects`).
    fn unique_temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the unix epoch")
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "snapback-compose-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// One row for `app.sessions`, pointed at `file`/`cwd`.
    fn session_at(id: &str, file: PathBuf, cwd: PathBuf) -> Session {
        Session {
            file,
            session_id: id.to_string(),
            cwd,
            git_branch: None,
            timestamp: None,
            repo: "repo".to_string(),
            label: format!("label {id}"),
            root_uuid: None,
            msg_count: 0,
            content_index: String::new(),
            background: false,
            has_agent_name: false,
            has_agent_setting: false,
            failed_task: None,
        }
    }

    /// The ONE source resolver: a reply reads its session's folder and transcript, a
    /// reply whose session left the store reads nothing, and a background draft
    /// reads the launch dir with no transcript.
    #[test]
    fn completion_source_is_the_sessions_folder_for_a_reply_and_the_launch_dir_for_a_draft() {
        let launch = PathBuf::from("/tmp/sbc-launch");
        let app = App::new(
            vec![session_at(
                "sbc-src",
                PathBuf::from("/tmp/sbc-src/s.jsonl"),
                PathBuf::from("/tmp/sbc-src"),
            )],
            Scope::All,
            launch.clone(),
        );
        let reply = |id: &str| ComposeTarget::Reply {
            session_id: id.to_string(),
            stop_job: None,
        };
        assert_eq!(
            completion_source(&app, &reply("sbc-src")),
            Some(CompletionSource {
                cwd: PathBuf::from("/tmp/sbc-src"),
                transcript: Some(PathBuf::from("/tmp/sbc-src/s.jsonl")),
            })
        );
        assert_eq!(completion_source(&app, &reply("sbc-gone")), None);
        for agent in [None, Some("planner".to_string())] {
            assert_eq!(
                completion_source(&app, &ComposeTarget::NewBackgroundAgent { agent }),
                Some(CompletionSource {
                    cwd: launch.clone(),
                    transcript: None,
                })
            );
        }
    }

    /// A refresh with no source (the reply's session left the store) clears the
    /// list but keeps the compose's one catalog ask spent.
    #[test]
    fn a_sourceless_refresh_clears_the_list_but_keeps_the_ask_spent() {
        let mut state = CompletionState {
            visible: Some(vec![Candidate {
                label: "/sbping".to_string(),
                insert: "sbping".to_string(),
                description: None,
            }]),
            catalog_requested: true,
            ..CompletionState::default()
        };
        state.refresh(&["/sb".to_string()], (0, 3), None);
        assert!(state.visible.is_none());
        assert!(state.catalog_requested);
    }

    /// The transcript fixture whose listing names the commands `brag-slim`,
    /// `cr-review`, `plugin-x:deploy`, `sbx-skill` and the agents `Explore`,
    /// `sby-agent`.
    fn listing_fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/skill_listing/listing.jsonl")
    }

    /// Refresh `state` for a draft holding `token` alone with the caret at its end,
    /// reading from `cwd` and `transcript` with no catalog landed.
    fn refresh_token(
        state: &mut CompletionState,
        token: &str,
        cwd: &std::path::Path,
        transcript: &std::path::Path,
    ) {
        let source = SourceView {
            cwd,
            transcript: Some(transcript),
            catalog: None,
        };
        state.refresh(
            &[token.to_string()],
            (0, token.chars().count()),
            Some(source),
        );
    }

    /// What `state`'s list would insert, row by row; empty when no list shows.
    fn shown(state: &CompletionState) -> Vec<String> {
        state
            .visible
            .iter()
            .flatten()
            .map(|c| c.insert.clone())
            .collect()
    }

    /// An `@` with a folder part lists that folder and never an agent
    /// (`complete::at_candidates`), so it never reads the reply's transcript the
    /// agents come from. A top-level `@` still reads it, in a fresh draft and in one
    /// whose earlier `@src/` skipped the read; a `/` never does, since it lists the
    /// catalog alone.
    #[test]
    fn a_folder_part_at_never_reads_the_transcript() {
        let cwd = unique_temp_dir("at-folder-part");
        std::fs::create_dir_all(cwd.join("src")).expect("create src/");
        std::fs::write(cwd.join("src").join("main.rs"), "").expect("write src/main.rs");
        let transcript = listing_fixture();

        let mut state = CompletionState::default();
        refresh_token(&mut state, "@src/", &cwd, &transcript);
        assert_eq!(
            shown(&state),
            ["main.rs"],
            "the folder part lists its folder"
        );
        assert!(
            state.transcript.is_none(),
            "`@src/` must not read the transcript"
        );

        refresh_token(&mut state, "@", &cwd, &transcript);
        assert!(state.transcript.is_some(), "a later top-level `@` reads it");
        assert_eq!(
            shown(&state),
            ["src/", "agent-Explore", "agent-sby-agent"],
            "and lists the transcript's agents"
        );

        let mut slash_after = CompletionState::default();
        refresh_token(&mut slash_after, "@src/", &cwd, &transcript);
        refresh_token(&mut slash_after, "/", &cwd, &transcript);
        assert!(
            slash_after.transcript.is_none(),
            "a later `/` must not read it"
        );
        assert!(
            shown(&slash_after).is_empty(),
            "and lists nothing while no catalog has landed"
        );

        let mut fresh_at = CompletionState::default();
        refresh_token(&mut fresh_at, "@", &cwd, &transcript);
        assert!(
            fresh_at.transcript.is_some(),
            "a fresh draft's `@` reads the transcript"
        );
        let mut fresh_slash = CompletionState::default();
        refresh_token(&mut fresh_slash, "/", &cwd, &transcript);
        assert!(
            fresh_slash.transcript.is_none(),
            "a fresh draft's `/` must not read the transcript"
        );
        assert!(shown(&fresh_slash).is_empty(), "and shows no list");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// A skipped read never costs a second one: the top-level `@` that reads the
    /// transcript after an `@src/` reads it for the whole draft, so a later
    /// top-level `@` lists from that read even once the file is gone.
    #[test]
    fn a_draft_reads_its_transcript_once_after_a_skipped_read() {
        let cwd = unique_temp_dir("at-read-once");
        std::fs::create_dir_all(cwd.join("src")).expect("create src/");
        let transcript = cwd.join("listing.jsonl");
        std::fs::copy(listing_fixture(), &transcript).expect("copy the listing fixture");

        let mut state = CompletionState::default();
        refresh_token(&mut state, "@src/", &cwd, &transcript);
        refresh_token(&mut state, "@sb", &cwd, &transcript);
        assert_eq!(shown(&state), ["agent-sby-agent"]);

        std::fs::remove_file(&transcript).expect("remove the transcript");
        refresh_token(&mut state, "@Ex", &cwd, &transcript);
        assert_eq!(
            shown(&state),
            ["agent-Explore"],
            "listed from the draft's one read, not from the file again"
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// `/` lists the folder's landed catalog alone: with none landed it shows
    /// nothing and never reads the reply's transcript, whose `skill_listing` is the
    /// model's list rather than claude's `/` menu.
    #[test]
    fn a_slash_lists_only_the_landed_catalog() {
        let transcript = listing_fixture();
        let cwd = transcript.parent().expect("the fixture has a folder");
        let refresh = |state: &mut CompletionState, token: &str, catalog: Option<&Listing>| {
            let source = SourceView {
                cwd,
                transcript: Some(&transcript),
                catalog,
            };
            state.refresh(
                &[token.to_string()],
                (0, token.chars().count()),
                Some(source),
            );
        };

        let mut state = CompletionState::default();
        for token in ["/", "/c"] {
            refresh(&mut state, token, None);
            assert!(
                shown(&state).is_empty(),
                "`{token}` shows nothing before the catalog lands"
            );
            assert!(
                state.transcript.is_none(),
                "`{token}` must not read the transcript"
            );
        }

        let catalog = Listing {
            commands: vec![crate::store::skills::ListingEntry {
                name: "sbping".to_string(),
                description: None,
            }],
            agents: Vec::new(),
        };
        refresh(&mut state, "/", Some(&catalog));
        assert_eq!(shown(&state), ["sbping"], "the landed catalog alone");
    }

    /// Every compose is BORN on its default model, whichever constructor opened it:
    /// the pick is per-compose state, so there is nothing a new box could inherit.
    #[test]
    fn every_compose_starts_on_its_default_model() {
        for state in [
            ComposeState::new_reply("sess-1".to_string(), None),
            ComposeState::new_reply("sess-1".to_string(), Some("job-1".to_string())),
            ComposeState::new_background(Some("planner".to_string())),
            ComposeState::new_background(None),
        ] {
            assert_eq!(
                state.model, None,
                "{:?} must open with no pick, so it sends no --model",
                state.target
            );
        }
    }

    /// A reply session file under `dir` that `send::plan_send` accepts (its in-file
    /// `cwd` is `dir`, which exists), plus the matching board row.
    fn sendable_session(dir: &std::path::Path, id: &str) -> Session {
        let file = dir.join(format!("{id}.jsonl"));
        std::fs::write(
            &file,
            format!(
                r#"{{"type":"user","sessionId":"{id}","cwd":"{}","message":{{"role":"user","content":"hi"}}}}"#,
                dir.display()
            ),
        )
        .expect("write the sendable fixture");
        session_at(id, file, dir.to_path_buf())
    }

    /// Type `text` into the open compose's editor.
    fn type_into(app: &mut App, text: &str) {
        app.compose
            .as_mut()
            .expect("a compose is open")
            .textarea
            .insert_str(text);
    }

    /// A pick made in THIS reply box reaches the QUICK REPLY argv, end to end from
    /// the compose state through the submit: `--model` then `--effort`, ahead of the
    /// positional message. Not redundant with `send`'s builder tests: those prove the
    /// formatter can carry a model, this proves the submit hands it the compose's
    /// own pick — the seam a picker that set state nothing reads would silently
    /// break. This path matters most, because `claude -p` is non-interactive and
    /// `/model` cannot reach it.
    #[test]
    fn a_compose_pick_reaches_the_quick_reply_argv() {
        let dir = unique_temp_dir("model-reply");
        let mut app = App::new(
            vec![sendable_session(&dir, "sbc-reply")],
            Scope::All,
            dir.clone(),
        );
        app.open_compose(ComposeState::new_reply("sbc-reply".to_string(), None), None);
        app.set_compose_model(Some(ModelPick {
            model: "opus".to_string(),
            effort: Some("xhigh"),
        }));
        type_into(&mut app, "ship it");

        match handle_compose_key(&mut app, key(KeyCode::Enter)) {
            Outcome::Send(req) => assert_eq!(
                req.argv.join(" "),
                "claude -p -r sbc-reply --output-format json --model opus --effort xhigh ship it"
            ),
            _ => panic!("Enter on a drafted reply must send"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The draft's pick reaches the BACKGROUND LAUNCH argv, beside the agent — the
    /// `Enter` half of the `Ctrl-N` draft.
    #[test]
    fn a_compose_pick_reaches_the_background_launch_argv() {
        let dir = unique_temp_dir("model-launch");
        let mut app = App::new(Vec::new(), Scope::All, dir.clone());
        app.open_compose(
            ComposeState::new_background(Some("planner".to_string())),
            None,
        );
        app.set_compose_model(Some(ModelPick {
            model: "sonnet".to_string(),
            effort: Some("low"),
        }));
        type_into(&mut app, "ship it");

        match handle_compose_key(&mut app, key(KeyCode::Enter)) {
            Outcome::BgLaunch(req) => assert_eq!(
                req.argv.join(" "),
                "claude --agent planner --model sonnet --effort low --bg ship it"
            ),
            _ => panic!("Enter on a drafted launch must launch"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The draft's pick reaches its `Ctrl-O` INTERACTIVE run too — a new session, so
    /// the flags sit ahead of the prompt — because which key starts the draft does
    /// not change the model it chose.
    #[test]
    fn a_compose_pick_reaches_the_drafts_interactive_run() {
        let dir = unique_temp_dir("model-interactive");
        let mut app = App::new(Vec::new(), Scope::All, dir.clone());
        app.open_compose(
            ComposeState::new_background(Some("planner".to_string())),
            None,
        );
        app.set_compose_model(Some(ModelPick {
            model: "haiku".to_string(),
            effort: Some("medium"),
        }));
        type_into(&mut app, "ship it");

        match handle_compose_key(
            &mut app,
            with_mods(KeyCode::Char('o'), KeyModifiers::CONTROL),
        ) {
            Outcome::Resume(ready) => {
                assert_eq!(
                    ready.argv.join(" "),
                    "claude --agent planner --model haiku --effort medium ship it"
                );
                assert_eq!(
                    ready.nonzero_hint,
                    crate::resume::MODEL_NONZERO_HINT,
                    "a launch that carried a model gets the model-worded hint"
                );
            }
            _ => panic!("Ctrl-O on a draft must hand off interactively"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With NO pick every compose path is byte-identical to what it has always been
    /// — no `--model`, no `--effort` anywhere — on the reply, the draft's background
    /// launch AND the draft's interactive run. The counterpart of the three tests
    /// above, and the one that pins "costs nothing when unused".
    #[test]
    fn an_unpicked_compose_sends_no_model_on_any_path() {
        let dir = unique_temp_dir("model-none");
        let mut app = App::new(
            vec![sendable_session(&dir, "sbc-none")],
            Scope::All,
            dir.clone(),
        );
        let no_model_flags =
            |argv: &[String]| !argv.iter().any(|arg| arg == "--model" || arg == "--effort");

        app.open_compose(ComposeState::new_reply("sbc-none".to_string(), None), None);
        type_into(&mut app, "ship it");
        match handle_compose_key(&mut app, key(KeyCode::Enter)) {
            Outcome::Send(req) => {
                assert_eq!(
                    req.argv.join(" "),
                    "claude -p -r sbc-none --output-format json ship it"
                );
                assert!(no_model_flags(&req.argv));
            }
            _ => panic!("Enter on a drafted reply must send"),
        }

        app.open_compose(ComposeState::new_background(None), None);
        type_into(&mut app, "ship it");
        match handle_compose_key(&mut app, key(KeyCode::Enter)) {
            Outcome::BgLaunch(req) => {
                assert_eq!(req.argv.join(" "), "claude --bg ship it");
                assert!(no_model_flags(&req.argv));
            }
            _ => panic!("Enter on a drafted launch must launch"),
        }

        app.open_compose(ComposeState::new_background(None), None);
        type_into(&mut app, "ship it");
        match handle_compose_key(
            &mut app,
            with_mods(KeyCode::Char('o'), KeyModifiers::CONTROL),
        ) {
            Outcome::Resume(ready) => {
                assert_eq!(ready.argv.join(" "), "claude ship it");
                assert!(no_model_flags(&ready.argv));
                assert_eq!(
                    ready.nonzero_hint,
                    crate::resume::NEW_SESSION_NONZERO_HINT,
                    "no model sent, so the hint is the ordinary new-session one"
                );
            }
            _ => panic!("Ctrl-O on a draft must hand off interactively"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `Ctrl-L` opens the model picker OVER the compose and changes nothing else:
    /// the draft's text, its target and its (absent) pick are exactly as they were,
    /// and nothing is sent or launched.
    #[test]
    fn ctrl_l_opens_the_picker_over_the_compose_and_touches_nothing_else() {
        let mut app = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp"));
        app.open_compose(ComposeState::new_background(None), None);
        type_into(&mut app, "half a thought");

        let out = handle_compose_key(
            &mut app,
            with_mods(KeyCode::Char('l'), KeyModifiers::CONTROL),
        );
        assert!(
            matches!(out, Outcome::Continue),
            "opening a picker sends nothing"
        );
        assert!(app.modal.is_some(), "Ctrl-L opens the model picker");
        let compose = app
            .compose
            .as_ref()
            .expect("the compose stays open under it");
        assert_eq!(
            compose.textarea.lines(),
            ["half a thought"],
            "the draft is untouched, and the key typed no `l` into it"
        );
        assert_eq!(compose.model, None, "opening the picker picks nothing");
    }

    /// `Ctrl-T` / `Ctrl-E` decode to the transcript jumps (both cases, for the kitty
    /// path's sake), and only with Ctrl: bare `t` / `e` still type.
    #[test]
    fn ctrl_t_and_ctrl_e_decode_to_the_preview_jumps() {
        for (c, want) in [
            ('t', ComposeAction::PreviewTop),
            ('T', ComposeAction::PreviewTop),
            ('e', ComposeAction::PreviewBottom),
            ('E', ComposeAction::PreviewBottom),
        ] {
            assert_eq!(
                compose_key_to_action(with_mods(KeyCode::Char(c), KeyModifiers::CONTROL), false),
                want
            );
        }
        for c in ['t', 'e'] {
            assert_eq!(
                compose_key_to_action(key(KeyCode::Char(c)), false),
                ComposeAction::Forward,
                "a bare `{c}` types"
            );
        }
    }

    /// On a reply the jumps scroll the transcript of the compose's own row and touch
    /// nothing else: not the draft's text, not its caret, not the row selection.
    #[test]
    fn a_reply_scrolls_the_transcript_and_leaves_the_draft_alone() {
        let mut app = App::new(
            vec![session_at(
                "s",
                PathBuf::from("/tmp/s.jsonl"),
                PathBuf::from("/tmp"),
            )],
            Scope::All,
            PathBuf::from("/tmp"),
        );
        app.open_compose(ComposeState::new_reply("s".to_string(), None), None);
        type_into(&mut app, "abc");
        let caret = app.compose.as_ref().unwrap().textarea.cursor();
        let selected = app.selected_session().map(|s| s.session_id.clone());
        app.preview_scroll = 9;
        app.preview_follow_bottom = true;

        let ctrl = |c| with_mods(KeyCode::Char(c), KeyModifiers::CONTROL);
        assert!(matches!(
            handle_compose_key(&mut app, ctrl('t')),
            Outcome::Continue
        ));
        assert_eq!(app.preview_scroll, 0, "Ctrl-T jumps to the top");
        assert!(!app.preview_follow_bottom, "and drops follow-bottom");

        handle_compose_key(&mut app, ctrl('e'));
        assert!(
            app.preview_follow_bottom,
            "Ctrl-E re-follows the newest turn"
        );

        let compose = app.compose.as_ref().expect("still composing");
        assert_eq!(compose.textarea.lines(), ["abc"]);
        assert_eq!(compose.textarea.cursor(), caret, "the caret did not move");
        assert_eq!(
            app.selected_session().map(|s| s.session_id.clone()),
            selected
        );
    }

    /// A draft has no transcript on screen, so Ctrl-E keeps the editor's end-of-line
    /// and the preview is not scrolled.
    #[test]
    fn a_draft_keeps_ctrl_e_as_end_of_line() {
        let mut app = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp"));
        app.open_compose(ComposeState::new_background(None), None);
        type_into(&mut app, "abc");
        app.compose
            .as_mut()
            .unwrap()
            .textarea
            .move_cursor(CursorMove::Head);
        app.preview_follow_bottom = false;

        handle_compose_key(
            &mut app,
            with_mods(KeyCode::Char('e'), KeyModifiers::CONTROL),
        );
        assert_eq!(app.compose.as_ref().unwrap().textarea.cursor(), (0, 3));
        assert!(!app.preview_follow_bottom, "the preview was not touched");
    }

    /// Every board transcript-scroll key decodes to its scroll action, with the
    /// board's own modifier rule, and nothing else is claimed.
    #[test]
    fn the_board_scroll_keys_decode_to_scroll_actions() {
        let ctrl = KeyModifiers::CONTROL;
        for (code, mods, want) in [
            (KeyCode::Char('u'), ctrl, ComposeAction::PreviewHalfUp),
            (KeyCode::Char('U'), ctrl, ComposeAction::PreviewHalfUp),
            (KeyCode::Char('d'), ctrl, ComposeAction::PreviewHalfDown),
            (KeyCode::Char('D'), ctrl, ComposeAction::PreviewHalfDown),
            (KeyCode::Home, KeyModifiers::NONE, ComposeAction::PreviewTop),
            (
                KeyCode::End,
                KeyModifiers::NONE,
                ComposeAction::PreviewBottom,
            ),
            (
                KeyCode::PageUp,
                KeyModifiers::NONE,
                ComposeAction::PreviewPageUp,
            ),
            (
                KeyCode::PageDown,
                KeyModifiers::NONE,
                ComposeAction::PreviewPageDown,
            ),
            // The board matches the named keys under Shift/Alt too.
            (
                KeyCode::PageUp,
                KeyModifiers::SHIFT,
                ComposeAction::PreviewPageUp,
            ),
            (KeyCode::Home, KeyModifiers::ALT, ComposeAction::PreviewTop),
        ] {
            assert_eq!(
                compose_key_to_action(with_mods(code, mods), false),
                want,
                "{code:?}"
            );
        }
        // Bare letters type; Ctrl+named keys are the board's `Ignore`, so they stay
        // the editor's (word/paragraph jumps).
        for code in [KeyCode::Char('u'), KeyCode::Char('d')] {
            assert_eq!(
                compose_key_to_action(key(code), false),
                ComposeAction::Forward
            );
        }
        for code in [
            KeyCode::Home,
            KeyCode::End,
            KeyCode::PageUp,
            KeyCode::PageDown,
        ] {
            assert_eq!(
                compose_key_to_action(with_mods(code, ctrl), false),
                ComposeAction::Forward,
                "{code:?}"
            );
        }
    }

    /// On a reply each scroll key moves the transcript through the board's method and
    /// leaves text, caret and selection alone.
    #[test]
    fn a_reply_scrolls_with_every_board_scroll_key() {
        let mut app = App::new(
            vec![session_at(
                "s",
                PathBuf::from("/tmp/s.jsonl"),
                PathBuf::from("/tmp"),
            )],
            Scope::All,
            PathBuf::from("/tmp"),
        );
        app.open_compose(ComposeState::new_reply("s".to_string(), None), None);
        type_into(&mut app, "abc");
        let caret = app.compose.as_ref().unwrap().textarea.cursor();
        let selected = app.selected_session().map(|s| s.session_id.clone());
        let ctrl = |c| with_mods(KeyCode::Char(c), KeyModifiers::CONTROL);
        type Board = fn(&mut App);
        let cases: [(KeyEvent, Board); 6] = [
            (ctrl('u'), App::preview_half_up),
            (ctrl('d'), App::preview_half_down),
            (key(KeyCode::PageUp), App::preview_page_up),
            (key(KeyCode::PageDown), App::preview_page_down),
            (key(KeyCode::Home), App::preview_top),
            (key(KeyCode::End), App::preview_bottom),
        ];
        for (k, board) in cases {
            for follow in [false, true] {
                let mut want = app_with_preview(follow);
                board(&mut want);
                app.preview_scroll = 40;
                app.preview_follow_bottom = follow;
                handle_compose_key(&mut app, k);
                assert_eq!(
                    (app.preview_scroll, app.preview_follow_bottom),
                    (want.preview_scroll, want.preview_follow_bottom),
                    "{k:?} follow={follow}"
                );
            }
        }
        let compose = app.compose.as_ref().expect("still composing");
        assert_eq!(compose.textarea.lines(), ["abc"]);
        assert_eq!(compose.textarea.cursor(), caret);
        assert_eq!(
            app.selected_session().map(|s| s.session_id.clone()),
            selected
        );
    }

    fn app_with_preview(follow: bool) -> App {
        let mut a = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp"));
        a.preview_scroll = 40;
        a.preview_follow_bottom = follow;
        a
    }

    /// A draft keeps all of them as editor keys: Ctrl-D deletes a char, Home moves
    /// the caret, and the preview is not touched.
    #[test]
    fn a_draft_keeps_every_scroll_key_as_an_editor_key() {
        let mut app = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp"));
        app.open_compose(ComposeState::new_background(None), None);
        type_into(&mut app, "abc");
        app.preview_scroll = 7;
        app.preview_follow_bottom = false;
        handle_compose_key(&mut app, key(KeyCode::Home));
        assert_eq!(app.compose.as_ref().unwrap().textarea.cursor(), (0, 0));
        handle_compose_key(
            &mut app,
            with_mods(KeyCode::Char('d'), KeyModifiers::CONTROL),
        );
        assert_eq!(app.compose.as_ref().unwrap().textarea.lines(), ["bc"]);
        handle_compose_key(&mut app, key(KeyCode::End));
        assert_eq!(app.compose.as_ref().unwrap().textarea.cursor(), (0, 2));
        handle_compose_key(
            &mut app,
            with_mods(KeyCode::Char('u'), KeyModifiers::CONTROL),
        );
        // The widget's Ctrl-U is undo: it undid the Ctrl-D delete.
        assert_eq!(app.compose.as_ref().unwrap().textarea.lines(), ["abc"]);
        assert_eq!((app.preview_scroll, app.preview_follow_bottom), (7, false));
    }
}
