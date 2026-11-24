//! The compose zone: multiline input + its key dispatch.
//!
//! This module owns the MULTILINE `ratatui_textarea` editor (plus the
//! `App::compose` field, whose type is [`ComposeState`]) — one of exactly TWO
//! sites that HOLD a `ratatui_textarea` value (three values in all). The other is
//! `tui::app`, which holds the board's ONE-LINE inputs: the search query,
//! [`App::query_input`](super::app::App::query_input), and the preview find bar's
//! text, [`FindBar::input`](super::app::FindBar::input), edited by `App`'s
//! mutators and drawn by `view::render_search` / `view::render_find_bar`. Those
//! two draw through the one `view::render_input_line`, whose `input` parameter is
//! the only place `tui::view` names the type: three files `use` the crate, two
//! hold a value. Each site owns its own configuration and its own key routing,
//! and neither reads the other's; the pairing is not a `memchr`-style
//! single-module confinement, and `Cargo.toml` states the same boundary as the
//! blast radius of a version bump.
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
//! ONE editor and ONE key router serve THREE targets, distinguished by
//! [`ComposeTarget`] rather than by parallel state:
//!
//! * [`ComposeTarget::Reply`] — the quick reply `Ctrl-R` opens on a selected
//!   session. `Enter` sends a one-shot `claude -p -r` ([`crate::send`]).
//! * [`ComposeTarget::Fork`] — the box `Ctrl-X f` opens on a selected session.
//!   `Enter` forks it headless (`claude -p -r <id> --fork-session --session-id
//!   <new>`, over the reply's send path) and `Ctrl-O` forks it interactively.
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
//! ALL THREE targets carry the same pick list ([`CompletionState`]), through the
//! same editor, router and driver: `/` first in the draft lists the folder's skills
//! and commands, `@` at the start of a word lists files and folders and, at the top
//! level, agents. The one per-target difference is WHERE the list reads from, and
//! [`completion_source`] alone decides it. While the list is showing the router
//! claims `Enter`/`Tab` (pick), `Up`/`Down` (highlight) and `Esc` (close the list,
//! never the draft); otherwise every key decodes as before. Its bounded reads run
//! from the key/paste handler and the `CatalogFetched` arm only
//! ([`refresh_completion`]), never the render path; claude's own list for the
//! folder is fetched off the UI thread, requested by [`take_catalog_fetch`].
//!
//! A draft too long to be one `claude` argument ([`send::check_prompt_len`]) is
//! REFUSED before any target routes, measured as it will be SENT (each attachment
//! placeholder expanded, [`draft`]) and after a box waits for its own attachment
//! read ([`submit_waits_for_attach`]): `Enter` in all three boxes
//! ([`submit_compose`]) and `Ctrl-O` in the fork box and the draft
//! ([`open_interactive`], the reply's being inert). The box stays open with the
//! draft intact — see [`refuse_too_long`].
//!
//! [`insert_paste`] is where the TEXT of a terminal paste enters: it arrives whole
//! (`super::update::handle_paste`) and goes straight into the editor, bypassing the
//! key router entirely. That bypass is the point — routed as keystrokes, a pasted
//! newline reached the `Enter` = Send arm above. An EMPTY paste takes no text: it
//! asks for the same clipboard attach as `Ctrl-V` ([`request_attach`]).

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_textarea::{CursorMove, TextArea, WrapMode};

use super::clipboard::{ReadMode, FILE_URI_SCHEME};
use crate::resume::ModelPick;
use crate::send::{self, BgLaunchRequest, SendPlan, SendRequest};
use crate::store::skills::{read_listing, Listing};
use crate::watch::ClipboardAttach;

use super::app::{App, NewSessionDraft};
use super::complete::{
    at_candidates, at_list, attachment_placeholder, completion_context, file_mention,
    filter_commands, is_mention_boundary, list_tree, mention_insert_text, move_highlight,
    replacement, settle_highlight, split_path_query, AttachmentKind, Candidate, Context,
    DirEntryInfo, MentionRefusal, Trigger,
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

/// The [`COMPOSE_EMPTY_HINT`] of the fork box: `Enter` forks headless and a headless
/// run needs a message, so it names the key that forks without one.
const COMPOSE_EMPTY_FORK_HINT: &str =
    "nothing to send — type a message, or Ctrl-O to fork without one";

/// Prefix of the refusal shown when no session id could be generated for a fork
/// (the OS gave no entropy). The compose stays open; nothing was dispatched.
const FORK_ID_FAILED_PREFIX: &str = "could not generate an id for the fork: ";

/// Status when the composed session vanished from the store between opening the
/// compose zone and pressing Send (e.g. its file was removed).
const COMPOSE_SESSION_GONE: &str = "that session is no longer loaded — nothing sent";

/// The [`COMPOSE_SESSION_GONE`] of the fork box's `Ctrl-O` run
/// (`update::launch_fork_run`): the box's session left the store before the
/// interactive fork could start, so nothing was forked and no terminal was handed
/// over.
pub(super) const COMPOSE_FORK_SESSION_GONE: &str =
    "that session is no longer loaded — nothing forked";

/// Status when `Ctrl-V` (or an empty paste) arrives while a clipboard read is
/// already running: one attach at a time, so this is a nudge, not a refusal.
const ATTACH_BUSY_HINT: &str = "already reading the clipboard — one attach at a time";

/// Status when `Enter` arrives while the box's own attach is still reading
/// ([`submit_waits_for_attach`]): nothing is sent, and the draft and the read stay
/// as they were, so this is a nudge, not a refusal.
pub(crate) const ENTER_WHILE_ATTACHING_HINT: &str =
    "still attaching — press Enter again once it lands";

/// The [`ENTER_WHILE_ATTACHING_HINT`] of `Ctrl-O`, the fork box's and the draft's
/// interactive run ([`submit_waits_for_attach`]): nothing is handed off, and the
/// draft and the read stay as they were, so this is a nudge, not a refusal. Its
/// own const because it names the key to press again.
pub(crate) const CTRL_O_WHILE_ATTACHING_HINT: &str =
    "still attaching — press Ctrl-O again once it lands";

/// Status when `Esc` cancels the box's own explicit attach instead of the box
/// ([`esc_cancels_attach`]): the draft stays as it was and the read's late result
/// is dropped as a stale token, so this is a nudge that expires, not a refusal.
pub(crate) const ATTACH_CANCELLED_HINT: &str =
    "attach cancelled — press Esc again to close the box";

/// The in-flight mark on the compose box's border while a clipboard read runs.
/// Typed state rendered on the box itself, never on the status line (STATUS-LINE
/// OWNERSHIP).
pub(crate) const ATTACHING_LABEL: &str = " attaching… ";

/// Refusal when the clipboard holds only text: the terminal's own paste carries it.
const ATTACH_TEXT_ONLY: &str =
    "clipboard holds text only — paste it with ⌘V / Ctrl-Shift-V (Ctrl-V attaches files and images)";
/// Refusal when the clipboard is empty.
const ATTACH_NOTHING: &str = "nothing to attach — the clipboard is empty";
/// Refusal for an image format claude cannot read.
const ATTACH_UNSUPPORTED_IMAGE: &str =
    "that image format is not supported — copy a PNG, JPEG, GIF or WebP";
/// Refusal for an image over the size cap.
const ATTACH_TOO_LARGE: &str = "that image is too large to attach";
/// Refusal over SSH, where a local tool would read the REMOTE clipboard.
const ATTACH_REMOTE: &str = "cannot read the clipboard over SSH — it would be the remote machine's";
/// Refusal when there is no display to ask.
const ATTACH_NO_DISPLAY: &str = "no display to read a clipboard from";
/// Refusal when the clipboard tool is missing.
const ATTACH_NO_TOOL: &str =
    "clipboard tool not found — install wl-clipboard (Wayland) or xclip (X11)";
/// Refusal when the read timed out or the tool failed.
const ATTACH_FAILED: &str = "could not read the clipboard";
/// Leads the refusal of an explicit attach some of whose paths no mention can
/// carry; the others still go in.
const ATTACH_REFUSED_PREFIX: &str = "not attached: ";
/// Leads the refusal of a copied-file paste that stays verbatim because one of
/// its files no mention can carry: a paste is upgraded whole or not at all.
const PASTE_KEPT_PREFIX: &str = "paste kept as text, cannot attach: ";
/// Between a refused path and its cause ([`MentionRefusal::describe`]).
const ATTACH_REFUSED_CAUSE_SEPARATOR: &str = " — ";
/// Between two refused paths.
const ATTACH_REFUSED_SEPARATOR: &str = "; ";
/// Leads the confirmation of copied files attached: their placeholder shows only a
/// file name, so the status names the full path the mention will carry.
const ATTACHED_PREFIX: &str = "attached ";
/// Around the count of further files in the confirmation of several: ` (+2 more)`.
const ATTACHED_MORE_OPEN: &str = " (+";
const ATTACHED_MORE_CLOSE: &str = " more)";

/// The longest paste, in characters, that [`paste_may_name_files`] will ask the
/// clipboard about. A pasted file path or a few names are short; anything longer
/// is prose, and the cap keeps a large paste from costing a child read.
const PASTE_FILE_NAMES_MAX_CHARS: usize = 1024;

/// The longest extension, in characters, that makes a bare word look like a file
/// name (`report.pdf`, `shot.jpeg`, `notes.markdown`).
const FILE_EXTENSION_MAX_CHARS: usize = 8;

/// How a one-line paste that names files by PATH starts, so [`paste_may_name_files`]
/// asks the clipboard about it unless it names a URI scheme other than `file`: an
/// absolute path, raw or backslash-escaped (iTerm2 escapes inside the path, never
/// its leading `/`); a `file://` URI; an absolute path in single or double quotes.
/// Those are the path renderings [`paste_names_files`] matches. `~/` is asked
/// about too, though no rendering matches it: the clipboard's paths are absolute,
/// so that read never upgrades.
const PASTE_PATH_PREFIXES: [&str; 5] = ["/", "~/", FILE_URI_SCHEME, "'/", "\"/"];

/// Ends a URI scheme that names an authority (RFC 3986 §3): `https://`, and the
/// `file://` [`FILE_URI_SCHEME`] opens with.
const URI_AUTHORITY_MARK: &str = "://";

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
    /// A FORK of an EXISTING session (`Ctrl-X f`). Addresses the same row a reply
    /// does, but writes a NEW transcript, so it carries no `stop_job`: claude
    /// skips the live-session refusal for a fork.
    Fork {
        /// Stable `session_id` of the original (STABLE-ID STATE), re-resolved to the
        /// authoritative `(cwd, session_id)` from inside the file at `Enter` or
        /// `Ctrl-O` time.
        session_id: String,
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
    /// What this draft is addressed to — a reply, a fork, or a new background
    /// agent.
    pub target: ComposeTarget,
    /// The multiline editor buffer, shared by EVERY target. One of exactly THREE
    /// `ratatui_textarea` values in the program — the others are the board's
    /// one-line [`App::query_input`](super::app::App::query_input) and the
    /// preview find bar's [`FindBar::input`](super::app::FindBar::input), which
    /// are separate buffers with their own configuration and are never routed
    /// through this one (see the module doc).
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
    /// read by the submit paths below, which emit `--model` ONLY when it is `Some`
    /// (and then its own effort, never the session's carried one), and by the compose
    /// box's `model:` label.
    pub model: Option<ModelPick>,
    /// The `/` and `@` pick list of this draft, reply, fork and background draft
    /// alike (see [`CompletionState`]).
    pub completion: CompletionState,
    /// The clipboard read this compose is waiting on, if any (see
    /// [`PendingAttach`]). Born `None` and dies with the compose, or with the `Esc`
    /// that cancels an explicit one ([`esc_cancels_attach`]).
    pub pending_attach: Option<PendingAttach>,
    /// What each attachment placeholder this box inserted stands for, in the order
    /// they went in (see [`Attachment`]).
    ///
    /// SCOPED TO ONE COMPOSE, exactly like [`model`](Self::model): born empty with
    /// every [`ComposeState::new`] and dead with the compose (`App::close_compose`);
    /// nothing global holds it and nothing on disk learns it. Written only by
    /// [`finish_attach`]; read only by [`draft`], which expands each placeholder
    /// still in the text ([`expand_attachments`]). An entry whose placeholder has
    /// left the text sends nothing.
    pub attachments: Vec<Attachment>,
}

/// One attachment of a compose box: the file an attachment placeholder in its
/// draft stands for. It stores only what varies; the placeholder is derived
/// through the ONE formatter ([`Attachment::placeholder`]), so what the draft shows
/// and what [`expand_attachments`] looks for cannot drift apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// An image saved in the paste cache, or a copied file.
    pub kind: AttachmentKind,
    /// The absolute path its mention names, accepted by `file_mention` when it was
    /// attached.
    pub path: PathBuf,
    /// Its number, unique for the board session ([`App::attachment_number`]).
    pub number: u64,
}

impl Attachment {
    /// The attachment placeholder the draft shows for it
    /// ([`attachment_placeholder`]).
    pub fn placeholder(&self) -> String {
        attachment_placeholder(self.kind, &self.path, self.number)
    }
}

/// One clipboard read a compose is waiting on. The `token` ties the worker's
/// `AppEvent::ClipboardRead` to THIS compose: a result for a compose that was
/// cancelled or replaced carries a token nobody holds and is dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAttach {
    /// The request id ([`App::attach_token`]).
    pub token: u64,
    /// Whether the driver has been handed this request ([`take_clipboard_read`]).
    pub requested: bool,
    /// Why the read was asked for.
    pub purpose: AttachPurpose,
}

impl PendingAttach {
    /// Whether the box shows its `attaching…` mark: only for a read the user asked
    /// for, never for the silent file-paste check.
    pub fn shows_mark(&self) -> bool {
        self.purpose == AttachPurpose::Explicit
    }
}

/// Why a clipboard read was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachPurpose {
    /// `Ctrl-V`, or an empty paste: attach whatever the clipboard holds.
    Explicit,
    /// A paste that looked like copied files: when the clipboard confirms it holds
    /// those files, the verbatim text becomes their attachment placeholders.
    Upgrade {
        /// The text that was inserted.
        inserted: String,
        /// The buffer's lines right after the insert.
        lines_after: Vec<String>,
        /// The caret right after the insert.
        cursor_after: (usize, usize),
    },
}

/// The compose pick list of any compose box: what is showing, plus the per-draft
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
    /// What every pick of `visible` is inserted under, set WITH it
    /// (`complete::AtList::dir_part`; empty for `/`), so accepting never re-derives
    /// it from the query: an `@` root-fallback row replaces the whole query.
    dir_part: String,
    /// The highlighted row of `visible`.
    pub highlight: usize,
    /// The token the caret ends, whether or not its list is showing.
    pub context: Option<Context>,
    /// The token `Esc` closed the list for: `(trigger, row, start column)`. The
    /// list stays closed while the caret is still in that same token.
    dismissed: Option<(Trigger, usize, usize)>,
    /// A REPLY's or FORK's `@` agents until a catalog lands, read from its session
    /// transcript by [`read_listing`]: `None` = not read yet, an empty listing =
    /// read and nothing found. Read at most once per draft, only while no catalog
    /// has landed for the folder, only for a top-level `@` (an `@` with a folder
    /// part lists no agent, and `/` lists the catalog alone), and never for a
    /// background draft (it has no transcript).
    transcript: Option<Listing>,
    /// Whether this compose has had its ONE ask for a catalog fetch
    /// ([`take_catalog_fetch`]). Born `false` with every compose, so the next
    /// compose on a folder whose fetch failed asks again, while this one never does.
    catalog_requested: bool,
    /// Folder listings keyed by the resolved folder. Read at most once per folder.
    dirs: HashMap<PathBuf, Vec<DirEntryInfo>>,
}

/// What one refresh of the list reads from, borrowed for that refresh alone: the
/// target's folder, its transcript (a reply's or fork's only) and the folder's
/// cached catalog, if one has landed. Borrowed, so a catalog is never cloned per
/// keystroke.
struct SourceView<'a> {
    cwd: &'a Path,
    transcript: Option<&'a Path>,
    catalog: Option<&'a Listing>,
}

impl CompletionState {
    /// No token, so no list and nothing remembered about a closed one.
    fn close(&mut self) {
        self.context = None;
        self.dismissed = None;
        self.visible = None;
    }

    /// Recompute the list for the caret at `cursor` in `lines`. `source` is what
    /// this draft's list reads from ([`completion_source`]), or `None` for a reply
    /// or fork whose session left the store, which shows no list.
    ///
    /// `/` lists the folder's `catalog` alone, and nothing until it lands: only
    /// claude's own list knows which skills and commands its `/` menu hides, while
    /// a transcript's `skill_listing` is the MODEL's list and carries no flag to
    /// tell them apart (`docs/agents/CLAUDE_CLI.md`, "The `initialize` control
    /// handshake"). A top-level `@` takes its agents from the catalog once it has
    /// landed — it REPLACES the session's transcript, never merges with it — else
    /// from that transcript, else none.
    ///
    /// The ONLY reads the pick list makes happen here: the transcript listing (once
    /// per reply or fork box, and only for a top-level `@`) and folder walks (once
    /// per folder, capped by `COMPLETION_MAX_DIR_ENTRIES` and
    /// `COMPLETION_MAX_TREE_ENTRIES`) — the folder an `@` names, plus, for an `@`
    /// root fallback alone (`complete::at_list`), the compose folder's own walk,
    /// the same cache entry a top-level `@` fills. Bounded
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
            self.close();
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
        let (candidates, dir_part) = match ctx.trigger {
            Trigger::Slash => (
                source.catalog.map_or_else(Vec::new, |listing| {
                    filter_commands(&listing.commands, &ctx.query)
                }),
                String::new(),
            ),
            Trigger::At => {
                let (dir_part, _) = split_path_query(&ctx.query);
                // Only a top-level `@` lists agents (`complete::at_candidates`), so
                // only then may the transcript be read.
                let agents = if dir_part.is_empty() {
                    active_listing(&mut self.transcript, &source)
                        .map_or(&[][..], |listing| listing.agents.as_slice())
                } else {
                    &[]
                };
                let folder = cached_tree(&mut self.dirs, source.cwd.join(dir_part));
                let folder_hits = at_candidates(folder, agents, &ctx.query);
                // The root fallback's walk is the one a top-level `@` reads: the
                // same cache entry, read only when the folder step found nothing.
                let dirs = &mut self.dirs;
                let list = at_list(&ctx.query, folder_hits, move || {
                    cached_tree(dirs, source.cwd.to_path_buf())
                });
                (list.candidates, list.dir_part)
            }
        };
        if candidates.is_empty() {
            self.visible = None;
            return;
        }
        self.highlight = settle_highlight(self.highlight, candidates.len(), start_changed);
        self.visible = Some(candidates);
        self.dir_part = dir_part;
    }
}

/// `dir`'s walk from `dirs`, read into it by `complete::list_tree` on first use:
/// at most once per folder per compose.
fn cached_tree(dirs: &mut HashMap<PathBuf, Vec<DirEntryInfo>>, dir: PathBuf) -> &[DirEntryInfo] {
    dirs.entry(dir).or_insert_with_key(|dir| list_tree(dir))
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
        ComposeTarget::Reply { session_id, .. } | ComposeTarget::Fork { session_id } => {
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
    /// input. The single constructor, so every target gets an
    /// identically-configured editor and the widget setup lives in exactly one
    /// place — and so every compose starts on its default model
    /// ([`model`](Self::model) `None`), however the last one ended.
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
            pending_attach: None,
            attachments: Vec::new(),
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

    /// Open a fresh FORK buffer for `session_id`.
    #[must_use]
    pub fn new_fork(session_id: String) -> Self {
        Self::new(ComposeTarget::Fork { session_id })
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
    /// The clone is a WHOLE `TextArea` copy, not a cheap handle: the draft's lines, its
    /// undo history, its yank buffer and its already-built screen map. The two moves on
    /// it then RE-WRAP NOTHING: `move_cursor` only resolves the cursor against that
    /// copied map, which the editor rebuilds on an EDIT and on a RENDER whose area
    /// changed, never on a move (it does scan the caret's line: 0.19 ms on one
    /// 786,432-byte line). Do NOT "optimize" it into a shared cursor move: the probe
    /// must not disturb where the user's caret actually is.
    ///
    /// The cost is PER FRAME, not per keystroke. Every wake-up of the run loop draws —
    /// each `watch::TICK` (250 ms, unconditional), key, mouse move and worker event —
    /// and a DOCKED box pays this copy at LEAST THREE times per wake-up: in the render
    /// (`preview_areas` → `compose_zone_height`) and in each of the two
    /// `view::preview_transcript_rect` calls in the loop of `tui/mod.rs`, which go
    /// through `preview_areas` too. The bottom-bar layout pays it once. Every OTHER
    /// `view::preview_transcript_rect` call pays it again, so a docked box pays MORE
    /// while a preview selection is shown (`view::overlay_preview_selection`) and
    /// while the pointer acts over the preview (`update::mouse_effect`).
    ///
    /// What one copy weighs is set by the UNDO HISTORY at least as much as by the
    /// draft. It holds 50 edits (the crate's default), and an insert or delete entry
    /// keeps its full text, so EACH large edit still in the history adds its own
    /// size to every copy. A draft is at most [`send::COMPOSE_PROMPT_MAX_BYTES`] from
    /// ONE paste and can exceed even that through several, since each paste is
    /// capped on its own. Per DOCKED wake-up of the three copies above (no selection
    /// shown, no pointer event), with the draft at the macOS limit of 786,432 bytes
    /// (median of the worse of two draft shapes, a 7,788-line log and a single line):
    ///
    /// * no history: 0.7 ms;
    /// * history depth 1, one maximal paste: 1.1 ms;
    /// * history depth 5, three maximal pastes with the first two each deleted whole:
    ///   3.3 ms;
    /// * the ceiling, 50 maximal edits in the history, about 50 MB per copy: 29 ms.
    ///
    /// The per-EDIT cost is separate: every edit rebuilds the screen map, about 11 ms
    /// per typed character at that size.
    ///
    /// Measured 2026-11-04 on an Apple M4 Max: release build, `ratatui-textarea`
    /// 0.9.2 configured as [`ComposeState::new`] does (`WrapMode::WordOrGlyph`), one
    /// `insert_str` per paste as `insert_paste` does, an 80x6 editor (the docked box on
    /// a 160x48 terminal), 300 iterations.
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
    /// Cancel (`Esc`): the box's own explicit attach while one is reading, keeping
    /// the draft ([`esc_cancels_attach`]), else the compose, back to the board.
    Cancel,
    /// Run the draft INTERACTIVELY instead of in the background (`Ctrl-O`).
    ///
    /// [`ComposeTarget::NewBackgroundAgent`] and [`ComposeTarget::Fork`] act on this;
    /// on a [`ComposeTarget::Reply`] it is INERT (there is no interactive launch to
    /// escape to — a reply addresses a session that already exists). The decision
    /// stays target-free here so the pure router remains a plain key → intent map;
    /// [`handle_compose_key`] is where the target decides whether the intent is
    /// actionable. `Ctrl-O` is unbound in `ratatui_textarea`, so claiming it costs
    /// the reply editor nothing it previously did. Like `Enter`, it waits while the
    /// box's own explicit attach is reading ([`submit_waits_for_attach`]).
    OpenInteractive,
    /// Attach what the OS clipboard holds (`Ctrl-V`): copied files, else an image,
    /// each as an attachment placeholder in the draft, sent as its `@"<path>"`
    /// mention ([`expand_attachments`]). Every target acts on it.
    ///
    /// Claims `Ctrl-V` from `ratatui-textarea`'s page-down on purpose: a draft keeps
    /// `PgDn` for its editor and a reply's or fork's `PgDn` scrolls the preview.
    /// Text paste stays the terminal's own bracketed paste.
    AttachClipboard,
    /// Open the model picker for THIS compose (`Ctrl-L`) — every target acts on it,
    /// since a reply, a fork and a draft each carry their own pick
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
    /// Acts only on a [`ComposeTarget::Reply`] or [`ComposeTarget::Fork`], whose
    /// compose previews the real transcript; a draft shows a placeholder card, so
    /// there the key falls back to the editor ([`handle_compose_key`]). `Ctrl-T` is
    /// unbound in the widget.
    PreviewTop,
    /// Jump the transcript to its bottom and re-follow the newest turn (`Ctrl-E`).
    ///
    /// Same target rule as [`ComposeAction::PreviewTop`]. `Ctrl-E` IS the widget's
    /// end-of-line, so on a reply or fork it is taken from the editor on purpose (the
    /// caret still reaches line end with `End`/`Ctrl-F`); a draft keeps it.
    PreviewBottom,
    /// Scroll the transcript a page up (`PgUp`, the board's key). Reply and fork
    /// only, like [`ComposeAction::PreviewTop`]; the editor loses its caret page-up
    /// there.
    PreviewPageUp,
    /// A page down (`PgDn`). Same rule as [`ComposeAction::PreviewPageUp`].
    PreviewPageDown,
    /// A quarter page up (`Ctrl-U`). On a reply or fork it is taken from the
    /// editor's delete-to-line-head on purpose; a draft keeps it.
    PreviewHalfUp,
    /// A quarter page down (`Ctrl-D`). On a reply or fork it is taken from the
    /// editor's delete-char on purpose; a draft keeps it.
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
/// * `Ctrl-O` → **OpenInteractive** (the background draft's and the fork's
///   interactive run; inert on a reply — see [`ComposeAction::OpenInteractive`]).
/// * `Ctrl-L` → **PickModel** (this compose's model picker, on every target — see
///   [`ComposeAction::PickModel`] for why the key is free).
/// * `Ctrl-V` → **AttachClipboard** (copied files, else an image, as attachment
///   placeholders sent as `@"<path>"` mentions; every target). It shadows
///   `ratatui-textarea`'s `Ctrl-V` page-down: a draft keeps `PgDn` for its
///   editor, a reply's or fork's `PgDn` scrolls the preview.
/// * `Ctrl-T` / `Ctrl-E` / `Home` / `End` → **PreviewTop** / **PreviewBottom**,
///   `PgUp` / `PgDn` → **PreviewPageUp** / **PreviewPageDown**, `Ctrl-U` / `Ctrl-D`
///   → **PreviewHalfUp** / **PreviewHalfDown**: exactly the board's transcript
///   scroll keys (`update::key_to_action` — the `Ctrl` letters whatever else is
///   held, the named keys only WITHOUT `Ctrl`). A reply or fork acts on them, a
///   draft forwards them to the editor.
/// * `Esc` → **Cancel** (the box's explicit attach first, if one is reading, then
///   the compose; never the app — see [`esc_cancels_attach`]).
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
        KeyCode::Char('v' | 'V') if ctrl => ComposeAction::AttachClipboard,
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
/// gate (and, for a waiting agent that is not registry-only, the stop
/// confirmation) has already run at the call site (`Ctrl-R` in `update`).
pub fn open(app: &mut App, session_id: String, stop_job: Option<String>) {
    // No draft card: a reply previews the REAL session it is addressed to.
    app.open_compose(ComposeState::new_reply(session_id, stop_job), None);
}

/// Open the FORK compose zone for `session_id`, bringing back a hidden preview like
/// [`open`]. The folder check has already run at the call site (`Ctrl-X f` in
/// `update`). No draft card: the preview shows the real session being forked.
pub fn open_fork(app: &mut App, session_id: String) {
    app.open_compose(ComposeState::new_fork(session_id), None);
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
/// FULL key handler ([`TextArea::input`]); Cancel drops the box's own explicit
/// attach while one is reading ([`esc_cancels_attach`]), else clears the compose
/// state; Send resolves the buffer through [`submit_compose`] (a reply send, a
/// headless fork, or a background launch); OpenInteractive escalates a background
/// draft or a fork to the interactive hand-off ([`open_interactive`]); both wait
/// while the box's own attach is still reading ([`submit_waits_for_attach`]); and
/// PickModel opens the model picker over this compose (`App::open_model_picker`),
/// which leaves the draft — text and pick alike — exactly as it was until the
/// picker's `Enter` writes a new pick into it.
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
            let pending = app.compose.as_ref().and_then(|c| c.pending_attach.as_ref());
            if esc_cancels_attach(pending) {
                // The read's late result now carries a token nobody holds.
                if let Some(compose) = app.compose.as_mut() {
                    compose.pending_attach = None;
                }
                app.set_status_transient(ATTACH_CANCELLED_HINT);
                return Outcome::Continue;
            }
            // Esc drops the editor AND the draft card together — one teardown, so a
            // cancelled draft can never leave a placeholder pane behind.
            app.close_compose();
            Outcome::Continue
        }
        ComposeAction::Send => {
            let pending = app.compose.as_ref().and_then(|c| c.pending_attach.as_ref());
            if submit_waits_for_attach(pending) {
                app.set_status_transient(ENTER_WHILE_ATTACHING_HINT);
                return Outcome::Continue;
            }
            submit_compose(app)
        }
        ComposeAction::OpenInteractive => open_interactive(app),
        // The transcript a reply or fork box previews is the SELECTED row's, which
        // is the box's target (STABLE-ID STATE: nothing here moves the selection),
        // so the board's own jump applies; the caret and text are not touched. A
        // draft has no transcript on screen, so the key reaches the editor as it
        // always did.
        scroll @ (ComposeAction::PreviewTop
        | ComposeAction::PreviewBottom
        | ComposeAction::PreviewPageUp
        | ComposeAction::PreviewPageDown
        | ComposeAction::PreviewHalfUp
        | ComposeAction::PreviewHalfDown) => {
            let replying = matches!(
                app.compose.as_ref().map(|c| &c.target),
                Some(ComposeTarget::Reply { .. } | ComposeTarget::Fork { .. })
            );
            if !replying {
                if let Some(compose) = app.compose.as_mut() {
                    compose.textarea.input(key);
                }
                // The key moved the caret or edited the text (`Home`, `Ctrl-U`, …),
                // so an open list was computed for a token that is no longer there:
                // a pick would delete the query's length at the NEW caret.
                refresh_completion(app);
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
        ComposeAction::AttachClipboard => {
            request_attach(app, AttachPurpose::Explicit, true);
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
/// close the draft. It edits the buffer, and a paste shaped like a copied file's
/// rendering ([`paste_may_name_files`]) ALSO requests a silent clipboard read, whose
/// result may later replace the pasted text with attachment placeholders
/// ([`finish_attach`]).
/// `text` is already normalized and capped by `update::accept_paste`; a no-op on an
/// empty string, and on a paste that arrives with no draft open (the caller checks,
/// but the `if let` keeps this total).
pub fn insert_paste(app: &mut App, text: &str) {
    if let Some(compose) = app.compose.as_mut() {
        compose.textarea.insert_str(text);
    }
    // A pasted `/` or `@` opens the list like a typed one; still no `Outcome`.
    refresh_completion(app);
    // Cmd+V of a copied file pastes its path or name as text. Ask the clipboard
    // whether it holds those files; the verbatim text stays unless it does.
    if paste_may_name_files(text) {
        if let Some(compose) = app.compose.as_ref() {
            let purpose = AttachPurpose::Upgrade {
                inserted: text.to_string(),
                lines_after: compose.textarea.lines().to_vec(),
                cursor_after: cursor_pos(&compose.textarea),
            };
            request_attach(app, purpose, false);
        }
    }
}

/// Start a clipboard read for the open compose. Does not spawn: the driver does,
/// after [`take_clipboard_read`] hands it the token. With a read already pending,
/// `nudge` says whether to tell the user (an explicit key) or stay silent (a paste
/// whose upgrade check is simply skipped). No-op with no compose open.
pub(crate) fn request_attach(app: &mut App, purpose: AttachPurpose, nudge: bool) {
    let Some(compose) = app.compose.as_ref() else {
        return;
    };
    // A silent upgrade check shows no mark and must not block the user's own
    // `Ctrl-V`: an explicit request takes over, and the check's result, carrying a
    // token nobody holds any more, is dropped.
    let silent_upgrade = matches!(
        compose.pending_attach.as_ref().map(|p| &p.purpose),
        Some(AttachPurpose::Upgrade { .. })
    );
    let supersedes = silent_upgrade && purpose == AttachPurpose::Explicit;
    if compose.pending_attach.is_some() && !supersedes {
        if nudge {
            app.set_status_transient(ATTACH_BUSY_HINT);
        }
        return;
    }
    app.attach_token = app.attach_token.wrapping_add(1);
    let token = app.attach_token;
    if let Some(compose) = app.compose.as_mut() {
        compose.pending_attach = Some(PendingAttach {
            token,
            requested: false,
            purpose,
        });
    }
}

/// The token and [`ReadMode`] of a clipboard read the driver should start now, if
/// any: `tui::run_inner` asks after every handled event and spawns on an answer,
/// so the decision stays here, pure and tested, and no key handler ever spawns.
/// Returns a pending, un-requested attach ONCE and marks it requested. An
/// `Upgrade` is `ListOnly`: only an explicit attach may save an image.
pub(crate) fn take_clipboard_read(app: &mut App) -> Option<(u64, ReadMode)> {
    let pending = app.compose.as_mut()?.pending_attach.as_mut()?;
    if pending.requested {
        return None;
    }
    pending.requested = true;
    let mode = match pending.purpose {
        AttachPurpose::Explicit => ReadMode::Attach,
        AttachPurpose::Upgrade { .. } => ReadMode::ListOnly,
    };
    Some((pending.token, mode))
}

/// The saved images of the clipboard results [`finish_attach`] dropped, for the
/// driver to delete now, each handed over ONCE: `tui::run_inner` asks after every
/// handled event and gives a non-empty answer to
/// `clipboard::spawn_paste_image_discard`, so nothing is deleted on the UI thread
/// and no handler spawns.
pub(crate) fn take_stale_paste_images(app: &mut App) -> Vec<PathBuf> {
    std::mem::take(&mut app.stale_paste_images)
}

/// Whether a submit — `Enter`, or the fork box's and the draft's `Ctrl-O` run —
/// must wait for the box's `pending` attach. PURE. Only an EXPLICIT read holds it
/// (`Ctrl-V`, or an empty paste: exactly the read whose `attaching…` mark is
/// showing), since leaving then would drop the attachment the user just asked for.
/// The silent copied-file check never does: its paste is already in the draft as
/// text, and its late result is dropped by the token rule.
pub(crate) fn submit_waits_for_attach(pending: Option<&PendingAttach>) -> bool {
    pending.is_some_and(PendingAttach::shows_mark)
}

/// Whether `Esc`, with the pick list closed, cancels the box's `pending` attach
/// and keeps the box. PURE. True for exactly the read [`submit_waits_for_attach`]
/// holds `Enter` and `Ctrl-O` for, so every read that holds the box has a key that
/// releases it: on Linux a hung clipboard owner keeps it for the read's whole
/// timeout. The silent copied-file check is no attach to cancel; `Esc` then
/// cancels the box. An open pick list is closed first, before this is asked
/// ([`compose_key_to_action`]).
pub(crate) fn esc_cancels_attach(pending: Option<&PendingAttach>) -> bool {
    submit_waits_for_attach(pending)
}

/// Whether a paste is worth asking the clipboard about. PURE. It is ONE line of at
/// most [`PASTE_FILE_NAMES_MAX_CHARS`], shaped like a rendering
/// [`paste_names_files`] matches:
///
/// * a path rendering: it starts with one of [`PASTE_PATH_PREFIXES`]; or
/// * bare file names: no `/`, and EVERY whitespace-separated word a file name
///   ([`looks_like_file_name`]);
///
/// and it names no URI scheme but `file`: no other `<scheme>://` anywhere
/// ([`holds_non_file_url`]), and no word opening with another `<scheme>:`
/// ([`opens_with_non_file_scheme`]).
///
/// So a URL, a `mailto:`, a relative `a/b`, a version (`v1.2`), a number (`3.14`)
/// and prose never ask. The shape is not exact, and DOMAIN.md (Compose attach)
/// states both ways it misses: some text still asks (`/compact`, a bare `/`,
/// `example.com`, `a@b.com`, `node.js`), and some copied files' pastes never do
/// (a bare name holding a space or lacking a lettered extension, a path or name
/// holding another scheme). A false positive costs one child read and nothing
/// else, since the upgrade also needs [`paste_names_files`] to hold.
pub(crate) fn paste_may_name_files(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() || t.contains('\n') || t.chars().count() > PASTE_FILE_NAMES_MAX_CHARS {
        return false;
    }
    if holds_non_file_url(t) || t.split_whitespace().any(opens_with_non_file_scheme) {
        return false;
    }
    PASTE_PATH_PREFIXES
        .iter()
        .any(|prefix| t.starts_with(prefix))
        || (!t.contains('/') && t.split_whitespace().all(looks_like_file_name))
}

/// Whether one whitespace-free `word` reads as a file name: a stem, then an
/// extension of at most [`FILE_EXTENSION_MAX_CHARS`] alphanumerics holding at least
/// one letter, so `report.pdf` and `shot.mp4` do and `v1.2` and `3.14` do not.
fn looks_like_file_name(word: &str) -> bool {
    word.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty()
            && ext.chars().count() <= FILE_EXTENSION_MAX_CHARS
            && ext.chars().all(char::is_alphanumeric)
            && ext.chars().any(char::is_alphabetic)
    })
}

/// Whether `text` holds, anywhere, a `<scheme>://` whose scheme is not `file`.
/// The scheme is the WHOLE run of scheme characters before the `://`, from its
/// first letter (RFC 3986 §3.1), so `xfile://` is not `file://`.
fn holds_non_file_url(text: &str) -> bool {
    text.match_indices(URI_AUTHORITY_MARK).any(|(at, _)| {
        let before = &text[..at];
        let run = &before[before.trim_end_matches(is_uri_scheme_char).len()..];
        let scheme = run.trim_start_matches(|c: char| !c.is_ascii_alphabetic());
        !scheme.is_empty() && !is_file_scheme(scheme)
    })
}

/// Whether one whitespace-free `word` opens with `<scheme>:` for a scheme other
/// than `file` (`mailto:a@b.com`, a bare `http:`). A colon inside a path
/// (`/a/b:c.png`) opens nothing.
fn opens_with_non_file_scheme(word: &str) -> bool {
    word.split_once(':')
        .is_some_and(|(scheme, _)| is_uri_scheme(scheme) && !is_file_scheme(scheme))
}

/// Whether `s` is an RFC 3986 §3.1 scheme: a letter, then scheme characters.
fn is_uri_scheme(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && chars.all(is_uri_scheme_char)
}

/// Whether `c` may follow a scheme's first letter (RFC 3986 §3.1).
fn is_uri_scheme_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')
}

/// Whether `scheme` is [`FILE_URI_SCHEME`]'s, the one [`paste_names_files`]
/// matches. Compared exactly, as every `file://` match here and in `clipboard` is.
fn is_file_scheme(scheme: &str) -> bool {
    FILE_URI_SCHEME.strip_suffix(URI_AUTHORITY_MARK) == Some(scheme)
}

/// Whether `pasted` is one terminal rendering of `files`, the files the clipboard
/// holds. An unknown rendering is `false`, so the paste stays verbatim.
///
/// Renderings, each read from the terminal's source (2026-10-06, not tested live):
/// the first path raw (VS Code / xterm.js pastes the first file's `fsPath`); all
/// paths joined by a space; backslash-escaped paths joined by a space (iTerm2
/// `stringFromPasteboard`; compared unescaped, so the escape set need not be
/// mirrored); paths in single or double quotes; `file://` URIs, raw or
/// percent-encoded; base names joined by a space (terminals that paste names only).
/// No newline-joined rendering: [`paste_may_name_files`] asks only about one line,
/// so a paste of files on separate lines stays text, and `Ctrl-V` attaches them.
/// That gate keeps some one-line renderings from ever reaching here too, so they
/// stay text: a bare name holding a space or lacking a lettered extension, and any
/// rendering naming a URI scheme other than `file` (DOMAIN.md, Compose attach).
pub(crate) fn paste_names_files(pasted: &str, files: &[PathBuf]) -> bool {
    let pasted = pasted.trim();
    let paths: Vec<&str> = files.iter().filter_map(|p| p.to_str()).collect();
    if pasted.is_empty() || paths.is_empty() || paths.len() != files.len() {
        return false;
    }
    let names: Vec<&str> = files
        .iter()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
        .collect();
    let spaced = paths.join(" ");
    let wrapped = |q: &str| {
        paths
            .iter()
            .map(|p| format!("{q}{p}{q}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let uris = paths
        .iter()
        .map(|p| format!("{FILE_URI_SCHEME}{p}"))
        .collect::<Vec<_>>()
        .join(" ");
    let unescaped = unescape_backslashes(pasted);
    let decoded = super::clipboard::percent_decode(pasted);
    pasted == paths[0]
        || pasted == spaced
        || unescaped == spaced
        || pasted == wrapped("'")
        || pasted == wrapped("\"")
        || pasted == uris
        || decoded.as_deref() == Some(uris.as_str())
        || (names.len() == files.len() && pasted == names.join(" "))
}

/// `s` with each backslash dropped and the character it escaped kept.
fn unescape_backslashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// What an explicit attach says for a result that attaches nothing, as a sticky
/// refusal; `None` for the results that carry a path, which [`finish_attach`]
/// attaches or names in its own refusal.
fn attach_refusal(result: &ClipboardAttach) -> Option<&'static str> {
    match result {
        ClipboardAttach::Files(_)
        | ClipboardAttach::Image(_)
        | ClipboardAttach::Unmentionable(_) => None,
        ClipboardAttach::TextOnly => Some(ATTACH_TEXT_ONLY),
        ClipboardAttach::Nothing => Some(ATTACH_NOTHING),
        ClipboardAttach::UnsupportedImage => Some(ATTACH_UNSUPPORTED_IMAGE),
        ClipboardAttach::TooLarge => Some(ATTACH_TOO_LARGE),
        ClipboardAttach::RemoteSession => Some(ATTACH_REMOTE),
        ClipboardAttach::NoDisplay => Some(ATTACH_NO_DISPLAY),
        ClipboardAttach::NoTool => Some(ATTACH_NO_TOOL),
        ClipboardAttach::Failed => Some(ATTACH_FAILED),
    }
}

/// The paths of `paths` a mention can carry, in order, and every path none can,
/// with its cause. Asked when a read lands, so a refused path never gets an
/// attachment placeholder.
fn split_mentionable(paths: &[PathBuf]) -> (Vec<&Path>, Vec<(&Path, MentionRefusal)>) {
    let mut accepted = Vec::new();
    let mut refused = Vec::new();
    for path in paths {
        match file_mention(path) {
            Ok(_) => accepted.push(path.as_path()),
            Err(why) => refused.push((path.as_path(), why)),
        }
    }
    (accepted, refused)
}

/// The text a status line holds for a path: quoted and escaped as `Path`'s
/// `Debug` escapes a string, so two files of one name stay apart and a line break
/// cannot split the line. The rule and its source: DOMAIN.md, "The confirmation".
/// The ONE way a path reaches the compose attach's status lines:
/// [`refusal_status`] and [`attached_status`].
fn shown_path(path: &Path) -> String {
    format!("{path:?}")
}

/// ONE status line naming every path of `refused` with its cause, led by
/// `prefix`, each path as [`shown_path`] shows it.
fn refusal_status(prefix: &str, refused: &[(&Path, MentionRefusal)]) -> String {
    let named: Vec<String> = refused
        .iter()
        .map(|(path, why)| {
            format!(
                "{}{ATTACH_REFUSED_CAUSE_SEPARATOR}{}",
                shown_path(path),
                why.describe()
            )
        })
        .collect();
    format!("{prefix}{}", named.join(ATTACH_REFUSED_SEPARATOR))
}

/// Apply a finished clipboard read to the open compose: the handler of
/// `AppEvent::ClipboardRead`.
///
/// Dropped unless a compose is open AND its pending attach holds `token`; a
/// dropped result records no attachment and takes no number, and a dropped
/// `Image`'s path is queued for the driver to delete ([`take_stale_paste_images`]),
/// since no draft holds it. An `Explicit` read inserts at the caret an attachment
/// placeholder for each path a mention can carry, recording each on the box
/// ([`ComposeState::attachments`]) under the next board-wide number
/// ([`App::attachment_number`]), and sets a sticky refusal naming every path it
/// could not carry, or naming why nothing was attached, leaving the draft
/// byte-identical. An `Upgrade` read replaces the pasted text with a placeholder
/// for EVERY one of its files only when the clipboard holds exactly the files it
/// names, the draft has not moved since, and every one of those files can be
/// mentioned. A file no mention can carry keeps the paste as it is, buffer and
/// caret, under a sticky refusal naming every such file; any other outcome keeps it
/// and says nothing, since it was an ordinary paste. Copied files that DO attach
/// are confirmed with their full path ([`confirm_attached`]); an image is not.
pub(crate) fn finish_attach(app: &mut App, token: u64, result: ClipboardAttach) {
    let holds_token = |c: &ComposeState| c.pending_attach.as_ref().map(|p| p.token) == Some(token);
    let Some(compose) = app.compose.as_mut().filter(|c| holds_token(c)) else {
        // `Unmentionable` wrote no file, and `Files` are the user's own.
        if let ClipboardAttach::Image(path) = result {
            app.stale_paste_images.push(path);
        }
        return;
    };
    let Some(PendingAttach { purpose, .. }) = compose.pending_attach.take() else {
        return;
    };
    // An upgrade goes on only when the clipboard holds the very files the paste
    // named and the draft has not moved; otherwise it was an ordinary paste, and
    // nothing is said.
    if let AttachPurpose::Upgrade {
        inserted,
        lines_after,
        cursor_after,
    } = &purpose
    {
        let ClipboardAttach::Files(files) = &result else {
            return;
        };
        let unchanged = compose.textarea.lines() == lines_after.as_slice()
            && cursor_pos(&compose.textarea) == *cursor_after;
        if !unchanged || !paste_names_files(inserted, files) {
            return;
        }
    }
    let (kind, paths) = match &result {
        ClipboardAttach::Files(files) => (AttachmentKind::File, files.clone()),
        // `clipboard::save_image` returns `Unmentionable` only for a path the same
        // `file_mention` refused, so it lands in `refused` below, never in an
        // attachment.
        ClipboardAttach::Image(path) | ClipboardAttach::Unmentionable(path) => {
            (AttachmentKind::Image, vec![path.clone()])
        }
        other => {
            // A refusal sticks until the next keypress, like every failure.
            if let Some(msg) = attach_refusal(other) {
                app.set_status(msg);
            }
            return;
        }
    };
    let (accepted, refused) = split_mentionable(&paths);
    match &purpose {
        AttachPurpose::Upgrade { inserted, .. } => {
            // Whole or not at all: swapping in only the accepted placeholders would
            // delete the refused files' pasted text from the draft.
            if !refused.is_empty() {
                app.set_status(refusal_status(PASTE_KEPT_PREFIX, &refused));
                return;
            }
            if let Some(compose) = app.compose.as_mut() {
                let back = inserted.chars().count();
                for _ in 0..back {
                    compose.textarea.move_cursor(CursorMove::Back);
                }
                compose.textarea.delete_str(back);
                let placeholders =
                    record_attachments(compose, &mut app.attachment_number, kind, &accepted);
                insert_placeholders(compose, &placeholders);
            }
            confirm_attached(app, kind, &accepted);
        }
        AttachPurpose::Explicit => {
            if !accepted.is_empty() {
                if let Some(compose) = app.compose.as_mut() {
                    let placeholders =
                        record_attachments(compose, &mut app.attachment_number, kind, &accepted);
                    insert_placeholders(compose, &placeholders);
                }
            }
            if !refused.is_empty() {
                app.set_status(refusal_status(ATTACH_REFUSED_PREFIX, &refused));
            }
            confirm_attached(app, kind, &accepted);
        }
    }
    refresh_completion(app);
}

/// The confirmation of copied files attached: `attached <first path>` with the
/// path as [`shown_path`] shows it (quoted, escaped), plus ` (+N more)` when there
/// are others. PURE; `None` for no file. A path here passed [`file_mention`].
fn attached_status(accepted: &[&Path]) -> Option<String> {
    let (first, rest) = accepted.split_first()?;
    let more = if rest.is_empty() {
        String::new()
    } else {
        format!("{ATTACHED_MORE_OPEN}{}{ATTACHED_MORE_CLOSE}", rest.len())
    };
    Some(format!("{ATTACHED_PREFIX}{}{more}", shown_path(first)))
}

/// Confirm copied files attached on the status line, unless something already
/// owns it. Only a COPIED FILE is confirmed (a screenshot's cache path means
/// nothing to the user), and never over a refusal, which is sticky where this note
/// is transient: a caller sets its own refusal BEFORE asking, and one a result set
/// while the clipboard read ran is already there.
fn confirm_attached(app: &mut App, kind: AttachmentKind, accepted: &[&Path]) {
    if kind != AttachmentKind::File || app.has_sticky_status() {
        return;
    }
    if let Some(msg) = attached_status(accepted) {
        app.set_status_transient(msg);
    }
}

/// Record one [`Attachment`] of `kind` on `compose` for each of `paths`, in order,
/// each under the next number of `last_number` (`App::attachment_number`, passed
/// as the disjoint field it is), and return their placeholders in that order.
fn record_attachments(
    compose: &mut ComposeState,
    last_number: &mut u64,
    kind: AttachmentKind,
    paths: &[&Path],
) -> Vec<String> {
    paths
        .iter()
        .map(|path| {
            *last_number = last_number.wrapping_add(1);
            let attachment = Attachment {
                kind,
                path: path.to_path_buf(),
                number: *last_number,
            };
            let placeholder = attachment.placeholder();
            compose.attachments.push(attachment);
            placeholder
        })
        .collect()
}

/// Insert `placeholders` at the caret, spaced per [`mention_insert_text`].
fn insert_placeholders(compose: &mut ComposeState, placeholders: &[String]) {
    let (row, col) = cursor_pos(&compose.textarea);
    let before = col
        .checked_sub(1)
        .and_then(|i| compose.textarea.lines().get(row)?.chars().nth(i));
    compose
        .textarea
        .insert_str(mention_insert_text(before, placeholders));
}

/// Recompute the open draft's pick list from its editor, reply, fork and
/// background draft alike, from the source [`completion_source`] names and the
/// folder's cached catalog; a reply or fork whose session left the store gets no
/// list.
///
/// Its bounded reads ([`CompletionState::refresh`]: the transcript listing, once
/// per reply or fork box and only for a top-level `@`, and folder listings, once
/// per folder on every target) run from the key and paste handlers and from
/// `update::dispatch`'s `CatalogFetched` arm — which calls this so a catalog that
/// lands mid-draft reaches an open list — and never from the render path.
///
/// A token that overlaps an attachment placeholder closes the list before any of
/// those reads ([`token_overlaps_placeholder`]).
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
    let lines = compose.textarea.lines();
    let cursor = cursor_pos(&compose.textarea);
    if token_overlaps_placeholder(lines, cursor, &compose.attachments) {
        compose.completion.close();
        return;
    }
    compose.completion.refresh(lines, cursor, view);
}

/// Whether the token [`completion_context`] reads at the caret OVERLAPS the span
/// of a placeholder some entry of `attachments` derives: it starts inside it, ends
/// inside it, or holds all of it. The pick list never reads a placeholder, so it
/// opens on no such token: a pick would write into the placeholder and break it.
/// Whitespace ends a token, yet a token can still reach into a placeholder: a
/// file name holding two spaces in a row (`[a  b.pdf #1]`) lets an `@` typed
/// between them read as a token of its own, and an `@` typed inside a name and
/// walked to the name's end reads as one made of typed characters and the name's.
///
/// Pure, over the editor's lines and the box's attachments; it asks
/// [`completion_context`] and reads no placeholder shape of its own, only the ones
/// [`Attachment::placeholder`] derives, through [`overlaps_placeholder`].
fn token_overlaps_placeholder(
    lines: &[String],
    cursor: (usize, usize),
    attachments: &[Attachment],
) -> bool {
    let Some(ctx) = completion_context(lines, cursor) else {
        return false;
    };
    let Some(line) = lines.get(ctx.row) else {
        return false;
    };
    let line: Vec<char> = line.chars().collect();
    attachments.iter().any(|attachment| {
        let placeholder: Vec<char> = attachment.placeholder().chars().collect();
        overlaps_placeholder(&line, ctx.start..cursor.1, &placeholder)
    })
}

/// Whether the characters `token` (a range of `line`) overlap the span of
/// `placeholder` in `line`.
///
/// WHERE A PLACEHOLDER'S SPAN IS once typing has broken its text: characters
/// typed into a placeholder are only ever in the token being typed, so the span
/// is found by setting aside any of the TOKEN's own characters, none, some or all,
/// and asking whether the placeholder then stands whole: its first and last
/// characters present, every other character of it in order, every character
/// outside the token matching exactly and in place. Its span is then from its
/// first matched character to its last, set-aside characters between them
/// included. The token overlaps it when it holds a matched character, or when
/// every character of it was set aside strictly inside the span (typed between
/// the placeholder's own characters, as an `@` between two spaces of a name).
///
/// That covers a token typed wholly inside, one holding typed characters AND some
/// of the placeholder's own (`@report.pdf` in `[my @report.pdf #1]`), one that
/// starts before the placeholder and ends inside it, one that starts inside it and
/// ends past it, and one that holds all of it. A token that only ABUTS it (ends
/// where the span starts, or starts where it ends) overlaps nothing, and a
/// placeholder that characters OUTSIDE the token have already broken is plain text
/// with no span: it is no longer in the draft ([`expand_attachments`] sends nothing
/// for it).
///
/// Pure over characters; costs at most the placeholder's length squared plus one
/// pass over the token for each PAIR of a prefix and a suffix of the placeholder
/// that the line really holds around the token (few of each).
fn overlaps_placeholder(line: &[char], token: Range<usize>, placeholder: &[char]) -> bool {
    let n = placeholder.len();
    if n == 0 || token.is_empty() || token.end > line.len() {
        return false;
    }
    let (start, end) = (token.start, token.end);
    // How much of the placeholder stands in `line` right before and right after
    // the token, character for character.
    let before = (0..=n.min(start)).filter(|&x| placeholder[..x] == line[start - x..start]);
    let after: Vec<usize> = (0..=n.min(line.len() - end))
        .filter(|&y| placeholder[n - y..] == line[end..end + y])
        .collect();
    before.into_iter().any(|x| {
        after.iter().any(|&y| {
            let middle = placeholder.get(x..n - y);
            match middle {
                // The token sits between two stretches of the placeholder.
                Some([]) => x > 0 && y > 0,
                Some(middle) => is_subsequence(middle, &line[start..end]),
                None => false,
            }
        })
    })
}

/// Whether `needle` occurs in `hay` in order, not necessarily next to each other.
fn is_subsequence(needle: &[char], hay: &[char]) -> bool {
    let mut rest = needle.iter();
    let mut want = rest.next();
    for c in hay {
        if want == Some(c) {
            want = rest.next();
        }
    }
    want.is_none()
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
    let (row, col) = cursor_pos(&compose.textarea);
    let next_is_whitespace = compose
        .textarea
        .lines()
        .get(row)
        .is_some_and(|l| l.chars().nth(col).is_some());
    let text = replacement(ctx.trigger, &state.dir_part, candidate, next_is_whitespace);
    let query_chars = ctx.query.chars().count();
    for _ in 0..query_chars {
        compose.textarea.move_cursor(CursorMove::Back);
    }
    compose.textarea.delete_str(query_chars);
    compose.textarea.insert_str(text);
}

/// The open draft as its submit paths need it: the message to send, what it is
/// addressed to, and the model picked for it (`None` = its default, no
/// `--model`). Cloned out so the borrow of the app ends before anything mutates it
/// — the clone-then-mutate discipline every other handler here follows.
struct Draft {
    /// The editor buffer, lines joined with `\n`, with each attachment placeholder
    /// still in it expanded into its `@"<absolute path>"` mention
    /// ([`expand_attachments`]).
    message: String,
    /// What the draft is addressed to.
    target: ComposeTarget,
    /// The compose's own model pick ([`ComposeState::model`]).
    model: Option<ModelPick>,
    /// The effort the box's default carries ([`ComposeDefault::carried_effort`]):
    /// asked of the SAME decision that words the `model:` label, so the launch sends
    /// exactly what the label names. Only read when `model` is `None`
    /// ([`crate::resume::push_model_or_carried_effort`]).
    carried_effort: Option<&'static str>,
}

/// Read the open draft out of the app (see [`Draft`]). The one reader every
/// submit goes through — [`submit_compose`] and [`open_interactive`] — and so the
/// ONE place an attachment placeholder becomes a mention.
fn draft(app: &App) -> Option<Draft> {
    let compose = app.compose.as_ref()?;
    Some(Draft {
        message: expand_attachments(&compose.textarea.lines().join("\n"), &compose.attachments),
        target: compose.target.clone(),
        model: compose.model.clone(),
        carried_effort: app
            .compose_default()
            .and_then(|default| default.carried_effort()),
    })
}

/// Refuse a draft too long to be one `claude` argument ([`send::check_prompt_len`]):
/// a STICKY status naming its size and the limit, and the box left open with the
/// draft intact. Asked before any launch path routes — `Enter` in every box
/// ([`submit_compose`]) and the fork box's and the draft's `Ctrl-O`
/// ([`open_interactive`]) — so nothing has closed the box, marked a send, armed a
/// fork jump or recorded the agent ([`App::set_last_new_agent`]) by the time it
/// answers: a refused launch is not a start. A draft grows past the limit by typing
/// or by several pastes, each within the paste cap, which is why the check is the
/// draft's, not the paste's, and it is asked of the EXPANDED message ([`draft`]) —
/// the one that becomes the argument — never of the shorter placeholders the editor
/// shows. Returns whether it refused.
fn refuse_too_long(app: &mut App, message: &str) -> bool {
    match send::check_prompt_len(message) {
        Ok(()) => false,
        Err(refusal) => {
            app.set_status(refusal);
            true
        }
    }
}

/// `text` with every occurrence of each of `attachments`' placeholders replaced by
/// the attachment's `@"<absolute path>"` mention ([`file_mention`]). PURE.
///
/// Matches the EXACT placeholder each entry derives, never a parsed `[…#N]`
/// shape, so text no entry of THIS box derives (typed, or pasted from another box,
/// whose numbers this one never hands out) stays text, and a file name holding `[`
/// or `]` cannot confuse it. One left-to-right pass over `text` that never
/// re-scans what it wrote; where two entries could match at one position the
/// earlier-recorded wins, though none can while `file_mention` refuses the `#`
/// every placeholder holds. A mention claude's extractor would not read — glued to
/// the character before it — is led by one space
/// (`docs/agents/CLAUDE_CLI.md`, "File mentions reach claude, bare paths do not").
/// An entry whose mention fails, impossible for a path accepted at attach time,
/// leaves its placeholder as text.
fn expand_attachments(text: &str, attachments: &[Attachment]) -> String {
    let swaps: Vec<(String, String)> = attachments
        .iter()
        .filter_map(|a| file_mention(&a.path).ok().map(|m| (a.placeholder(), m)))
        .collect();
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        let hit = swaps.iter().find_map(|(placeholder, mention)| {
            Some((rest.strip_prefix(placeholder.as_str())?, mention))
        });
        if let Some((after, mention)) = hit {
            if out
                .chars()
                .next_back()
                .is_some_and(|c| !is_mention_boundary(c))
            {
                out.push(' ');
            }
            out.push_str(mention);
            rest = after;
        } else {
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

/// Resolve the compose buffer into a driver [`Outcome`], routing on the open
/// [`ComposeTarget`]: a reply sends, a fork sends headless under a fresh id
/// ([`submit_fork`]), a background draft launches — each with the model picked in
/// THIS compose, if any. A draft over [`send::COMPOSE_PROMPT_MAX_BYTES`] is refused
/// FIRST, for every target ([`refuse_too_long`]), before the target's own guards.
fn submit_compose(app: &mut App) -> Outcome {
    let Some(Draft {
        message,
        target,
        model,
        carried_effort,
    }) = draft(app)
    else {
        return Outcome::Continue;
    };
    if refuse_too_long(app, &message) {
        return Outcome::Continue;
    }
    match target {
        ComposeTarget::Reply {
            session_id,
            stop_job,
        } => submit_reply(
            app,
            message,
            session_id,
            stop_job,
            model.as_ref(),
            carried_effort,
        ),
        ComposeTarget::Fork { session_id } => {
            submit_fork(app, message, session_id, model.as_ref(), carried_effort)
        }
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
/// By the time this runs [`submit_compose`] has already refused a draft over
/// [`send::COMPOSE_PROMPT_MAX_BYTES`]. An empty/whitespace draft keeps the pane open
/// with a gentle nudge (a background agent with no prompt would do nothing);
/// otherwise the launch dir is gated ([`send::plan_bg_launch`]), the argv built, the
/// draft card is marked launching in place, and a [`BgLaunchRequest`] handed to the
/// driver.
///
/// The ONE thing recorded is the AGENT, as the pick `Ctrl-N`'s picker pre-highlights
/// next time — because this is a real launch, and that memory means "the agent of
/// the last new session actually started". It is written past the too-long refusal
/// and the empty-buffer nudge (a draft that launched nothing is not a start) and
/// before the launch-dir gate, so it survives a launch-dir refusal, exactly like the
/// picker's `Ctrl-O`.
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
/// On a [`ComposeTarget::Fork`] it forks INTERACTIVELY instead: an ordinary
/// hand-off through [`super::update::launch_fork_run`] and [`crate::resume`]'s
/// `ForkRun` action, `claude -r <id> --fork-session --name <name> [--model <alias>
/// [--effort <level>] | --effort <level>] [<message>]`. An empty box with no pick and
/// no carried effort is the plain `claude -r <id> --fork-session` plus `--name "fork: <the session's label>"`. The message AUTO-SUBMITS as a
/// trailing positional, like a new session's prompt (probed for `-r
/// --fork-session` at claude 2.1.291 on 2026-10-08; `docs/agents/CLAUDE_CLI.md`).
///
/// While the box's own explicit attach is reading, a draft or a fork hands off
/// nothing: a transient nudge, the box, its text and the read untouched
/// ([`submit_waits_for_attach`]). That wait comes first, as `Enter`'s does.
///
/// A draft over [`send::COMPOSE_PROMPT_MAX_BYTES`] is then refused, on the fork and
/// the draft ([`refuse_too_long`]: sticky status, box kept, nothing recorded — the
/// agent memory above is written only past it).
///
/// INERT on a [`ComposeTarget::Reply`]: a reply addresses a session that already
/// exists, so there is no new-session launch to escape to — and so nothing to
/// refuse either, over the limit or not, and no run to hold for an attach.
fn open_interactive(app: &mut App) -> Outcome {
    let Some(Draft {
        message,
        target,
        model,
        carried_effort,
    }) = draft(app)
    else {
        return Outcome::Continue;
    };
    let waits =
        submit_waits_for_attach(app.compose.as_ref().and_then(|c| c.pending_attach.as_ref()));
    // A reply launches nothing, so it has no prompt to refuse either. The wait comes
    // before the size check, as `Enter`'s does: a draft still taking an attachment is
    // told to wait, not told a size it is about to change.
    if !matches!(target, ComposeTarget::Reply { .. }) && !waits && refuse_too_long(app, &message) {
        return Outcome::Continue;
    }
    // An empty / whitespace draft launches with NO positional, which is exactly what
    // the picker's own `Ctrl-O` (and an untouched fork box's) always emitted; a
    // fork is still named, from the session's label.
    let prompt = (!message.trim().is_empty()).then_some(message);
    // Arm order is load-bearing: the reply's no-op comes before the wait, so a
    // reply's `Ctrl-O` never shows the nudge.
    match target {
        // No interactive launch on the reply target.
        ComposeTarget::Reply { .. } => Outcome::Continue,
        _ if waits => {
            app.set_status_transient(CTRL_O_WHILE_ATTACHING_HINT);
            Outcome::Continue
        }
        ComposeTarget::NewBackgroundAgent { agent } => {
            // The board is about to be torn down for the interactive child, so the
            // card has nothing left to report: close the whole surface.
            app.close_compose();
            app.set_last_new_agent(agent.clone());
            super::update::launch_new_session(
                app,
                agent.as_deref(),
                prompt.as_deref(),
                model.as_ref(),
            )
        }
        ComposeTarget::Fork { session_id } => {
            app.close_compose();
            super::update::launch_fork_run(
                app,
                &session_id,
                prompt.as_deref(),
                model.as_ref(),
                carried_effort,
            )
        }
    }
}

/// Fork the composed session headless — the `Enter` half of the fork target.
///
/// A fork is a reply that lands in a NEW session, so it rides [`send`]'s reply path
/// whole: this function only decides what is different. In order: (1) guard an empty
/// message (a headless run needs one); (2) re-read the original's AUTHORITATIVE
/// `(cwd, session_id)` from inside its file ([`send::plan_send`]), exactly as a
/// reply does; (3) take ONE fresh id for the fork; (4) build
/// `claude -p -r <original> --fork-session --session-id <fork> --name <name> ...`
/// ([`send::build_fork_send_argv`]; the name is the message's first line under the
/// fork prefix, the original's label being the fallback); (5) record the send as in
/// progress under the FORK's id, the same id the request carries so its completion
/// lands on it; and (6) arm the cursor jump to it ([`App::set_fork_jump`]). That one
/// id is the `--session-id`, the in-flight entry, the request and the jump's fork
/// side.
///
/// A draft over [`send::COMPOSE_PROMPT_MAX_BYTES`] never gets here: [`submit_compose`]
/// refused it before this step (1).
///
/// The id is `fork_id`, a parameter, so a test supplies it; the caller passes
/// [`send::fresh_session_id`]. When it is an error the fork is refused with a sticky
/// status and the box stays open, nothing dispatched.
fn submit_fork_with(
    app: &mut App,
    message: String,
    origin_id: String,
    model: Option<&ModelPick>,
    carried_effort: Option<&str>,
    fork_id: std::io::Result<String>,
) -> Outcome {
    if message.trim().is_empty() {
        app.set_status_transient(COMPOSE_EMPTY_FORK_HINT);
        return Outcome::Continue;
    }
    let (file, label) = match app.session_by_id(&origin_id) {
        Some(session) => (session.file.clone(), session.label.clone()),
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
            let fork_id = match fork_id {
                Ok(id) => id,
                Err(err) => {
                    app.set_status(format!("{FORK_ID_FAILED_PREFIX}{err}"));
                    return Outcome::Continue;
                }
            };
            let argv = send::build_fork_send_argv(
                &authoritative_id,
                &fork_id,
                model,
                carried_effort,
                &message,
                &label,
            );
            app.close_compose();
            // The fork has no transcript yet, so there is no turn count to compare
            // the echo against: once the file exists its history is already longer.
            app.mark_sending(super::app::Sending {
                session_id: fork_id.clone(),
                message,
                baseline_msg_count: 0,
            });
            app.set_fork_jump(origin_id, fork_id.clone());
            Outcome::Send(SendRequest {
                argv,
                cwd,
                session_id: fork_id,
                stop_job: None,
            })
        }
        SendPlan::Refuse(message) => {
            app.close_compose();
            app.set_status(message);
            Outcome::Continue
        }
    }
}

/// [`submit_fork_with`] over a fresh OS-random id.
fn submit_fork(
    app: &mut App,
    message: String,
    origin_id: String,
    model: Option<&ModelPick>,
    carried_effort: Option<&str>,
) -> Outcome {
    submit_fork_with(
        app,
        message,
        origin_id,
        model,
        carried_effort,
        send::fresh_session_id(),
    )
}

/// Send the drafted quick reply — the `Enter` half of the reply target.
///
/// [`submit_compose`] has already refused a draft over
/// [`send::COMPOSE_PROMPT_MAX_BYTES`]. Guards an empty/whitespace buffer (keep
/// composing, gentle status). Otherwise it re-reads the AUTHORITATIVE
/// `(cwd, session_id)` from inside the file ([`send::plan_send`]) — never the stale
/// in-memory copy — builds the argv, marks the send in flight, clears the compose
/// state, and hands a [`SendRequest`] to the driver as [`Outcome::Send`]. A refusal
/// (deleted worktree / unreadable file) sets a board status and stays on the board.
///
/// The model picked in THIS reply box (`model`, from `Ctrl-L`) rides along into the
/// argv, and this path is the one where it is not merely a convenience:
/// `claude -p` is non-interactive, so the in-session `/model` command cannot reach
/// it and `--model` is the ONLY way to choose a model for a quick reply. With no
/// pick (`None`) the argv is unchanged and carries no `--model`, so claude normally
/// restores the model the session last answered with — the `model: session (…)`
/// the box showed — and the argv carries `--effort` alone when that label names an
/// effort too (`carried_effort`, [`crate::resume::push_effort_flag`]). After a picked
/// reply claude keeps the session on that model by itself (the next `-r` normally
/// restores it); the effort is kept the same way a carried one is: the next box reads
/// the level that reply recorded off the transcript and sends it again.
fn submit_reply(
    app: &mut App,
    message: String,
    session_id: String,
    stop_job: Option<String>,
    model: Option<&ModelPick>,
    carried_effort: Option<&str>,
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
            let argv = send::build_send_argv(&authoritative_id, model, carried_effort, &message);
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
    use crate::tui::app::{Scope, STATUS_DWELL_TICKS};

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
                later_roots: Vec::new(),
                lineage_root: None,
                msg_count: 0,
                content_index: String::new(),
                background: false,
                has_agent_name: false,
                has_agent_setting: false,
                failed_task: None,
                custom_title: None,
                task_ledger: Default::default(),
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
            later_roots: Vec::new(),
            lineage_root: None,
            msg_count: 0,
            content_index: String::new(),
            background: false,
            has_agent_name: false,
            has_agent_setting: false,
            failed_task: None,
            custom_title: None,
            task_ledger: Default::default(),
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
                ..Candidate::default()
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
            ["agent-Explore", "agent-sby-agent", "src/"],
            "and lists the transcript's agents, which lead the one tier a bare `@` has"
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

    /// A compose folder holding `src/modules/sitemaps/sitemaps-storage.service.ts`.
    fn sitemaps_dir(tag: &str) -> PathBuf {
        let cwd = unique_temp_dir(tag);
        let sitemaps = cwd.join("src/modules/sitemaps");
        std::fs::create_dir_all(&sitemaps).expect("create src/modules/sitemaps/");
        std::fs::write(sitemaps.join("sitemaps-storage.service.ts"), "")
            .expect("write the service");
        cwd
    }

    /// An `@` query whose folder step finds nothing falls back to the compose
    /// folder's root walk, inserted with NO folder part, and like every `@` with a
    /// `/` never reads the transcript the agents come from. When the folder step
    /// finds something, the root walk is never read at all.
    #[test]
    fn a_root_fallback_inserts_whole_paths_and_never_reads_the_transcript() {
        let cwd = sitemaps_dir("at-fallback");
        let transcript = listing_fixture();

        let mut state = CompletionState::default();
        refresh_token(&mut state, "@sitemaps/stor", &cwd, &transcript);
        assert_eq!(
            shown(&state),
            ["src/modules/sitemaps/sitemaps-storage.service.ts"]
        );
        assert_eq!(
            state.dir_part, "",
            "a fallback row replaces the whole query"
        );
        assert!(
            state.transcript.is_none(),
            "a `/` query must not read the transcript"
        );

        let mut folder_step = CompletionState::default();
        refresh_token(&mut folder_step, "@src/", &cwd, &transcript);
        assert_eq!(shown(&folder_step), ["modules/"]);
        assert_eq!(folder_step.dir_part, "src/");
        assert_eq!(
            folder_step.dirs.keys().collect::<Vec<_>>(),
            [&cwd.join("src/")],
            "a folder step that found something never reads the root walk"
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// The fallback re-filters the root walk a top-level `@` already read — the
    /// same cache entry — rather than reading the folder again: a file written
    /// after that read is not listed.
    #[test]
    fn a_root_fallback_reuses_the_top_level_walk() {
        let cwd = sitemaps_dir("at-fallback-cache");
        let transcript = listing_fixture();
        let mut state = CompletionState::default();
        refresh_token(&mut state, "@", &cwd, &transcript);
        std::fs::write(cwd.join("sitemaps-stored-late.ts"), "").expect("write a late file");
        refresh_token(&mut state, "@sitemaps/stor", &cwd, &transcript);
        assert_eq!(
            shown(&state),
            ["src/modules/sitemaps/sitemaps-storage.service.ts"],
            "listed from the cached walk, not a second read"
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// A query neither the folder step nor the root fallback matches shows NO
    /// list, never an empty one that would still claim `Enter`.
    #[test]
    fn a_slash_query_neither_step_matches_shows_no_list() {
        let cwd = sitemaps_dir("at-fallback-none");
        let mut state = CompletionState::default();
        refresh_token(&mut state, "@nope/zzz", &cwd, &listing_fixture());
        assert!(state.visible.is_none());
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
                kind: None,
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

    /// A sendable session whose last answering turn is Opus 5.5 recording
    /// `effort_fields` (raw JSON members, e.g. `"perTurnEffort":"xhigh"`, or empty),
    /// with its preview already rendered — the state a compose box opens over, since
    /// the carried effort comes off the cached render and no file is read on a key.
    fn app_over_effort_session(dir: &std::path::Path, id: &str, effort_fields: &str) -> App {
        let file = dir.join(format!("{id}.jsonl"));
        let comma = if effort_fields.is_empty() { "" } else { "," };
        std::fs::write(
            &file,
            format!(
                r#"{{"type":"user","sessionId":"{id}","cwd":"{cwd}","message":{{"role":"user","content":"hi"}}}}
{{"type":"assistant","sessionId":"{id}","cwd":"{cwd}","message":{{"role":"assistant","model":"claude-opus-5-5","content":"ok"}}{comma}{effort_fields}}}
"#,
                cwd = dir.display()
            ),
        )
        .expect("write the effort fixture");
        let mut app = App::new(
            vec![session_at(id, file, dir.to_path_buf())],
            Scope::All,
            dir.to_path_buf(),
        );
        let _ = app.preview_text(80);
        app
    }

    /// With no pick, the reply, the fork box's `Enter` and its `Ctrl-O` all send the
    /// effort the `model: session (Opus 5.5 · <level>)` label names, `--effort`
    /// alone and ahead of the message, end to end from the cached render through the
    /// submit. A `Ctrl-L` pick wins over it on all three.
    #[test]
    fn the_session_effort_reaches_the_reply_and_both_fork_launches_until_a_pick_wins() {
        let dir = unique_temp_dir("carry-effort");
        let ctrl_o = || with_mods(KeyCode::Char('o'), KeyModifiers::CONTROL);

        let mut app = app_over_effort_session(&dir, "sbc-carry", r#""perTurnEffort":"xhigh""#);
        app.open_compose(ComposeState::new_reply("sbc-carry".to_string(), None), None);
        assert_eq!(
            app.compose_default().map(|d| d.picker_label()),
            Some("session's model (Opus 5.5 \u{b7} xhigh)".to_string())
        );
        type_into(&mut app, "ship it");
        match handle_compose_key(&mut app, key(KeyCode::Enter)) {
            Outcome::Send(req) => assert_eq!(
                req.argv.join(" "),
                "claude -p -r sbc-carry --output-format json --effort xhigh ship it"
            ),
            _ => panic!("Enter on a drafted reply must send"),
        }

        let mut app = app_over_effort_session(&dir, "sbc-carry", r#""perTurnEffort":"xhigh""#);
        open_fork(&mut app, "sbc-carry".to_string());
        type_into(&mut app, "try it");
        let Outcome::Send(req) = handle_compose_key(&mut app, key(KeyCode::Enter)) else {
            panic!("Enter on a typed fork box must send");
        };
        let tail = &req.argv[req.argv.len() - 4..];
        assert_eq!(
            tail,
            ["json", "--effort", "xhigh", "try it"],
            "the flag sits after the fixed head and ahead of the message: {:?}",
            req.argv
        );
        assert!(req.argv.iter().any(|a| a == "--fork-session"));
        assert!(!req.argv.iter().any(|a| a == "--model"));

        let mut app = app_over_effort_session(&dir, "sbc-carry", r#""perTurnEffort":"xhigh""#);
        open_fork(&mut app, "sbc-carry".to_string());
        type_into(&mut app, "try it");
        let Outcome::Resume(ready) = handle_compose_key(&mut app, ctrl_o()) else {
            panic!("Ctrl-O must hand off");
        };
        assert_eq!(
            ready.argv.join(" "),
            "claude -r sbc-carry --fork-session --name fork: try it --effort xhigh try it"
        );
        assert_eq!(ready.nonzero_hint, crate::resume::FORK_NONZERO_HINT);

        // A pick wins on every route: its own model, its own effort, never the
        // session's.
        let mut app = app_over_effort_session(&dir, "sbc-carry", r#""perTurnEffort":"xhigh""#);
        app.open_compose(ComposeState::new_reply("sbc-carry".to_string(), None), None);
        app.set_compose_model(Some(ModelPick::new("sonnet")));
        type_into(&mut app, "ship it");
        match handle_compose_key(&mut app, key(KeyCode::Enter)) {
            Outcome::Send(req) => assert_eq!(
                req.argv.join(" "),
                "claude -p -r sbc-carry --output-format json --model sonnet ship it"
            ),
            _ => panic!("Enter on a drafted reply must send"),
        }
        let mut app = app_over_effort_session(&dir, "sbc-carry", r#""perTurnEffort":"xhigh""#);
        open_fork(&mut app, "sbc-carry".to_string());
        app.set_compose_model(Some(ModelPick::new("sonnet")));
        type_into(&mut app, "try it");
        let Outcome::Resume(ready) = handle_compose_key(&mut app, ctrl_o()) else {
            panic!("Ctrl-O must hand off");
        };
        assert_eq!(
            ready.argv.join(" "),
            "claude -r sbc-carry --fork-session --name fork: try it --model sonnet try it"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Nothing is carried when the box's default is not the session's own model with
    /// a KNOWN effort: a last turn that recorded none, an unknown or flag-shaped level, an environment override of the
    /// model, and a session with no answering model.
    #[test]
    fn no_effort_is_carried_for_a_cleared_strange_overridden_or_modelless_session() {
        let dir = unique_temp_dir("carry-effort-none");
        let bare = "claude -p -r sbc-none --output-format json ship it";
        let send_from = |app: &mut App| {
            app.open_compose(ComposeState::new_reply("sbc-none".to_string(), None), None);
            type_into(app, "ship it");
            match handle_compose_key(app, key(KeyCode::Enter)) {
                Outcome::Send(req) => req.argv.join(" "),
                _ => panic!("Enter on a drafted reply must send"),
            }
        };

        for strange in [
            r#""perTurnEffort":"ultra""#,
            r#""perTurnEffort":"--model""#,
            r#""perTurnEffort":"""#,
            r#""perTurnEffort":7"#,
            "",
        ] {
            let mut app = app_over_effort_session(&dir, "sbc-none", strange);
            assert_eq!(send_from(&mut app), bare, "{strange:?}");
        }

        let mut app = app_over_effort_session(&dir, "sbc-none", r#""perTurnEffort":"high""#);
        app.set_restore_overridden(true);
        assert_eq!(
            send_from(&mut app),
            bare,
            "an overridden model carries nothing"
        );

        // A session with no answering model: the plain fixture has no assistant turn.
        let mut app = App::new(
            vec![sendable_session(&dir, "sbc-none")],
            Scope::All,
            dir.clone(),
        );
        let _ = app.preview_text(80);
        assert_eq!(send_from(&mut app), bare, "no answering model on record");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A real two-turn transcript whose first turn ran at `xhigh` and whose LAST turn
    /// recorded no effort (`sess-effort-cleared-1`) sends no `--effort` on the reply,
    /// the fork box's `Enter` or its `Ctrl-O`: the newer turn clears the older level
    /// rather than leaving it standing. The control, `sess-effort-present-1` (one turn
    /// at `xhigh`), carries it on the same three routes, so a fixture that failed to
    /// load could not pass this vacuously.
    #[test]
    fn a_last_turn_without_effort_after_one_with_it_sends_no_effort_on_any_route() {
        let dir = unique_temp_dir("carry-effort-cleared");
        // The fixtures name a `cwd` that does not exist here; the send gate needs one
        // that does, so the copy is pointed at the temp dir.
        let app_over_fixture = |id: &str| {
            let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/preview")
                .join(format!("{id}.jsonl"));
            let file = dir.join(format!("{id}.jsonl"));
            std::fs::write(
                &file,
                std::fs::read_to_string(&source)
                    .expect("read the preview fixture")
                    .replace("/Users/me/project-alpha", &dir.display().to_string()),
            )
            .expect("write the fixture copy");
            let mut app = App::new(
                vec![session_at(id, file, dir.clone())],
                Scope::All,
                dir.clone(),
            );
            let _ = app.preview_text(80);
            app
        };
        let ctrl_o = || with_mods(KeyCode::Char('o'), KeyModifiers::CONTROL);
        // The argv of the reply, the fork's `Enter` and the fork's `Ctrl-O`, in order.
        let argvs = |id: &str| -> [Vec<String>; 3] {
            let mut app = app_over_fixture(id);
            app.open_compose(ComposeState::new_reply(id.to_string(), None), None);
            type_into(&mut app, "go on");
            let Outcome::Send(reply) = handle_compose_key(&mut app, key(KeyCode::Enter)) else {
                panic!("Enter on a drafted reply must send");
            };

            let mut app = app_over_fixture(id);
            open_fork(&mut app, id.to_string());
            type_into(&mut app, "go on");
            let Outcome::Send(fork) = handle_compose_key(&mut app, key(KeyCode::Enter)) else {
                panic!("Enter on a typed fork box must send");
            };

            let mut app = app_over_fixture(id);
            open_fork(&mut app, id.to_string());
            type_into(&mut app, "go on");
            let Outcome::Resume(run) = handle_compose_key(&mut app, ctrl_o()) else {
                panic!("Ctrl-O must hand off");
            };
            [reply.argv, fork.argv, run.argv]
        };

        let routes = ["reply Enter", "fork Enter", "fork Ctrl-O"];
        let carried = argvs("sess-effort-present-1");
        for (route, argv) in routes.iter().zip(&carried) {
            assert!(
                argv.windows(2).any(|pair| pair == ["--effort", "xhigh"]),
                "{route}: the control must carry its xhigh: {argv:?}"
            );
        }

        let cleared = argvs("sess-effort-cleared-1");
        for (route, argv) in routes.iter().zip(&cleared) {
            assert!(
                !argv.iter().any(|a| a == "--effort" || a == "xhigh"),
                "{route}: the older turn's effort must not outlive a later turn without one: {argv:?}"
            );
        }
        assert_eq!(
            cleared[0].join(" "),
            "claude -p -r sess-effort-cleared-1 --output-format json go on"
        );
        assert_eq!(
            cleared[2].join(" "),
            "claude -r sess-effort-cleared-1 --fork-session --name fork: go on go on"
        );

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

    /// The fork box's `Enter` ties ONE generated id together: it is the
    /// `--session-id` in the command, the id of the in-flight send, the id of the
    /// request (so the completion lands on it), and the fork side of the jump pair,
    /// and it is not the original's. The command forks the AUTHORITATIVE original.
    #[test]
    fn the_fork_boxs_enter_ties_one_fresh_id_through_command_send_and_jump() {
        let dir = unique_temp_dir("fork-ties");
        let mut app = App::new(
            vec![sendable_session(&dir, "sbf-origin")],
            Scope::All,
            dir.clone(),
        );
        open_fork(&mut app, "sbf-origin".to_string());
        app.set_compose_model(Some(ModelPick {
            model: "opus".to_string(),
            effort: Some("low"),
        }));
        type_into(&mut app, "try it");
        let fork_id = "11111111-2222-4333-8444-555555555555";
        let pick = app.compose.as_ref().and_then(|c| c.model.clone());
        let out = submit_fork_with(
            &mut app,
            "try it".to_string(),
            "sbf-origin".to_string(),
            pick.as_ref(),
            None,
            Ok(fork_id.to_string()),
        );
        let Outcome::Send(req) = out else {
            panic!("Enter on a typed fork box must send");
        };
        assert_eq!(
            req.argv.join(" "),
            format!(
                "claude -p -r sbf-origin --fork-session --session-id {fork_id} \
                 --name fork: try it --output-format json --model opus --effort low try it"
            )
        );
        assert_eq!(req.session_id, fork_id);
        assert_ne!(req.session_id, "sbf-origin");
        assert!(req.stop_job.is_none());
        let sending = app
            .sending_to(fork_id)
            .expect("the send is recorded under the fork");
        assert_eq!(sending.message, "try it");
        assert!(
            app.sending_to("sbf-origin").is_none(),
            "the original is not marked"
        );
        assert_eq!(
            app.fork_jump,
            Some(crate::tui::app::ForkJump {
                origin: "sbf-origin".to_string(),
                fork: fork_id.to_string(),
            })
        );
        assert!(app.compose.is_none(), "the box closes");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Through the real key and the real id source: `Enter` generates a UUID-shaped id
    /// that differs from the original's and is the one command, request and jump use.
    #[test]
    fn the_fork_boxs_enter_uses_a_real_generated_id() {
        let dir = unique_temp_dir("fork-real-id");
        let mut app = App::new(
            vec![sendable_session(&dir, "sbf-real")],
            Scope::All,
            dir.clone(),
        );
        open_fork(&mut app, "sbf-real".to_string());
        type_into(&mut app, "go");
        let Outcome::Send(req) = handle_compose_key(&mut app, key(KeyCode::Enter)) else {
            panic!("Enter on a typed fork box must send");
        };
        assert_ne!(req.session_id, "sbf-real");
        assert_eq!(req.session_id.len(), 36);
        assert!(req.argv.contains(&req.session_id));
        assert_eq!(
            app.fork_jump.as_ref().map(|j| j.fork.as_str()),
            Some(req.session_id.as_str())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An empty fork box keeps the box open with a nudge; an id the OS would not give
    /// refuses with a sticky status, keeps the box (and the text) and dispatches
    /// nothing — no in-flight entry, no jump.
    #[test]
    fn the_fork_box_nudges_when_empty_and_refuses_without_an_id() {
        let dir = unique_temp_dir("fork-refusals");
        let mut app = App::new(
            vec![sendable_session(&dir, "sbf-refuse")],
            Scope::All,
            dir.clone(),
        );
        open_fork(&mut app, "sbf-refuse".to_string());
        assert!(matches!(
            handle_compose_key(&mut app, key(KeyCode::Enter)),
            Outcome::Continue
        ));
        assert!(app.compose.is_some(), "an empty fork box stays open");
        assert_eq!(app.status.as_deref(), Some(COMPOSE_EMPTY_FORK_HINT));

        type_into(&mut app, "go");
        let out = submit_fork_with(
            &mut app,
            "go".to_string(),
            "sbf-refuse".to_string(),
            None,
            None,
            Err(std::io::Error::other("no entropy")),
        );
        assert!(matches!(out, Outcome::Continue));
        assert!(app.compose.is_some(), "the box and its text survive");
        assert!(app
            .status
            .as_deref()
            .is_some_and(|s| s.starts_with(FORK_ID_FAILED_PREFIX) && s.contains("no entropy")));
        assert!(app.sending.is_empty());
        assert!(app.fork_jump.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A draft over [`send::COMPOSE_PROMPT_MAX_BYTES`], one byte too many.
    fn over_long_draft() -> String {
        "z".repeat(send::COMPOSE_PROMPT_MAX_BYTES + 1)
    }

    /// Assert `out` was a REFUSAL of an over-long draft: nothing dispatched, the box
    /// still open with the whole draft, and a STICKY status naming the size and limit.
    fn assert_refused_and_kept(app: &App, out: &Outcome, draft: &str) {
        assert!(
            matches!(out, Outcome::Continue),
            "a refusal dispatches nothing"
        );
        let compose = app.compose.as_ref().expect("the box stays open");
        assert_eq!(
            compose.textarea.lines().join("\n"),
            draft,
            "the draft is intact"
        );
        let status = app.status.as_deref().expect("a refusal is shown");
        assert!(
            status.contains(&draft.len().to_string()),
            "names the size: {status}"
        );
        assert!(
            status.contains(&send::COMPOSE_PROMPT_MAX_BYTES.to_string()),
            "names the limit: {status}"
        );
        assert_eq!(
            app.status_ttl, None,
            "a refusal stays until the next keypress"
        );
        assert!(app.sending.is_empty(), "no send was marked");
        assert!(app.fork_jump.is_none());
    }

    /// Every launch path that would carry the draft as ONE `claude` argument refuses
    /// a draft past the limit before it closes the box: the reply's `Enter`, the
    /// fork's `Enter` and `Ctrl-O`, the background draft's `Enter` and `Ctrl-O`.
    #[test]
    fn an_over_long_draft_is_refused_on_every_launch_path_and_the_box_stays_open() {
        let dir = unique_temp_dir("over-long");
        let draft = over_long_draft();
        let enter = || key(KeyCode::Enter);
        let ctrl_o = || with_mods(KeyCode::Char('o'), KeyModifiers::CONTROL);
        let fresh = || {
            App::new(
                vec![sendable_session(&dir, "sbl-origin")],
                Scope::All,
                dir.clone(),
            )
        };

        let mut app = fresh();
        open(&mut app, "sbl-origin".to_string(), None);
        type_into(&mut app, &draft);
        let out = handle_compose_key(&mut app, enter());
        assert_refused_and_kept(&app, &out, &draft);

        let mut app = fresh();
        open_fork(&mut app, "sbl-origin".to_string());
        type_into(&mut app, &draft);
        let out = handle_compose_key(&mut app, enter());
        assert_refused_and_kept(&app, &out, &draft);

        let mut app = fresh();
        open_fork(&mut app, "sbl-origin".to_string());
        type_into(&mut app, &draft);
        let out = handle_compose_key(&mut app, ctrl_o());
        assert_refused_and_kept(&app, &out, &draft);

        let mut app = fresh();
        open_background(&mut app, None);
        type_into(&mut app, &draft);
        let out = handle_compose_key(&mut app, enter());
        assert_refused_and_kept(&app, &out, &draft);

        let mut app = fresh();
        open_background(&mut app, None);
        type_into(&mut app, &draft);
        let out = handle_compose_key(&mut app, ctrl_o());
        assert_refused_and_kept(&app, &out, &draft);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The agent the next `Ctrl-N` picker pre-highlights, which is the only public
    /// read of `App::last_new_agent`: closes the compose, opens the picker over
    /// `previous` and `reviewer`, and names the highlighted row's agent.
    fn remembered_agent(app: &mut App) -> Option<String> {
        use crate::defined_agents::DefinedAgent;
        use crate::tui::app::ModalAction;

        let agent = |name: &str| DefinedAgent {
            name: name.to_string(),
            description: None,
        };
        app.close_compose();
        app.open_agent_picker(vec![agent("previous"), agent("reviewer")]);
        match app.modal.as_ref().and_then(|m| m.selected_action()) {
            Some(ModalAction::New(picked)) => picked.clone(),
            other => panic!("the agent picker did not open on a New row: {other:?}"),
        }
    }

    /// A refused draft records NO agent: `last_new_agent` means "the agent of the last
    /// new session actually started", so the refusal must come before
    /// `set_last_new_agent`, on `Enter` and on `Ctrl-O` alike. The draft opens on an
    /// agent that DIFFERS from the one already remembered, so a refusal that recorded
    /// first would be visible; the existing every-path test opens its drafts with
    /// `agent: None`, over an app that remembers none, and would not notice.
    #[test]
    fn a_refused_draft_does_not_record_its_agent() {
        let dir = unique_temp_dir("over-long-agent");
        let draft = over_long_draft();
        let fresh = || {
            let mut app = App::new(
                vec![sendable_session(&dir, "sbl-agent")],
                Scope::All,
                dir.clone(),
            );
            app.set_last_new_agent(Some("previous".to_string()));
            open_background(&mut app, Some("reviewer".to_string()));
            type_into(&mut app, &draft);
            app
        };

        let mut app = fresh();
        let out = handle_compose_key(&mut app, key(KeyCode::Enter));
        assert_refused_and_kept(&app, &out, &draft);
        assert_eq!(
            remembered_agent(&mut app).as_deref(),
            Some("previous"),
            "Enter's refusal must not record the draft's agent"
        );

        let mut app = fresh();
        let out = handle_compose_key(
            &mut app,
            with_mods(KeyCode::Char('o'), KeyModifiers::CONTROL),
        );
        assert_refused_and_kept(&app, &out, &draft);
        assert_eq!(
            remembered_agent(&mut app).as_deref(),
            Some("previous"),
            "Ctrl-O's refusal must not record the draft's agent"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A draft of EXACTLY the limit is not refused: it launches on every path, and the
    /// reply's inert `Ctrl-O` stays inert (no launch, no refusal) however long.
    #[test]
    fn a_draft_at_exactly_the_limit_is_not_refused() {
        let dir = unique_temp_dir("at-limit");
        let draft = "z".repeat(send::COMPOSE_PROMPT_MAX_BYTES);
        let enter = || key(KeyCode::Enter);
        let ctrl_o = || with_mods(KeyCode::Char('o'), KeyModifiers::CONTROL);
        let fresh = || {
            App::new(
                vec![sendable_session(&dir, "sbl-edge")],
                Scope::All,
                dir.clone(),
            )
        };

        let mut app = fresh();
        open(&mut app, "sbl-edge".to_string(), None);
        type_into(&mut app, &draft);
        assert!(matches!(
            handle_compose_key(&mut app, enter()),
            Outcome::Send(_)
        ));
        assert!(app.compose.is_none());

        let mut app = fresh();
        open_fork(&mut app, "sbl-edge".to_string());
        type_into(&mut app, &draft);
        assert!(matches!(
            handle_compose_key(&mut app, enter()),
            Outcome::Send(_)
        ));

        let mut app = fresh();
        open_fork(&mut app, "sbl-edge".to_string());
        type_into(&mut app, &draft);
        assert!(matches!(
            handle_compose_key(&mut app, ctrl_o()),
            Outcome::Resume(_)
        ));

        let mut app = fresh();
        open_background(&mut app, None);
        type_into(&mut app, &draft);
        assert!(matches!(
            handle_compose_key(&mut app, enter()),
            Outcome::BgLaunch(_)
        ));

        let mut app = fresh();
        open_background(&mut app, None);
        type_into(&mut app, &draft);
        assert!(matches!(
            handle_compose_key(&mut app, ctrl_o()),
            Outcome::Resume(_)
        ));

        // The reply's Ctrl-O launches nothing: inert, and says nothing, over the
        // limit or not.
        let mut app = fresh();
        open(&mut app, "sbl-edge".to_string(), None);
        type_into(&mut app, &over_long_draft());
        assert!(matches!(
            handle_compose_key(&mut app, ctrl_o()),
            Outcome::Continue
        ));
        assert!(app.compose.is_some());
        assert_eq!(app.status, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `Ctrl-O` in the fork box forks interactively: an untouched box is
    /// `claude -r <id> --fork-session` (what a fork has always handed off) named after
    /// the session's label, a typed one is named after its first line and adds the
    /// message, and a pick adds `--model`/`--effort` BEFORE the message with the
    /// model-worded non-zero hint.
    #[test]
    fn ctrl_o_in_the_fork_box_forks_interactively() {
        let dir = unique_temp_dir("fork-interactive");
        let mut app = App::new(
            vec![sendable_session(&dir, "sbf-run")],
            Scope::All,
            dir.clone(),
        );
        let ctrl_o = || with_mods(KeyCode::Char('o'), KeyModifiers::CONTROL);

        open_fork(&mut app, "sbf-run".to_string());
        let Outcome::Resume(ready) = handle_compose_key(&mut app, ctrl_o()) else {
            panic!("Ctrl-O must hand off");
        };
        let mut plain_named = crate::resume::build_argv("sbf-run", true);
        plain_named.extend(["--name".to_string(), "fork: label sbf-run".to_string()]);
        assert_eq!(ready.argv, plain_named);
        assert_eq!(ready.nonzero_hint, crate::resume::FORK_NONZERO_HINT);
        assert!(ready.race_probe_id.is_none());
        assert!(app.compose.is_none());

        open_fork(&mut app, "sbf-run".to_string());
        app.set_compose_model(Some(ModelPick {
            model: "haiku".to_string(),
            effort: Some("medium"),
        }));
        type_into(&mut app, "ship it");
        let Outcome::Resume(ready) = handle_compose_key(&mut app, ctrl_o()) else {
            panic!("Ctrl-O must hand off");
        };
        assert_eq!(
            ready.argv.join(" "),
            "claude -r sbf-run --fork-session --name fork: ship it --model haiku --effort \
             medium ship it"
        );
        assert_eq!(ready.nonzero_hint, crate::resume::MODEL_NONZERO_HINT);
        assert!(app.fork_jump.is_none(), "no jump after Ctrl-O");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The fork box offers the same pick list source as a reply: the session's
    /// folder and transcript.
    #[test]
    fn the_fork_box_reads_its_sessions_folder_for_its_pick_list() {
        let dir = unique_temp_dir("fork-source");
        let app = App::new(
            vec![sendable_session(&dir, "sbf-src")],
            Scope::All,
            PathBuf::from("/tmp"),
        );
        let source = completion_source(
            &app,
            &ComposeTarget::Fork {
                session_id: "sbf-src".to_string(),
            },
        )
        .expect("the session is on the board");
        assert_eq!(source.cwd, dir);
        assert!(source.transcript.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The fork box's session leaves the store while the box is open: `Enter` and
    /// `Ctrl-O` (through `update::launch_fork_run`) each close the box, say so in
    /// their own words, and dispatch nothing — no send, no in-flight entry, no jump,
    /// no hand-off.
    #[test]
    fn the_fork_box_dispatches_nothing_for_a_session_that_left_the_store() {
        let dir = unique_temp_dir("fork-gone");
        let mut app = App::new(Vec::new(), Scope::All, dir.clone());
        let ctrl_o = with_mods(KeyCode::Char('o'), KeyModifiers::CONTROL);
        for (pressed, gone) in [
            (key(KeyCode::Enter), COMPOSE_SESSION_GONE),
            (ctrl_o, COMPOSE_FORK_SESSION_GONE),
        ] {
            app.apply_sessions(vec![sendable_session(&dir, "sbf-gone")]);
            open_fork(&mut app, "sbf-gone".to_string());
            type_into(&mut app, "go");
            app.apply_sessions(Vec::new());

            assert!(
                matches!(handle_compose_key(&mut app, pressed), Outcome::Continue),
                "{gone}"
            );
            assert!(app.compose.is_none(), "{gone}");
            assert_eq!(app.status.as_deref(), Some(gone));
            assert!(app.sending.is_empty(), "{gone}");
            assert!(app.fork_jump.is_none(), "{gone}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `Enter` re-reads the original from its file at that moment
    /// ([`send::plan_send`]): a folder deleted after the box opened refuses with the
    /// re-read's own words, closes the box and dispatches nothing.
    #[test]
    fn the_fork_boxs_enter_refuses_a_folder_deleted_after_the_box_opened() {
        let dir = unique_temp_dir("fork-plan-refused");
        let mut app = App::new(
            vec![sendable_session(&dir, "sbf-deleted")],
            Scope::All,
            dir.clone(),
        );
        open_fork(&mut app, "sbf-deleted".to_string());
        type_into(&mut app, "go");
        let missing = dir.join("deleted-worktree");
        std::fs::write(
            dir.join("sbf-deleted.jsonl"),
            format!(
                r#"{{"type":"user","sessionId":"sbf-deleted","cwd":"{}","message":{{"role":"user","content":"hi"}}}}"#,
                missing.display()
            ),
        )
        .expect("rewrite the transcript");

        assert!(matches!(
            handle_compose_key(&mut app, key(KeyCode::Enter)),
            Outcome::Continue
        ));
        assert!(app.compose.is_none(), "the box closes on the refusal");
        assert!(app.status.as_deref().is_some_and(
            |s| s.contains("no longer exists") && s.contains(&missing.display().to_string())
        ));
        assert!(app.sending.is_empty());
        assert!(app.fork_jump.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn ctrl_v() -> KeyEvent {
        with_mods(KeyCode::Char('v'), KeyModifiers::CONTROL)
    }

    #[test]
    fn ctrl_v_attaches_from_the_clipboard() {
        for open in [false, true] {
            for c in ['v', 'V'] {
                assert_eq!(
                    compose_key_to_action(with_mods(KeyCode::Char(c), KeyModifiers::CONTROL), open),
                    ComposeAction::AttachClipboard
                );
            }
        }
        assert_eq!(
            compose_key_to_action(key(KeyCode::Char('v')), false),
            ComposeAction::Forward
        );
    }

    #[test]
    fn a_pending_attach_is_requested_once_and_a_second_ctrl_v_only_nudges() {
        let mut app = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp"));
        app.open_compose(ComposeState::new_background(None), None);
        assert_eq!(take_clipboard_read(&mut app), None);
        handle_compose_key(&mut app, ctrl_v());
        let token = take_clipboard_read(&mut app)
            .expect("asks the driver once")
            .0;
        assert_eq!(take_clipboard_read(&mut app), None);
        handle_compose_key(&mut app, ctrl_v());
        assert_eq!(app.status.as_deref(), Some(ATTACH_BUSY_HINT));
        assert_eq!(take_clipboard_read(&mut app), None);
        assert_eq!(
            app.compose
                .as_ref()
                .unwrap()
                .pending_attach
                .as_ref()
                .unwrap()
                .token,
            token
        );
    }

    /// A submit (`Enter`, or `Ctrl-O`) waits only for an EXPLICIT read, requested
    /// by the driver or not yet; never for the silent copied-file check, and never
    /// with nothing pending.
    #[test]
    fn submit_waits_only_for_an_explicit_attach() {
        let pending = |requested, purpose| PendingAttach {
            token: 1,
            requested,
            purpose,
        };
        let upgrade = AttachPurpose::Upgrade {
            inserted: "/tmp/a.png".to_string(),
            lines_after: vec!["/tmp/a.png".to_string()],
            cursor_after: (0, 10),
        };
        let rows = [
            (None, false, "nothing pending"),
            (
                Some(pending(false, AttachPurpose::Explicit)),
                true,
                "an explicit read not yet handed to the driver",
            ),
            (
                Some(pending(true, AttachPurpose::Explicit)),
                true,
                "an explicit read in flight",
            ),
            (
                Some(pending(false, upgrade.clone())),
                false,
                "a silent check not yet handed over",
            ),
            (
                Some(pending(true, upgrade)),
                false,
                "a silent check in flight",
            ),
        ];
        for (pending, waits, case) in rows {
            assert_eq!(submit_waits_for_attach(pending.as_ref()), waits, "{case}");
        }
    }

    #[test]
    fn an_explicit_image_result_inserts_a_placeholder_and_a_stale_token_is_dropped() {
        let mut app = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp"));
        app.open_compose(ComposeState::new_background(None), None);
        type_into(&mut app, "look");
        handle_compose_key(&mut app, ctrl_v());
        let token = take_clipboard_read(&mut app).unwrap().0;
        finish_attach(
            &mut app,
            token + 1,
            ClipboardAttach::Image(PathBuf::from("/tmp/a.png")),
        );
        assert_eq!(app.compose.as_ref().unwrap().textarea.lines(), ["look"]);
        finish_attach(
            &mut app,
            token,
            ClipboardAttach::Image(PathBuf::from("/tmp/a.png")),
        );
        assert_eq!(
            app.compose.as_ref().unwrap().textarea.lines(),
            ["look [Image #1] "]
        );
        assert!(app.compose.as_ref().unwrap().pending_attach.is_none());
    }

    #[test]
    fn a_text_only_result_refuses_and_leaves_the_draft_alone() {
        let mut app = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp"));
        app.open_compose(ComposeState::new_background(None), None);
        type_into(&mut app, "x");
        handle_compose_key(&mut app, ctrl_v());
        let token = take_clipboard_read(&mut app).unwrap().0;
        finish_attach(&mut app, token, ClipboardAttach::TextOnly);
        assert_eq!(app.compose.as_ref().unwrap().textarea.lines(), ["x"]);
        assert_eq!(app.status.as_deref(), Some(ATTACH_TEXT_ONLY));
    }

    #[test]
    fn a_file_shaped_paste_upgrades_only_when_the_clipboard_names_it_and_the_draft_is_unchanged() {
        let files = vec![PathBuf::from("/tmp/my doc.pdf")];
        let open = |app: &mut App| {
            app.open_compose(ComposeState::new_background(None), None);
            insert_paste(app, "/tmp/my doc.pdf");
            take_clipboard_read(app)
                .expect("a path-shaped paste asks the clipboard")
                .0
        };
        let mut app = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp"));
        let token = open(&mut app);
        finish_attach(&mut app, token, ClipboardAttach::Files(files.clone()));
        assert_eq!(
            app.compose.as_ref().unwrap().textarea.lines(),
            ["[my doc.pdf #1] "]
        );

        let mut app = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp"));
        let token = open(&mut app);
        type_into(&mut app, "!");
        finish_attach(&mut app, token, ClipboardAttach::Files(files.clone()));
        assert_eq!(
            app.compose.as_ref().unwrap().textarea.lines(),
            ["/tmp/my doc.pdf!"]
        );

        let mut app = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp"));
        let token = open(&mut app);
        finish_attach(
            &mut app,
            token,
            ClipboardAttach::Files(vec![PathBuf::from("/tmp/other.pdf")]),
        );
        assert_eq!(
            app.compose.as_ref().unwrap().textarea.lines(),
            ["/tmp/my doc.pdf"]
        );
        assert_eq!(app.status, None);
    }

    #[test]
    fn paste_names_files_matches_each_rendering() {
        let files = vec![PathBuf::from("/a/x y.png"), PathBuf::from("/b/z.txt")];
        for yes in [
            "/a/x y.png",
            "/a/x y.png /b/z.txt",
            "/a/x\\ y.png /b/z.txt",
            "'/a/x y.png' '/b/z.txt'",
            "\"/a/x y.png\" \"/b/z.txt\"",
            "file:///a/x y.png file:///b/z.txt",
            "file:///a/x%20y.png file:///b/z.txt",
            "x y.png z.txt",
            // Any rendering, with whitespace around it.
            " /a/x y.png\n",
        ] {
            assert!(paste_names_files(yes, &files), "{yes}");
        }
        // A near miss of EVERY rendering above: its shape over the wrong files, or
        // a broken variant of it. Collected, so a loosened rendering names every
        // row it lets through.
        let matched: Vec<&str> = [
            "",
            "hello",
            // the first path raw: another file, or a path that is not the first
            "/a/other.png",
            "/b/z.txt",
            // raw paths joined by a space: the wrong files, or the wrong order
            "/a/x y.png /b/other.txt",
            "/b/z.txt /a/x y.png",
            // the very files joined by a newline, paths or names: no rendering,
            // since `paste_may_name_files` never asks about a second line
            "/a/x y.png\n/b/z.txt",
            "x y.png\nz.txt",
            // backslash-escaped paths joined by a space
            "/a/x\\ y.png /b/other.txt",
            // single-quoted, and quotes mixed
            "'/a/x y.png' '/b/other.txt'",
            "'/a/x y.png' \"/b/z.txt\"",
            // double-quoted
            "\"/a/x y.png\" \"/b/other.txt\"",
            // `file://` URIs raw, and the scheme on one path only
            "file:///a/x y.png file:///b/other.txt",
            "file:///a/x y.png /b/z.txt",
            // `file://` URIs percent-encoded
            "file:///a/x%20y.png file:///b/other.txt",
            // base names joined by a space
            "x y.png other.txt",
            // whitespace is trimmed around the paste only, never inside a path
            "/a/x  y.png",
        ]
        .into_iter()
        .filter(|no| paste_names_files(no, &files))
        .collect();
        assert!(
            matched.is_empty(),
            "near misses taken as the files: {matched:?}"
        );
    }

    #[test]
    fn paste_may_name_files_is_single_line_and_path_or_name_shaped() {
        assert!(paste_may_name_files("/tmp/a b.png"));
        assert!(paste_may_name_files("report.pdf"));
        assert!(!paste_may_name_files("hello world"));
        assert!(!paste_may_name_files("/a\n/b"));
        assert!(!paste_may_name_files(""));
    }

    /// Pins `PASTE_FILE_NAMES_MAX_CHARS` (1024) by literal lengths, so a changed
    /// cap moves one side of the boundary and fails here.
    #[test]
    fn paste_may_name_files_asks_about_a_1024_char_path_but_not_a_1025_char_one() {
        let at_limit = format!("/{}", "a".repeat(1023));
        let past_limit = format!("/{}", "a".repeat(1024));
        assert_eq!(at_limit.chars().count(), 1024, "fixture");
        assert_eq!(past_limit.chars().count(), 1025, "fixture");
        assert!(paste_may_name_files(&at_limit));
        assert!(!paste_may_name_files(&past_limit));
    }

    /// Pins `FILE_EXTENSION_MAX_CHARS` (8) by literal lengths: a `/`-free name
    /// with an 8-char extension looks like a file, a 9-char one does not.
    #[test]
    fn paste_may_name_files_takes_an_8_char_extension_but_not_a_9_char_one() {
        assert!(paste_may_name_files("notes.abcdefgh"));
        assert!(!paste_may_name_files("notes.abcdefghi"));
    }

    /// Only a paste shaped like a rendering [`paste_names_files`] matches asks the
    /// clipboard, and the stated exceptions (DOMAIN.md, Compose attach) still do,
    /// so a later tightening is a visible change here. Collected, so a loosened or
    /// tightened shape names every row it moves.
    #[test]
    fn paste_may_name_files_asks_only_about_a_files_rendering() {
        let asks = [
            // a raw absolute path, and absolute paths joined by a space
            "/abs/x.png",
            "/a/x y.png /b/z.txt",
            // iTerm2's backslash-escaped paths start with the `/` itself
            "/Users/me/My\\ File.png",
            // an absolute path in single or double quotes
            "'/abs/x.png'",
            "\"/abs/x.png\"",
            // `file://` URIs, raw or percent-encoded
            "file:///abs/x.png",
            "file:///abs/x%20y.png",
            // `file` is the one scheme a paste may hold: alone, as several URIs,
            // or beside a path
            "file:///a/b.png",
            "file:///a/b.png file:///c/d.png",
            "/a/b.png file:///c/d.png",
            // home-relative: asked, though no rendering can match it
            "~/x.png",
            // bare names, one or several, whitespace around them trimmed
            "report.pdf",
            "shot.JPEG notes.md",
            "  report.pdf\n",
            // the stated exceptions: text that still costs a read, as a leading-`/`
            // word, a bare `/`, or dotted words that read as file names
            "/compact",
            "/",
            "example.com",
            "a@b.com",
            "node.js",
        ];
        let never = [
            // a scheme other than `file://`
            "https://example.com/x.png",
            "ftp://host/x.txt",
            // ... anywhere in the paste, after a path that would ask
            "/x https://y",
            "/a/b.png https://example.com/c",
            "'/a/b.png' ftp://h/f",
            // ... even where no word opens with it
            "/a/b.png (https://example.com)",
            "/x/https://y",
            // the scheme is the WHOLE run before `://`: `xfile` is not `file`
            "/a/xfile:///b.png",
            // a word opening with a `scheme:` other than `file:`
            "mailto:a@b.com",
            "http:",
            "x.png mailto:a@b.com",
            // a quoted `file://` URI: no rendering quotes one, so no prefix asks
            "'file:///a/b.png'",
            // a relative path
            "a/b",
            "src/main.rs",
            // a version or a number: no letter in the extension
            "v1.2",
            "3.14",
            "1.0.0",
            // prose, ending in a file name or not
            "hello world",
            "the build is green.",
            "see notes.txt",
            // a bare name holding a space reads as words
            "my doc.pdf",
            // a dotfile has no stem
            ".bashrc",
            // more than one line, and nothing at all
            "/a\n/b",
            "",
        ];
        let misjudged: Vec<(&str, bool)> = asks
            .iter()
            .map(|t| (*t, true))
            .chain(never.iter().map(|t| (*t, false)))
            .filter(|(t, want)| paste_may_name_files(t) != *want)
            .collect();
        assert!(
            misjudged.is_empty(),
            "misjudged (paste, should ask): {misjudged:?}"
        );
    }

    /// `Esc` cancels exactly the read `Enter` and `Ctrl-O` wait for — an EXPLICIT
    /// one, handed to the driver or not yet — and never the silent copied-file
    /// check or nothing at all, which leave `Esc` to cancel the box.
    #[test]
    fn esc_cancels_only_an_explicit_attach() {
        let pending = |requested, purpose| PendingAttach {
            token: 1,
            requested,
            purpose,
        };
        let upgrade = AttachPurpose::Upgrade {
            inserted: "/tmp/a.png".to_string(),
            lines_after: vec!["/tmp/a.png".to_string()],
            cursor_after: (0, 10),
        };
        let rows = [
            (None, false, "nothing pending"),
            (
                Some(pending(false, AttachPurpose::Explicit)),
                true,
                "an explicit read not yet handed to the driver",
            ),
            (
                Some(pending(true, AttachPurpose::Explicit)),
                true,
                "an explicit read in flight",
            ),
            (
                Some(pending(false, upgrade.clone())),
                false,
                "a silent check not yet handed over",
            ),
            (
                Some(pending(true, upgrade)),
                false,
                "a silent check in flight",
            ),
        ];
        for (pending, cancels, case) in rows {
            assert_eq!(esc_cancels_attach(pending.as_ref()), cancels, "{case}");
            assert_eq!(
                submit_waits_for_attach(pending.as_ref()),
                cancels,
                "{case}: a read that holds the box is one `Esc` releases"
            );
        }
    }

    /// An explicit attach inserts a placeholder for every path a mention can carry,
    /// the refused ones taking no number, and names EVERY refused path, whole, with
    /// its own cause, on one sticky line.
    #[test]
    fn an_explicit_attach_inserts_the_accepted_placeholders_and_names_every_refused_path() {
        let mut app = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp"));
        app.open_compose(ComposeState::new_background(None), None);
        handle_compose_key(&mut app, ctrl_v());
        let token = take_clipboard_read(&mut app).unwrap().0;
        finish_attach(
            &mut app,
            token,
            ClipboardAttach::Files(vec![
                PathBuf::from("/tmp/a.png"),
                PathBuf::from("/tmp/bad#name.png"),
                PathBuf::from("/tmp/c.png"),
                PathBuf::from("/tmp/q\"uote.png"),
            ]),
        );
        assert_eq!(
            app.compose.as_ref().unwrap().textarea.lines(),
            ["[a.png #1] [c.png #2] "]
        );
        assert_eq!(
            app.status.as_deref(),
            Some(
                "not attached: \"/tmp/bad#name.png\" \u{2014} its path contains a #; \
                 \"/tmp/q\\\"uote.png\" \u{2014} its path contains a double quote"
            )
        );
        assert!(app.status_ttl.is_none(), "a refusal is sticky");
    }

    /// An explicit attach that can mention none of its files leaves the draft
    /// byte-identical, caret included, and names every one; a newline in a path is
    /// escaped, so the refusal stays ONE line.
    #[test]
    fn an_explicit_attach_with_every_path_refused_leaves_the_draft_and_names_all() {
        let mut app = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp"));
        app.open_compose(ComposeState::new_background(None), None);
        type_into(&mut app, "keep");
        let before = draft_snapshot(&app);
        handle_compose_key(&mut app, ctrl_v());
        let token = take_clipboard_read(&mut app).unwrap().0;
        finish_attach(
            &mut app,
            token,
            ClipboardAttach::Files(vec![
                PathBuf::from("/tmp/a#.png"),
                PathBuf::from("/tmp/b\nc.png"),
            ]),
        );
        assert_eq!(draft_snapshot(&app), before);
        assert_eq!(
            app.status.as_deref(),
            Some(
                "not attached: \"/tmp/a#.png\" \u{2014} its path contains a #; \
                 \"/tmp/b\\nc.png\" \u{2014} its path contains a control character"
            )
        );
        assert!(app.status_ttl.is_none(), "a refusal is sticky");
    }

    /// An image the read could not save, because no mention can carry its cache
    /// path, leaves the draft byte-identical and names that path and its cause on
    /// the same sticky `not attached: …` line a refused file gets.
    #[test]
    fn an_unmentionable_image_names_its_path_and_leaves_the_draft() {
        let mut app = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp"));
        app.open_compose(ComposeState::new_background(None), None);
        type_into(&mut app, "keep");
        let before = draft_snapshot(&app);
        handle_compose_key(&mut app, ctrl_v());
        let token = take_clipboard_read(&mut app).unwrap().0;
        finish_attach(
            &mut app,
            token,
            ClipboardAttach::Unmentionable(PathBuf::from("/cfg#1/cache/paste/paste-1-2.png")),
        );
        assert_eq!(draft_snapshot(&app), before);
        assert_eq!(
            app.status.as_deref(),
            Some(
                "not attached: \"/cfg#1/cache/paste/paste-1-2.png\" \u{2014} its path contains a #"
            )
        );
        assert!(app.status_ttl.is_none(), "a refusal is sticky");
    }

    /// The open draft's lines and caret.
    fn draft_snapshot(app: &App) -> (Vec<String>, (usize, usize)) {
        let textarea = &app.compose.as_ref().unwrap().textarea;
        (textarea.lines().to_vec(), cursor_pos(textarea))
    }

    /// Paste `text` into a fresh draft holding `tail` AFTER the caret, finish its
    /// upgrade check with `files`, and return the app and the draft right after the
    /// paste.
    fn upgrade_with(text: &str, files: &[&str]) -> (App, (Vec<String>, (usize, usize))) {
        let mut app = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp"));
        app.open_compose(ComposeState::new_background(None), None);
        type_into(&mut app, "tail");
        app.compose
            .as_mut()
            .unwrap()
            .textarea
            .move_cursor(CursorMove::Head);
        insert_paste(&mut app, text);
        let pasted = draft_snapshot(&app);
        let token = take_clipboard_read(&mut app)
            .expect("a path-shaped paste asks the clipboard")
            .0;
        let files = files.iter().map(PathBuf::from).collect();
        finish_attach(&mut app, token, ClipboardAttach::Files(files));
        (app, pasted)
    }

    /// A paste the clipboard confirms, one of whose files no mention can carry, is
    /// not upgraded at all: the verbatim paste and its caret stay, and a sticky
    /// refusal names the refused file.
    #[test]
    fn an_upgrade_with_one_refused_path_keeps_the_paste_and_caret_and_names_it() {
        let (app, pasted) =
            upgrade_with("/tmp/a.png /tmp/b#c.png", &["/tmp/a.png", "/tmp/b#c.png"]);
        assert_eq!(
            pasted,
            (vec!["/tmp/a.png /tmp/b#c.pngtail".to_owned()], (0, 23)),
            "the paste went in before the caret, ahead of the typed tail"
        );
        assert_eq!(draft_snapshot(&app), pasted);
        assert_eq!(
            app.status.as_deref(),
            Some("paste kept as text, cannot attach: \"/tmp/b#c.png\" \u{2014} its path contains a #")
        );
        assert!(app.status_ttl.is_none(), "a refusal is sticky");
    }

    /// Every refused file of a kept paste is named, whole, so two of one name in
    /// different folders stay apart.
    #[test]
    fn an_upgrade_with_two_refused_paths_names_both() {
        let (app, pasted) = upgrade_with(
            "/a/shot#1.png /b/shot#1.png",
            &["/a/shot#1.png", "/b/shot#1.png"],
        );
        assert_eq!(draft_snapshot(&app), pasted);
        assert_eq!(
            app.status.as_deref(),
            Some(
                "paste kept as text, cannot attach: \"/a/shot#1.png\" \u{2014} its path contains a #; \
                 \"/b/shot#1.png\" \u{2014} its path contains a #"
            )
        );
    }

    /// Every result that attaches nothing leaves the draft byte-identical and says
    /// its own words.
    #[test]
    fn each_no_attach_result_sets_its_exact_refusal_and_leaves_the_draft_alone() {
        let rows = [
            (ClipboardAttach::TextOnly, ATTACH_TEXT_ONLY),
            (ClipboardAttach::Nothing, ATTACH_NOTHING),
            (ClipboardAttach::UnsupportedImage, ATTACH_UNSUPPORTED_IMAGE),
            (ClipboardAttach::TooLarge, ATTACH_TOO_LARGE),
            (ClipboardAttach::RemoteSession, ATTACH_REMOTE),
            (ClipboardAttach::NoDisplay, ATTACH_NO_DISPLAY),
            (ClipboardAttach::NoTool, ATTACH_NO_TOOL),
            (ClipboardAttach::Failed, ATTACH_FAILED),
        ];
        let mut seen = std::collections::HashSet::new();
        for (result, want) in rows {
            assert!(seen.insert(want), "{want}: each refusal has its own words");
            let mut app = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp"));
            app.open_compose(ComposeState::new_background(None), None);
            type_into(&mut app, "keep");
            handle_compose_key(&mut app, ctrl_v());
            let token = take_clipboard_read(&mut app).unwrap().0;
            finish_attach(&mut app, token, result.clone());
            assert_eq!(app.status.as_deref(), Some(want), "{result:?}");
            assert_eq!(app.compose.as_ref().unwrap().textarea.lines(), ["keep"]);
        }
        assert_eq!(
            ATTACH_REMOTE,
            "cannot read the clipboard over SSH \u{2014} it would be the remote machine's"
        );
        assert_eq!(
            ATTACH_NOTHING,
            "nothing to attach \u{2014} the clipboard is empty"
        );
    }

    /// The silent file-paste check never saves an image and never blocks `Ctrl-V`.
    #[test]
    fn an_upgrade_read_lists_only_and_an_explicit_ctrl_v_takes_over() {
        let mut app = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp"));
        app.open_compose(ComposeState::new_background(None), None);
        insert_paste(&mut app, "/tmp/a.png");
        let (upgrade_token, mode) = take_clipboard_read(&mut app).unwrap();
        assert_eq!(mode, ReadMode::ListOnly);
        assert!(!app
            .compose
            .as_ref()
            .unwrap()
            .pending_attach
            .as_ref()
            .unwrap()
            .shows_mark());
        handle_compose_key(&mut app, ctrl_v());
        assert_eq!(app.status, None, "no busy nudge for a silent check");
        let (token, mode) = take_clipboard_read(&mut app).expect("the explicit read starts");
        assert_eq!(mode, ReadMode::Attach);
        assert_ne!(token, upgrade_token);
        finish_attach(
            &mut app,
            upgrade_token,
            ClipboardAttach::Files(vec![PathBuf::from("/tmp/a.png")]),
        );
        assert_eq!(
            app.compose.as_ref().unwrap().textarea.lines(),
            ["/tmp/a.png"],
            "the superseded check's result is dropped"
        );
    }

    /// A confirmed copied-file paste of two files becomes a placeholder for EACH,
    /// numbered in the clipboard's order, in place of the pasted span.
    #[test]
    fn an_upgrade_inserts_a_placeholder_for_every_clipboard_file() {
        let mut app = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp"));
        app.open_compose(ComposeState::new_background(None), None);
        insert_paste(&mut app, "/tmp/a.png /tmp/b.pdf");
        let token = take_clipboard_read(&mut app)
            .expect("a path-shaped paste asks the clipboard")
            .0;
        finish_attach(
            &mut app,
            token,
            ClipboardAttach::Files(vec![
                PathBuf::from("/tmp/a.png"),
                PathBuf::from("/tmp/b.pdf"),
            ]),
        );
        assert_eq!(
            app.compose.as_ref().unwrap().textarea.lines(),
            ["[a.png #1] [b.pdf #2] "]
        );
    }

    /// Finish an explicit `Ctrl-V` read with `result` on a fresh draft.
    fn explicit_attach_with(result: ClipboardAttach) -> App {
        let mut app = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp"));
        app.open_compose(ComposeState::new_background(None), None);
        handle_compose_key(&mut app, ctrl_v());
        let token = take_clipboard_read(&mut app).unwrap().0;
        finish_attach(&mut app, token, result);
        app
    }

    fn files(paths: &[&str]) -> ClipboardAttach {
        ClipboardAttach::Files(paths.iter().map(PathBuf::from).collect())
    }

    /// The confirmation is transient: it expires on its own.
    fn assert_transient_status(app: &App, want: &str) {
        assert_eq!(app.status.as_deref(), Some(want));
        assert_eq!(
            app.status_ttl,
            Some(STATUS_DWELL_TICKS),
            "a confirmation expires"
        );
    }

    #[test]
    fn attached_status_names_the_first_path_and_counts_the_rest() {
        let p = |s: &'static str| Path::new(s);
        assert_eq!(attached_status(&[]), None);
        assert_eq!(
            attached_status(&[p("/a/x.pdf")]).as_deref(),
            Some("attached \"/a/x.pdf\"")
        );
        assert_eq!(
            attached_status(&[p("/a/x.pdf"), p("/b/y.png"), p("/c/z.txt")]).as_deref(),
            Some("attached \"/a/x.pdf\" (+2 more)")
        );
    }

    /// The path is shown by the refusal lines' own quoting, so each row's path
    /// reads as `Debug` escapes it. Each row: what the path holds, the path, and
    /// the line the status holds. Collected, so a broken row names itself.
    #[test]
    fn attached_status_escapes_what_would_mislead_a_reader() {
        let rows: [(&str, &str, &str); 7] = [
            (
                "a bidi override",
                "/tmp/doc\u{202e}fdp.exe",
                "attached \"/tmp/doc\\u{202e}fdp.exe\"",
            ),
            (
                "a zero-width space",
                "/tmp/a\u{200b}b.pdf",
                "attached \"/tmp/a\\u{200b}b.pdf\"",
            ),
            (
                "an ideographic space",
                "/tmp/a\u{3000}b.pdf",
                "attached \"/tmp/a\\u{3000}b.pdf\"",
            ),
            (
                "combining marks (a decomposed e-acute)",
                "/tmp/Re\u{301}sume\u{301}.pdf",
                "attached \"/tmp/Re\\u{301}sume\\u{301}.pdf\"",
            ),
            (
                "a private-use code point",
                "/tmp/a\u{e000}b.pdf",
                "attached \"/tmp/a\\u{e000}b.pdf\"",
            ),
            (
                "a backslash",
                "/tmp/a\\b.pdf",
                "attached \"/tmp/a\\\\b.pdf\"",
            ),
            (
                "a plain space (kept, quoted)",
                "/tmp/my doc.pdf",
                "attached \"/tmp/my doc.pdf\"",
            ),
        ];
        let wrong: Vec<String> = rows
            .iter()
            .filter_map(|(what, path, want)| {
                let got = attached_status(&[Path::new(path)]);
                (got.as_deref() != Some(*want)).then(|| format!("{what}: {got:?}, want {want:?}"))
            })
            .collect();
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[test]
    fn an_explicit_attach_of_one_file_confirms_its_full_path() {
        let app = explicit_attach_with(files(&["/tmp/docs/report.pdf"]));
        assert_transient_status(&app, "attached \"/tmp/docs/report.pdf\"");
    }

    #[test]
    fn an_explicit_attach_of_several_files_names_the_first_and_counts_the_rest() {
        let app = explicit_attach_with(files(&["/tmp/a.pdf", "/tmp/b.pdf", "/tmp/c.pdf"]));
        assert_transient_status(&app, "attached \"/tmp/a.pdf\" (+2 more)");
    }

    #[test]
    fn a_screenshot_attach_gets_no_confirmation() {
        let app = explicit_attach_with(ClipboardAttach::Image(PathBuf::from(
            "/cfg/cache/paste/paste-1-2.png",
        )));
        assert_eq!(
            app.compose.as_ref().unwrap().textarea.lines(),
            ["[Image #1] "]
        );
        assert_eq!(app.status, None);
    }

    #[test]
    fn an_upgrade_of_one_file_confirms_its_full_path() {
        let (app, _) = upgrade_with("/tmp/docs/a.png", &["/tmp/docs/a.png"]);
        assert_transient_status(&app, "attached \"/tmp/docs/a.png\"");
    }

    #[test]
    fn an_upgrade_of_several_files_names_the_first_and_counts_the_rest() {
        let (app, _) = upgrade_with(
            "/tmp/a.png /tmp/b.pdf /tmp/c.txt",
            &["/tmp/a.png", "/tmp/b.pdf", "/tmp/c.txt"],
        );
        assert_transient_status(&app, "attached \"/tmp/a.png\" (+2 more)");
    }

    /// A refusal is never replaced: this attach's own (the accepted files still go
    /// in, only the sticky refusal shows), nor one already on the line when the
    /// read lands.
    #[test]
    fn a_confirmation_never_replaces_a_refusal() {
        let app = explicit_attach_with(files(&["/tmp/a.png", "/tmp/bad#name.png"]));
        assert_eq!(
            app.compose.as_ref().unwrap().textarea.lines(),
            ["[a.png #1] "]
        );
        assert!(app
            .status
            .as_deref()
            .is_some_and(|s| s.starts_with("not attached: ")));
        assert!(app.status_ttl.is_none(), "the refusal stays sticky");

        let mut app = App::new(Vec::new(), Scope::All, PathBuf::from("/tmp"));
        app.open_compose(ComposeState::new_background(None), None);
        handle_compose_key(&mut app, ctrl_v());
        let token = take_clipboard_read(&mut app).unwrap().0;
        app.set_status("an earlier failure");
        finish_attach(&mut app, token, files(&["/tmp/a.png"]));
        assert_eq!(
            app.compose.as_ref().unwrap().textarea.lines(),
            ["[a.png #1] "]
        );
        assert_eq!(app.status.as_deref(), Some("an earlier failure"));
        assert!(app.status_ttl.is_none());
    }

    /// The first, narrowest case of the overlap rule: a token the caret ends that
    /// sits typed wholly INSIDE a placeholder some entry derives, which then stands
    /// whole around it with the token taken out. The rule is wider than these rows
    /// (a token that starts inside, ends inside or holds a placeholder; its own table is
    /// `a_token_overlapping_a_recorded_placeholder_is_told_from_one_clear_of_it`).
    /// The rows are the caret cases: between the two spaces of a file name that
    /// holds two in a row, with the token's query typed on, past the placeholder's
    /// trailing space, before the placeholder, a lookalike no entry derives, and
    /// another entry's number.
    #[test]
    fn a_token_typed_inside_a_recorded_placeholder_is_told_from_one_beside_it() {
        let held = vec![Attachment {
            kind: AttachmentKind::File,
            path: PathBuf::from("/tmp/a  b.pdf"),
            number: 1,
        }];
        let one = |text: &str, col: usize, attachments: &[Attachment]| {
            token_overlaps_placeholder(&[text.to_owned()], (0, col), attachments)
        };
        // `[a ` is 3 columns, so column 4 is right after an `@` typed between the
        // two spaces: `[a @ b.pdf #1]`.
        assert!(one("[a @ b.pdf #1]", 4, &held), "between the two spaces");
        assert!(one("[a @x b.pdf #1]", 5, &held), "its query typed on");
        assert!(
            !one("[a  b.pdf #1] @", 15, &held),
            "past the trailing space"
        );
        assert!(
            !one("@ [a  b.pdf #1]", 1, &held),
            "a token before the placeholder"
        );
        assert!(!one("[a @ b.pdf #1]", 4, &[]), "no entry derives it");
        let other = vec![Attachment {
            number: 2,
            ..held[0].clone()
        }];
        assert!(!one("[a @ b.pdf #1]", 4, &other), "another entry's number");
    }

    /// The guard through the caret, as a keystroke reaches it: a token the caret
    /// ends overlaps a recorded placeholder when it holds typed characters AND the
    /// placeholder's own, or is begun by them, or starts before the placeholder and
    /// ends inside it, not only when it was typed wholly inside. A token that stands
    /// clear of the placeholder, or one beside a placeholder that characters outside
    /// the token have already broken, overlaps nothing. Collected, so a broken rule
    /// names every row it breaks.
    #[test]
    fn a_token_overlapping_a_recorded_placeholder_is_told_from_one_clear_of_it() {
        let held: Vec<Attachment> = [("/tmp/a  b.pdf", 1), ("/tmp/my report.pdf", 2)]
            .into_iter()
            .map(|(path, number)| Attachment {
                kind: AttachmentKind::File,
                path: PathBuf::from(path),
                number,
            })
            .collect();
        // (the line, the caret column, whether the token overlaps a placeholder)
        let rows: [(&str, usize, bool); 11] = [
            ("[a @ b.pdf #1]", 4, true),
            ("[my @report.pdf #2]", 15, true),
            ("[my @report.pdfX #2]", 16, true),
            ("[my @re@port.pdf #2]", 16, true),
            ("@x[my report.pdf #2]", 5, true),
            ("/x[my report.pdf #2]", 5, true),
            ("@x [my report.pdf #2]", 2, false),
            ("[my report.pdf #2] @x", 21, false),
            ("[my x @report.pdf #2]", 17, false),
            ("[my @report.pdf #9]", 15, false),
            ("[a b @ c.pdf #1]", 6, false),
        ];
        let wrong: Vec<String> = rows
            .iter()
            .filter_map(|&(text, col, want)| {
                let lines = [text.to_owned()];
                // A caret with no token never reaches the overlap rule, so a row
                // there would prove nothing about it.
                if completion_context(&lines, (0, col)).is_none() {
                    return Some(format!("{text:?} at {col}: no token reaches the rule"));
                }
                let got = token_overlaps_placeholder(&lines, (0, col), &held);
                (got != want).then(|| format!("{text:?} at {col}: got {got}, want {want}"))
            })
            .collect();
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    /// Where a placeholder's span is, against a token range given outright, so the
    /// relations a keystroke cannot reach are asked too: a token inside it (set
    /// aside wholly, partly, or not at all), straddling its start, straddling its
    /// end and holding all of it overlap; one ending where it starts or starting
    /// where it ends does not, nor does an empty token or one beside a placeholder
    /// that characters outside the token have broken. Collected, so a broken rule
    /// names every row it breaks.
    #[test]
    fn a_placeholder_span_is_told_from_a_token_range_by_how_they_overlap() {
        let chars = |text: &str| -> Vec<char> { text.chars().collect() };
        let small = chars("[a.pdf #1]");
        let long = chars("[my report.pdf #1]");
        // (the row, the line, the token's range, the placeholder, overlaps)
        type Row<'a> = (&'a str, &'a str, Range<usize>, &'a [char], bool);
        let rows: [Row; 14] = [
            (
                "inside, typed between its characters",
                "[a.@pdf #1]",
                3..4,
                &small,
                true,
            ),
            (
                "inside, its own characters alone",
                "[a.pdf #1]",
                1..6,
                &small,
                true,
            ),
            (
                "inside, typed and its own characters",
                "[my @report.pdf #1]",
                4..15,
                &long,
                true,
            ),
            (
                "inside, typed in two places",
                "[my @re@port.pdf #1]",
                4..16,
                &long,
                true,
            ),
            ("straddling the start", "@x[a.pdf #1]", 0..5, &small, true),
            ("straddling the end", "[a.pdf #1]@xy", 7..13, &small, true),
            ("enclosing", "@[a.pdf #1]x", 0..12, &small, true),
            ("adjacent on the left", "@x[a.pdf #1]", 0..2, &small, false),
            (
                "adjacent on the right",
                "[a.pdf #1]@x",
                10..12,
                &small,
                false,
            ),
            ("clear on the left", "@x [a.pdf #1]", 0..2, &small, false),
            ("clear on the right", "[a.pdf #1] @x", 11..13, &small, false),
            ("an empty token", "[a.pdf #1]", 3..3, &small, false),
            (
                "broken outside the token",
                "[my x @report.pdf #1]",
                6..17,
                &long,
                false,
            ),
            ("an empty placeholder", "@x", 0..2, &[], false),
        ];
        let wrong: Vec<String> = rows
            .iter()
            .filter_map(|(row, text, token, placeholder, want)| {
                let got = overlaps_placeholder(&chars(text), token.clone(), placeholder);
                (got != *want).then(|| format!("{row}: got {got}, want {want}"))
            })
            .collect();
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    /// Each row: the draft text, the box's attachments, and the message sent. Only
    /// the exact placeholder an entry derives is swapped, every occurrence of it,
    /// and a mention glued to what precedes it is led by one space. Collected, so a
    /// broken rule names every row it breaks.
    #[test]
    fn expand_attachments_swaps_only_each_recorded_placeholder() {
        let entry = |kind, path: &str, number| Attachment {
            kind,
            path: PathBuf::from(path),
            number,
        };
        let shot = entry(AttachmentKind::Image, "/tmp/red square.png", 1);
        let other = entry(AttachmentKind::Image, "/tmp/b.png", 2);
        let bracketed = entry(AttachmentKind::File, "/tmp/[draft] v2.pdf", 3);
        let rows: Vec<(&str, &str, Vec<Attachment>, &str)> = vec![
            (
                "at the start",
                "[Image #1] hi",
                vec![shot.clone()],
                "@\"/tmp/red square.png\" hi",
            ),
            (
                "after a space",
                "see [Image #1] ",
                vec![shot.clone()],
                "see @\"/tmp/red square.png\" ",
            ),
            (
                "after a newline",
                "line\n[Image #1]",
                vec![shot.clone()],
                "line\n@\"/tmp/red square.png\"",
            ),
            (
                "glued to a word",
                "look[Image #1] ",
                vec![shot.clone()],
                "look @\"/tmp/red square.png\" ",
            ),
            (
                "after a NEL, which is no boundary to claude",
                "a\u{85}[Image #1]",
                vec![shot.clone()],
                "a\u{85} @\"/tmp/red square.png\"",
            ),
            (
                "after a BOM and an ideographic space, which are",
                "\u{feff}[Image #1]\u{3000}[Image #2]",
                vec![shot.clone(), other.clone()],
                "\u{feff}@\"/tmp/red square.png\"\u{3000}@\"/tmp/b.png\"",
            ),
            (
                "two glued placeholders",
                "[Image #1][Image #2]",
                vec![shot.clone(), other.clone()],
                "@\"/tmp/red square.png\" @\"/tmp/b.png\"",
            ),
            (
                "a file name holding [ and ]",
                "[[draft] v2.pdf #3] ",
                vec![bracketed.clone()],
                "@\"/tmp/[draft] v2.pdf\" ",
            ),
            (
                "a typed placeholder no entry derives",
                "[Image #9] ",
                vec![shot.clone()],
                "[Image #9] ",
            ),
            (
                "another name under an entry's number",
                "[report.pdf #2] [Image #2]",
                vec![other.clone()],
                "[report.pdf #2] @\"/tmp/b.png\"",
            ),
            (
                "the same placeholder twice",
                "[Image #1] and [Image #1]",
                vec![shot.clone()],
                "@\"/tmp/red square.png\" and @\"/tmp/red square.png\"",
            ),
            (
                "an entry whose placeholder was deleted",
                "just text",
                vec![shot.clone()],
                "just text",
            ),
            (
                "no placeholder at all",
                "naïve\t[Image#1] [draft] v2.pdf @\"/x\"\n",
                vec![shot, bracketed],
                "naïve\t[Image#1] [draft] v2.pdf @\"/x\"\n",
            ),
        ];
        let wrong: Vec<String> = rows
            .into_iter()
            .filter_map(|(case, text, attachments, want)| {
                let got = expand_attachments(text, &attachments);
                (got != want).then(|| format!("{case}: {got:?} != {want:?}"))
            })
            .collect();
        assert!(wrong.is_empty(), "{wrong:#?}");
    }
}
