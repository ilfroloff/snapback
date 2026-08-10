# Architecture

Static structure: what the pieces are and how they connect. For the rules on
*building new things*, see [PATTERNS.md](PATTERNS.md); for the Claude Code
session model, see [DOMAIN.md](DOMAIN.md).

## Identity

`snapback` (short alias `sb`) is a single self-contained Rust binary: a
[ratatui](https://ratatui.rs) TUI that browses, searches, and resumes **Claude
Code** sessions stored as JSONL under `~/.claude/projects/`, plus the two board
actions that need no teardown — quick reply (`Ctrl-R`) and interrupt (`Ctrl-K`).
It exists because the built-in `/resume` picker is per-project with no cross-folder
view and no content search. It depends on nothing outside its own `Cargo.toml`.

That last sentence still holds despite `npm/` existing. The npm package is a
**distribution channel, not a dependency**: it carries prebuilt binaries and
copies one onto your `PATH`. The installed program is the same self-contained
Rust binary, with no node anywhere near it. Nothing under `src/` may ever import,
shell out to, or assume node.

## Stack

| Concern | Crate | Pin | Notes |
| --- | --- | --- | --- |
| Terminal UI | `ratatui` | `=0.30.2` | crossterm backend, plus `unstable-rendered-line-info` for `Paragraph::line_count` — the transcript's height must come from the wrapper that paints it, and `reflow::WordWrapper` is private, so that accessor is the only door. An upstream-unstable API is exactly why the pin stays exact |
| Terminal control + input | `crossterm` | `=0.29.0` | matches ratatui 0.30 |
| Compose editor | `ratatui-textarea` | `=0.9.2` | multiline input behind BOTH drafts (the `Ctrl-R` quick reply and the `Ctrl-N` background draft); isolated in `src/tui/compose.rs`; default features only (`search`/`regex` NOT enabled) |
| Highlight matcher | `nucleo` | `=0.5.0` | isolated in `src/search.rs`; backs the highlight seam only — NOT the filter |
| Substring filter | `memchr` | `=2.8.2` | SIMD `memmem`; IS the per-keystroke filter, answering membership per atom (`src/search.rs`); already transitive via `nucleo`/`serde_json` |
| FS watch | `notify` | `=8.2.0` | recursive, over the store root |
| Debounce | `notify-debouncer-mini` | `=0.7.0` | ~200ms coalescing |
| JSONL parse | `serde_json` | `=1.0.150` | as `Value`, never typed structs |
| Timestamps | `time` | `=0.3.53` | RFC 3339 parse/format |
| Home dir | `dirs` | `=6.0.0` | resolves the default store root |
| Errors | `anyhow` | `=1.0.103` | propagation in the core + TUI |
| Parallel scan | `rayon` | `=1.12.0` | per-file parse in `SessionStore::load` |

All versions are pinned **exact** for reproducibility (see `Cargo.toml`
comments). `Cargo.lock` is committed. Toolchain: stable Rust (developed on
1.95).

## Module map

Data-core-first: the `store` layer's parsing core (`discover` / `parse` /
`group` / `label` / `lineage`) is framework-independent and fully unit-tested
before any TUI code runs on top of it. **`store::preview` is a deliberate exception**: it
returns styled ratatui `Text` because it *is* the rendering step (transcript →
markdown), colocated with the store since its output is cached per session and
fitted to the pane width.

So the invariant is not "`store` is framework-pure" — that would be false, and
this crate ships exactly one frontend, so decoupling the renderer would buy
nothing. The invariant that earns its keep is narrower: **a module whose job is
PARSING never owns a palette; only a module whose job is RENDERING reaches for
the render framework.** That is why `agents::classify` buckets the undocumented
`state`/`status` value set while `tui::view` maps that bucket to a `Color`.

| Module | File(s) | Responsibility |
| --- | --- | --- |
| `run` | `src/lib.rs` | Crate root + persistent-dashboard loop: parse args, load store, run the board, spawn `claude` on resume, recover a lost liveness race (`after_nonzero_resume`), reload, repeat. Hidden `--print-list` dump. `src/main.rs` and `src/bin/sb.rs` are thin shims that only call `run`. |
| `cli` | `src/cli.rs` | Argument parsing (`--all`/`-a`, `--project`/`-p`, `--help`/`-h`, hidden `--print-list`); resolves the canonical launch dir. The two scope flags are mutually exclusive in meaning but not in syntax — the LAST one on the command line wins (the plain single-pass assignment, so an alias carrying `-p` stays overridable by a trailing `-a`). `-a` carries a SECOND, orthogonal meaning that precedence does not touch: `Args::all_scope_enabled`, which is what puts the all scope on the `Ctrl-A` cycle and is the only route to it. |
| `store` | `src/store/mod.rs` | Data core entry: `Session` model + the `SessionStore` reload pipeline; sorts repo→branch→timestamp-desc. **STATEFUL**, and the one cache in the crate that is not derived state: it owns a store root and an in-memory `path -> (FileStamp, Session)` map, so `reload` re-parses only the files whose `(mtime, len)` moved and hands back a `Reload` naming which sessions it re-read. Discovery is never cached (see [the load pipeline](#the-load-pipeline-store)). `load{,_from}` remain as one-shot wrappers over an empty cache, for `--print-list` and tests. |
| `store::discover` | `src/store/discover.rs` | Store-root resolution + depth-pinned file enumeration (the subagent-exclusion rule). |
| `store::parse` | `src/store/parse.rs` | Fail-soft, streaming per-file JSONL scan → `ParsedFile`. |
| `store::group` | `src/store/group.rs` | `repo_of(cwd)` repo/branch grouping heuristic (worktree collapse). A PURE string function — it never asks git, which is what keeps the collapse a property of the path alone; the git-backed answer is `worktrees`' job. |
| `store::label` | `src/store/label.rs` | Label preference (summary → first real user prompt → session id). |
| `store::lineage` | `src/store/lineage.rs` | Background-fork lineage identity + folding: `lineage_key` (`(repo, branch, root_uuid)`), `head_of` (the newest member), `group_members` (partition any index set into conversations — the ONE grouping the header's counter and `tui::app::child_indices` share, with a rootless session a lineage of one), and `fold` — the single DISPLAY entry point, which reduces a display list to the visible indices plus a head→hidden-count map (it keeps its own keyed map because it must consult `expanded` BY key). Presentation-only: it hides indices, it cannot drop a session. Pure and framework-free. See [DOMAIN.md](DOMAIN.md#fork-lineage-storelineage) for the mechanism it models. |
| `store::preview` | `src/store/preview.rs` | Transcript → `RenderedPreview` (styled ratatui `Text` + clickable `LinkRegion`s), self-contained markdown pass. |
| `worktrees` | `src/worktrees.rs` | **Which project a directory belongs to**, answered two ways because `Scope::Project` needs both. (1) The launch project's LIVE worktree set — the **only** place `snapback` shells out to `git` (`git -C <launch_dir> worktree list --porcelain`, argv built by the pure `git_worktree_argv`). (2) `project_root`, the canonicalized repo root of any path, built on the store's pure `group::repo_root_of` — the arm that still answers for a REMOVED worktree, which git structurally cannot. `project_root` derives the prefix from the RAW path and canonicalizes only THAT; the reverse order silently breaks a symlinked prefix under a missing leaf, which is exactly the case it exists for. `project_root_name` is the one place the "git resolved no label" fallback name is written, shared by the group head and the header so a project cannot be called two things on one screen. Sits OUTSIDE `src/store/*` on purpose: the store's grouping core stays a pure string heuristic, so the git answer is launch context (like the launch dir) resolved here and handed to the TUI. A TOP-LEVEL module, not a `tui` child, so the dependency runs one way — `tui` → `worktrees` → `store::group` — with no cycle. Owns `resolve_dir`, the canonicalization every RUNTIME comparison goes through — a session `cwd`, a git-reported worktree root, and the launch dir once `App` holds it — so membership is always decided in one resolved form; it moved here from `tui::app` so the resolver cannot depend on the TUI. Not literally the only `canonicalize` call in the crate: `cli::launch_dir` canonicalizes the process cwd once at startup, before `App` exists, with the same fall-back-to-raw behavior. Fail-soft toward "could not resolve": a missing `git`, a non-repo dir, a non-zero exit, non-UTF-8 output, or unparseable text all yield an EMPTY `WorktreeSet` — a meaningful answer, not an error (see [DOMAIN.md](DOMAIN.md#user-facing-modes-tuiapp)). Output is captured into pipes with stdin null, so git can never write to the board's terminal. |
| `search` | `src/search.rs` | The **only** place `nucleo` or `memchr` is called: substring index, incremental re-filter, highlight seam. Per keystroke the filter answers MEMBERSHIP with `memchr::memmem` over prebuilt haystacks — no nucleo, no UTF-32 conversion, no ranking (`App::order_filtered` owns display order). Smart case is decided per ATOM, selecting the cased or lowercased haystack. nucleo backs `match_indices` (the highlight) alone. |
| `agents` | `src/agents.rs` | Agent detection via `claude agents --json`, read TWO ways through ONE fail-soft parser. `reported_agents` (`--all`, polled off-thread every `watch::AGENTS_REFRESH`, 5s, skipped while the board is idle — see [DOMAIN.md](DOMAIN.md#why-the-gate-does-not-read-the---all-map)) is the DISPLAY signal: `classify` buckets each qualifier into an `AgentActivity` that the preview banner, the list row's translated qualifier (both via `qualifier_copy`), list-badge color and pulse (which alternates that color, never the glyph) derive from. `live_agents` (bare, NO `--all`) is the HAND-OFF signal, taken one-shot at EVERY hand-off — the resume/attach gate and the `Ctrl-R`/`Ctrl-K` gates alike — and its records also carry the short job `id` that `claude attach`/`claude stop` take. Why the two readings must never be merged, and why an authoritative decision never reads the polled map, is [DOMAIN.md](DOMAIN.md#why-the-gate-does-not-read-the---all-map) — do not restate it here. Framework-free: it interprets the value set, while the color it maps to is the view's call. |
| `defined_agents` | `src/defined_agents.rs` | DEFINED-agent discovery for a new session: fail-soft frontmatter scan of `~/.claude/agents/*.md` + `<launch_dir>/.claude/agents/*.md`, deduped (project over user). Distinct from `agents` (live vs. defined). |
| `watch` | `src/watch.rs` | Debounced FS watcher + `EventLoop` that merges input/watcher/tick/agents onto one channel. The watcher filters each debounce batch through the shared `store::discover::is_session_path` predicate (`classify_watch_path` / `batch_needs_reload`) before emitting `SessionsChanged`, so an irrelevant write elsewhere in the store root never triggers a reload. The agents poller backs off to `AGENTS_REFRESH` (5s) and skips the `claude agents` shell-out once the board-activity stamp is older than `AGENTS_IDLE_AFTER` (60s) — see [DOMAIN.md](DOMAIN.md#reported-agents-srcagentsrs). |
| `resume` | `src/resume.rs` | Resume/fork/attach/new-session hand-off: re-read authoritative parts (or, for a new session, gate on the launch dir + optional `--agent <name>`), existence gate, spawn `claude`, return. Each `Ready` carries its own neutral non-zero hint (resume vs. new-session). |
| `config` | `src/config.rs` | The SINGLE place that reads the environment to resolve snapback-owned paths: `config_dir()` (`$SNAPBACK_CONFIG_DIR` else `~/.config/snapback` — deliberately non-XDG on macOS, so one greppable home on every OS) and `state_dir()` (`<config>/state`, where `hidden` persists). No other module reads the env for these locations; future config/cache/state resolvers land here. |
| `hidden` | `src/hidden.rs` | snapback's OWN persistent state (the ONLY thing it writes for itself): the soft-hidden session id set, in the `state/` subdir under the config dir (path resolved by `config`, file `hidden_sessions`), NOT the read-only store. Pure `parse_hidden`/`serialize_hidden` (newline-delimited, sorted for stable diffs) + fail-soft `load_hidden` / atomic `save_hidden` (temp file + rename). |
| `delete` | `src/delete.rs` | The gated store-mutation core: pure `can_delete` (the claude-side WRITER guard — refuse an OPEN INTERACTIVE session or a still-RUNNING background agent, allow the parked ones; expressed over `AgentActivity` directly and deliberately NOT over `agents::is_active`, which decides a badge pulse), pure `can_delete_target` (the FULL gate the confirm calls — `can_delete` composed with snapback's own in-flight quick reply, the THIRD writer claude's active list structurally cannot show, refused with `DELETE_SENDING_REFUSAL`), pure `status_for_delete` (one target reports its reason verbatim, a lineage reports the split, and an FS failure is never counted as a running skip), and `toggle_hidden` (flip membership, return the new state), plus the thin `remove` driver that unlinks a session's own `<id>.jsonl` AND its sibling `<id>/` dir — only ever that id's own dir, never another path. |
| `send` | `src/send.rs` | Non-UI core for the THREE board actions that run without a terminal teardown: the quick reply (`Ctrl-R`, a one-shot `claude -p -r <id> --output-format json` that appends in place), the interrupt (`Ctrl-K`, `claude stop <job-id>`), and the background-agent launch (`Enter` in the new-session `Ctrl-N` draft pane, `claude [--agent <name>] --bg <prompt>`). Pure, tested decisions — `build_send_argv` / `build_stop_argv` / `build_bg_launch_argv`, the authoritative `plan_send` re-read and its promptless sibling `plan_bg_launch` (a launch-dir existence gate; a session that does not exist yet has no file to be authoritative about), the `status_for_output` / `status_for_failed_send` / `status_for_stop` / `status_for_bg_launch` maps that report failures HONESTLY (claude's own stderr reason, never a false `sent`/`stopped`/`started`), and the two gate state machines over the same one-shot probe record: `reply_gate` (never interrupt live work) and `interrupt_gate` (its mirror with the opposite intent, so `working` is a valid target there). Both route by `agents::classify` and both treat an unstoppable record — held with no job id — as a refusal ahead of the bucket; the full routing tables are in [DOMAIN.md](DOMAIN.md#quick-reply--non-interactive-send-srcsendrs). `status_for_bg_launch` is the strictest of the status maps, and deliberately not a copy of `status_for_output`: `--bg` can fail SILENTLY (an unknown `--agent` exits **0**, warns on stderr, and starts the session without it), so a zero exit with a non-empty stderr is *started-but-warned*, never a clean start. Three impure twins — `spawn_send` (optional stop, then the send), `spawn_interrupt`, and `spawn_bg_launch` — each run on a detached thread (mirroring `resume::open_url`) and deliver one `AppEvent::SendFinished` / `AppEvent::InterruptFinished` / `AppEvent::BgLaunchFinished`. Each event carries the id needed to attribute it to the dispatch that sent it (`session_id` for send and interrupt, `launch_id` for background launch) plus a `success` flag so `tui::update` can expire confirmations after `STATUS_DWELL_TICKS` ticks while keeping failures and refusals sticky. The status maps support the status-line ownership rule: only outcomes and refusals reach `App::status`; in-flight facts render on the surface that owns them — a single `cooking…` placeholder inline in the preview for a quick reply, `starting in the background…` on the draft card for a `--bg` launch, and no visible label for an interrupt (the identity guard in `App::interrupting` still prevents stale completions from landing on a moved-on surface). Passes NO permission flags (inherits the user's settings). |
| `tui` | `src/tui/mod.rs` | Terminal setup/teardown (+ panic hook) and the draw/event `run` loop; also fires a confirmed quick-reply send (`Outcome::Send`), interrupt (`Outcome::Interrupt`), or background-agent launch (`Outcome::BgLaunch`) on a detached thread without tearing the board down. TWO terminal modes are snapback's own here — mouse capture and bracketed paste — and `reset_child_modes_and_reassert_board` is the ordered seam that clears a child's leftovers before re-asserting them. |
| `tui::app` | `src/tui/app.rs` | The `App` model — all TUI state, pure state transitions, no terminal I/O. Owns the scope, `all_scope_enabled` (the `-a` half that decides whether `Ctrl-A` is a two-state flip or the three-stop cycle — passed INTO `Scope::toggled` and `view::empty_list_message`, never read from inside them) and, with it, the CACHED `WorktreeSet` that `Scope::Project` scopes by plus the `worktree_probe` seam that (re-)resolves it — refreshed only in `App::new` and `apply_sessions` (see [the load pipeline](#the-load-pipeline-store)), never on a keystroke. Owns the TWO caches `recompute_scope` writes together and nothing else may: `scoped` (the row indices the scope admits) and `population` (what the header COUNTS, already GROUPED INTO LINEAGES — `Scope::Project` membership even in the default folder scope, the whole store only under `All`), read by `App::session_counts` (`count_lineages`), which counts all THREE header numbers in lineages and derives the hidden split per call so a hide never needs a re-resolve (see [DOMAIN.md](DOMAIN.md#user-facing-modes-tuiapp)). The lineage grouping itself is pure and lives there only for cost; `child_indices` and the counter share the one `lineage::group_members` partition, so an indented child row can never be counted as a conversation of its own. Owns the width-scoped preview cache, whose entry holds everything derived from one `(session, width)` render — styled text, link regions AND the transcript's wrapped row count — in ONE value, so no invalidation can drop half of it. Owns the fold state: `expanded` (a set of `LineageKey`s, EMPTY by default so every lineage starts folded) and the derived `hidden` head→count map, applied by `lineage::fold` as the last step of `recompute_filtered` so `filtered` holds only VISIBLE indices. Owns the compose SURFACE as two fields, not one: `compose` (the editor — what the keyboard does) and `draft: Option<NewSessionDraft>` (the new-session placeholder card — what the preview pane shows). Those three methods — `open_compose` / `close_compose` / `dispatch_draft` — are the only WRITERS, which is what keeps a refusal from clearing one and forgetting the other; the one state they can leave apart (a dispatched card with no editor) is bounded separately, by `launching_draft` and by the board session. See [DOMAIN.md](DOMAIN.md#background-agent-draft-pane-ctrl-n). |
| `tui::update` | `src/tui/update.rs` | Elm-style event→state dispatch: `key_to_action`, `handle_event`, mouse routing (wheel scroll, splitter drag, preview link click-to-open), terminal-paste routing (`handle_paste` over the pure `accept_paste` / `flatten_for_query`, walking the same six-owner precedence as the key arm — see [DOMAIN.md](DOMAIN.md#terminal-paste-routing-eventpaste)), the `Ctrl-X` leader chord (`chord_key`), ONE generic modal key machine (`modal_key` → `confirm_modal`) that serves the running-session choice, the new-session agent picker, and the hard-delete confirm (`confirm_delete`) alike — plus the picker's SECOND verb, `Ctrl-O` → `launch_pick_interactively`, gated twice so it stays the picker's alone (`modal_key` binds it on the `List` layout only, and the handler acts only on a `ModalAction::New` choice). `Enter` on a `New` choice opens the background draft pane instead of launching, so BOTH routes out of the picker cost exactly one key and `Ctrl-O` names the same verb there as it does in the draft, the `Ctrl-R` quick-reply gate (`reply` classifies a one-shot liveness probe via `send::reply_gate`, then opens compose, opens the `App::pending_stop` "stop the waiting agent?" confirmation, or refuses), and the `Ctrl-K` interrupt gate (`interrupt` classifies the SAME probe via `send::interrupt_gate`, then dispatches the stop straight away, opens the `App::pending_interrupt` confirmation, or refuses). `AppEvent::SendFinished` re-anchors the previewed transcript to the newest turn, and `AppEvent::BgLaunchFinished` closes the draft card whose `launch_id` it names. `handle_event` also owns ONE teardown seam over every route: an `Outcome` that `ends_board_session` (`Quit`, any `Resume`) closes the compose surface, since neither the editor nor an in-flight card can outlive the event channel it reports on. `confirm_delete` takes a SET of ids — the selected session, or its whole fork lineage — takes claude's active list ONCE for all of them (`App::live_agents_now`, never one probe per member: that would be N blocking shell-outs on the render loop), guards each member with `can_delete_target` (the probe verdict AND snapback's own in-flight reply), and reloads the board once at the end if anything was actually removed. |
| `tui::compose` | `src/tui/compose.rs` | The compose modal: owns the `ratatui_textarea` dependency (the only place it is referenced, like `search` owns nucleo). ONE editor and ONE key router serve TWO drafts, forked by `ComposeTarget` rather than by parallel state — a `Reply` to an existing session (`Ctrl-R`) and a `NewBackgroundAgent` draft (`Ctrl-N`, via `Enter` on the agent picker or straight away when no agents are defined). Opening the background draft also installs the pane-level `App::draft` card through `App::open_compose`, so the preview stops showing an unrelated session; the reply passes `None` there, because it previews the real session it addresses. Pure `compose_key_to_action` (Enter=submit, Ctrl-J/Alt+Enter=newline, Ctrl-O=run interactively, Esc=cancel, else forward to the editor) + the `handle_compose_key` driver that edits the buffer or resolves a submit into `Outcome::Send` (reply), `Outcome::BgLaunch` (background), or `Outcome::Resume` (the `Ctrl-O` escape hatch, which is INERT on a reply). `insert_paste` is the second, non-key entry point — a terminal paste goes in through `TextArea::insert_str` as TEXT and returns no `Outcome`, so a pasted newline can never reach the `Enter`=submit arm. The auto-growing box's HEIGHT is this module's answer too, not the view's: `ComposeState::screen_rows` PROBES the widget for the draft's wrapped row count (the pinned `=0.9.2` publishes none), so the editor's own word wrap is the single source of truth and no second wrap model in the renderer can drift from it. |
| `tui::view` | `src/tui/view.rs` | Rendering: two-pane grouped list + preview, header/search/help lines (the help line renders only `App::status` outcomes/refusals, with sticky failures and transient confirmations that dwell for `STATUS_DWELL_TICKS` ticks), the single generic modal overlay (`render_modal`, `Row` button strip or `List` picker), the two mutually-exclusive stop confirmations (`render_stop_confirm` for the `Ctrl-R` "stop the waiting agent?" prompt, `render_interrupt_confirm` for the `Ctrl-K` "stop this agent?" one, worded without any mention of a reply), and the which-key chord hint that takes over the help line while a `Ctrl-X` chord is pending. BOTH sides of the header's counter come from `App::session_counts` — never a local `filtered.len()` over that call's denominator, which mixed a post-fold row count with a session-FILE count — and it appends a `· N hidden` segment only for a non-zero count; every header segment is joined by the one `HEADER_SEPARATOR`. The header's right-aligned version indicator branches on `cfg!(debug_assertions)`: release builds show `v<crate-version>`, dev builds `dev+<git-hash>[-dirty]` (pure `format_version_label`). Owns the transcript's HEIGHT (`wrapped_text_rows`, which asks `Paragraph::line_count` — the same `WordWrapper` that paints the pane — so the bottom anchor and the scrollbar can reach the newest turn; cached per session+width in `App`, with only a draft card or an in-flight reply tail measured per frame, the latter rendering a single `cooking…` placeholder — interval-scoped in-flight facts render on the surface that owns them, so `App::status` carries only outcomes/refusals, and the interrupt's identity guard lives in `App::interrupting` with no visible label) and, separately, the APPROXIMATE wrap-mapping (`wrapped_line_height`/`link_at`) that hit-tests a click to a preview link, the pure `preview_split` that carves a REPORTED session's pinned status-banner row off the preview pane (the transcript rect `update` must hit-test against — keyed on having a banner, never on liveness, since a `done` agent has one but is not live), and the badge's palette (`badge_color`, mapping an `AgentActivity` to a named ANSI color). Draws a folded head's `(+N)` and indents an expanded lineage's children, with the pure `fit_label` reserving the marker's columns BEFORE the label's so a narrow pane clips the label instead of the marker. A child's turn count (`child_msgs` / `fit_child_msgs`) is the mirror of that rule: it is drawn WHOLE or dropped entirely, never clipped, since a truncated count reads back as a plausible wrong number rather than a short one. While composing, the pure `compose_uses_bottom_bar` / `preview_compose_split` decide whether the compose zone docks in the bottom of the preview pane or falls back to a full-width bottom bar on a short terminal — both placements read their rect from the pure `preview_inner`, the ONE place the pane's border inset is applied, and their height from the editor (`ComposeState::screen_rows`) rather than from a wrap model of their own; `render_compose_zone` draws the bordered `TextArea`, titled by the pure `compose_title` and hinted by the pure `compose_hint` — both branching on `ComposeTarget`, so a reply names its session while a background draft names its agent and offers `Ctrl-O run interactively` (worded to avoid promising a review the CLI cannot give). While a quick-reply send is in flight, `sending_tail` appends the optimistic echo turns (`store::preview::pending_reply_turns`) to the previewed transcript and follows the bottom, and `preview_banner` returns `None` so the inline turns replace the pinned banner without desyncing the hit-test. While a NEW-SESSION draft is open, the pure `draft_card` replaces the transcript entirely with a near-empty placeholder (agent, launch dir, key hints — or a spinner line once the launch is dispatched), keyed on `App::draft` rather than on the compose target, and `preview_banner` returns `None` for the same hit-test reason. |
| `build` | `build.rs` | Build script (compile-time, not a runtime module): fail-soft `git rev-parse`/`status` into `SNAPBACK_GIT_HASH`/`SNAPBACK_GIT_DIRTY` env vars for the dev version indicator; degrades to `unknown`/`0` outside a repo. |
| *(packaging)* | `npm/cli.js` | **Not part of the program** — an install-time wrapper, published to npm as `snapback-tui`, that hands out the prebuilt binaries so installing needs no Rust toolchain. `install` copies the platform's binary onto `PATH` under both names and exits; node is gone from that point on. Bare `npx snapback-tui` also spawns the TUI (stdio inherited, SIGINT/SIGTSTP left to the child so the TUI's own terminal restore is not pre-empted), but that path is a convenience, not the blessed one. See [OPERATIONS.md](OPERATIONS.md#the-npm-package). |

## Runtime architecture

### The persistent-dashboard loop (`lib::run`)

`run` owns the `App` across the whole session and calls `tui::run(&mut app)` in
a loop:

- `Outcome::Quit` → break.
- `Outcome::Resume(ready)` → `tui::run` has already torn the terminal down;
  `resume::launch` spawns `claude` as a **child** that inherits the TTY, waits
  for it, then the store is reloaded and the loop calls `run` again (which
  re-initializes the terminal). Quitting the resumed `claude` drops the user
  back onto the board — `snapback` never replaces its own process image.

A **non-zero** child exit routes through `after_nonzero_resume`. On the
plain-resume path only (`Ready::race_probe_id` is `Some`; every other hand-off is
structurally excluded), it re-probes claude: if the session is live NOW, the
resume lost the liveness race, so the board says so and opens the Attach/Fork
overlay, which persists on the `App` into the next `tui::run`. Otherwise the
plan's neutral hint stands — the board never claims a session is running on
anything but the probe's word, and it never parses claude's error text (stdout
and stderr belong to the child; see the terminal-safety seams).

### The elm-style board (`tui`)

`tui::run` → `run_inner` initializes the terminal, builds an `EventLoop`, and
loops: `terminal.draw(view::render)` then `events.recv()` →
`update::handle_event`. `App` is pure state; `view` reads it (and writes back
layout-derived values like pane rects, scroll offsets, and viewport height so
mouse hit-testing and paging work without the model knowing the layout). A
left-click in the preview is mapped screen→content by `view::link_at` — over
`wrapped_line_height`, an APPROXIMATE character-packing model kept only here, since
the exact wrapper answers a total and never the per-line map a hit-test needs —
against the cached `LinkRegion`s, and a hit is opened off the render loop via
`resume::open_url`.

### Event sources (`watch::EventLoop`)

Four producers merged onto one `mpsc` channel of `AppEvent`:

1. **Input** — a crossterm reader thread (`AppEvent::Input`) that polls with a
   timeout so it can observe a shutdown flag; joined on `EventLoop` drop so the
   reader releases stdin **before** `claude` is spawned onto the same fd. It
   forwards every crossterm `Event` variant verbatim, so what `tui::update`
   routes is decided entirely there — keys, mouse, and (since `init_terminal`
   enables bracketed paste) `Event::Paste`, the whole clipboard drop in one event
   rather than a stream of keystrokes.
2. **Watcher** — a recursive `notify` watch over the store root, debounced
   ~200ms; an entire settle batch coalesces to one `AppEvent::SessionsChanged`
   — but only once the batch is filtered through `classify_watch_path`
   (`watch::batch_needs_reload`), the same depth-2 `.jsonl` predicate
   discovery uses. A batch whose paths are all provably outside that shape
   (too deep, or a proven non-session file/dir) is dropped before the store
   ever reloads; anything uncertain (missing metadata, an ambiguous depth-1
   directory) falls through to reload rather than risk missing a real change.
3. **Tick** — a ~250ms `AppEvent::Tick` that drives redraw/autorefresh
   visibility (does nothing costly) and advances `App::tick`, the board's only
   clock — `view::blink_visible` phases the live-badge pulse (a color swap, via
   `view::pulse_color`) and the search cursor off it. It NEVER touches the
   board-activity stamp the agents poller reads (below) — a `Tick` alone must
   never keep the board "active" forever.
4. **Agents poller** — an off-UI-thread `claude agents --json --all` poll every
   `watch::AGENTS_REFRESH` (5s), ONE call per cycle, delivering
   `AppEvent::ReportedAgents`, so the shell-out never blocks rendering. Once the
   board-activity stamp (touched only by input events and emitted
   `SessionsChanged`) is older than `watch::AGENTS_IDLE_AFTER` (60s), the poller
   skips the shell-out but keeps sleeping on the same interval, so the first
   activity after idle resumes polling within one cycle. Like the reader it
   observes `EventLoop`'s shutdown flag once per turn — at the TOP, ahead of the
   idle gate — so it is bounded to the board session even while idle, when it
   sends nothing that could notice a dropped receiver. Unlike the reader it is
   NOT joined, so teardown never waits on a `claude` child: it outlives the
   request by up to one cycle plus whatever shell-out that turn had already
   begun. It feeds badges/banner only; the resume gate's liveness probe is a
   separate one-shot at hand-off and is NOT an event source
   (see [PATTERNS.md](PATTERNS.md#6-off-ui-thread-for-anything-that-can-block)).

Three more NON-recurring producers join on demand, all via `EventLoop::sender` and
all one-shot per keypress rather than pollers, so the render loop never blocks on
the child:

- a confirmed quick-reply send (`Ctrl-R`) fires `send::spawn_send` on a detached
  thread that runs `claude -p -r <id>` to completion and delivers exactly ONE
  `AppEvent::SendFinished`;
- a confirmed interrupt (`Ctrl-K`) fires `send::spawn_interrupt` the same way for
  `claude stop <job-id>`, delivering exactly ONE `AppEvent::InterruptFinished`;
- a confirmed background-agent launch (`Enter` in the new-session draft pane) fires
  `send::spawn_bg_launch` for `claude [--agent <name>] --bg <prompt>`, delivering
  exactly ONE `AppEvent::BgLaunchFinished`. It is keyed by no SESSION — a brand-new
  agent has no `sessionId` yet, and the agent itself arrives through the watcher
  reload — but it IS keyed by its own dispatch: `App::dispatch_draft` mints a
  board-local `launch_id`, the `BgLaunchRequest` carries it out and the event
  carries it back, and `App::launching_draft` is what decides whether the card on
  screen is still that launch's to close. That single event is what ends the card
  the dispatch left standing, so the in-flight placeholder needs no clock of its
  own.

All three are **emitted** exactly once, spawn failures included — but emitted is
not delivered, and the difference is bounded by the board session. `run_inner`
builds a NEW `EventLoop` per board session and drops the old receiver at every
hand-off, while `lib::run` re-enters the board on the SAME `App`; a child still
running across that seam reports into a channel nobody reads. Nothing keyed to a
transcript minds (the watcher reload covers it), but the draft card is UI state
waiting on its event, so it cannot be left to one that may never arrive:
`update::handle_event` tears the compose surface down on any outcome that
[ends the board session](#the-persistent-dashboard-loop-librun) — `Quit` and every
`Resume` — which bounds the card to the board that dispatched it.

### The load pipeline (`store`)

`discover` (depth-pinned, subagent-excluding) → **reuse or** `parse` (fail-soft,
per file in parallel via rayon) → derive (`label`, `repo`, timestamp,
`root_uuid`, `msg_count`, `content_index`) → sort repo↑ / branch↑ / timestamp↓.
Every correctness constraint lives here and is covered by unit tests. See
[DOMAIN.md](DOMAIN.md) for the data model and
[the incremental reload](DOMAIN.md#incremental-reload-storesessionstore) for what
the reuse step may and may not decide.

**Discovery runs in full on every reload; only PARSING is incremental.** That
split is the whole safety argument: the cache answers what a discovered file
CONTAINS, so its worst failure is a briefly stale row, and it is structurally
incapable of the failure that would matter — a session on disk that never
reaches the board. The new cache is rebuilt from the discovered set rather than
edited, so a deleted file's entry leaves with it.

`root_uuid` and `msg_count` both fall out of the SAME streaming pass — no second
read, no walk, no index — which is why the lineage needs no cache of its own and
why the turn count costs nothing to keep. `msg_count` is counted OUTSIDE the
`CONTENT_INDEX_CAP` guard on purpose: inside it, the count would stop with the
buffer (see [DOMAIN.md](DOMAIN.md#turn-count-storeparse)). `store::lineage` then
runs **above** this pipeline, at display time (`App::recompute_filtered`), never
during the load: folding is a view of the store, not part of it.

Every reload lands in ONE funnel, `App::apply_reload`, and that is also where
the launch project's worktree set is **re-resolved** (`worktrees`, above) before
`recompute_scope` runs — so `Scope::Project` picks up a worktree created mid-run
without a restart, and a reload is scoped by the current set rather than one
reload behind it. The funnel needs no wiring at its callers: the
`AppEvent::SessionsChanged` watcher reload, the post-resume reload in `lib::run`,
the post-delete reload and the `Ctrl-X r` forced rescan all already go through it
(the three `tui` ones via `update::reload_board`), so a future reload path
inherits the same behavior. `apply_sessions` is the same funnel entered from a
session list that did not come through the store cache — it states "assume every
row moved" — and is what tests with synthetic sessions call. The set is resolved
at exactly two moments — construction and reload — and NEVER in
`recompute_scope` / `toggle_scope`, which run on a keystroke.

The `Reload` the funnel receives also names the sessions the store actually
re-read, and that is what lets the derived caches survive a reload: the preview
cache evicts only those entries (plus any whose session left the board) and
`SearchIndex::refresh` reuses the rest on its own fingerprint. Clearing either
wholesale would re-render every transcript the board touches at the watcher's
cadence and spend the saved parse one layer up.

The single `SessionStore` is owned by `lib::run` and threaded into `tui::run` →
`update::handle_event`, replacing the store root that used to travel there: the
cache is keyed by paths under one root, so root and cache are one value and
cannot drift onto different trees. Threading it (rather than rebuilding one per
reload) is also what makes the launch load warm the cache the board then reloads
against.

### Terminal safety seams

`tui::init_terminal` enters the alt screen + raw mode, wraps ratatui's panic
hook to also disable mouse capture **and bracketed paste**, then enables those
two modes. `restore_terminal` is idempotent, disables both, and runs on **every**
exit — clean quit, error, resume hand-off, and panic — so the user's shell always
gets a clean terminal back. This is what makes the resume round trip and
re-initialization safe.

Bracketed paste (`CSI ?2004h`) is enabled for a specific reason: it makes
crossterm deliver a clipboard drop as one `Event::Paste` instead of a keystroke
stream in which the first embedded newline reads as `Enter` — which sent a quick
reply's first line, closed compose, typed the rest into the search query, and let
a later newline hit the board's resume binding. Enabling it is therefore what
`tui::update` routes against; the ownership obligations that come with turning a
mode on are the TERMINAL SAFETY rule in [AGENTS.md](../../AGENTS.md).

The **return** leg is defended symmetrically: `run_inner` calls `hard_reset`
before the first draw. `init_terminal` re-enters the alt screen + raw + mouse on
every board (re)entry, but cannot recover a terminal a returning `claude` child
left dirty: its `EnterAlternateScreen` is a no-op when the emulator already
believes it is on the alt screen, and the diff renderer only repaints cells that
differ from its freshly-built buffer. `hard_reset` is therefore a deterministic
full re-init that asserts ONE complete return-to-known-state rather than clearing
one mode per reported bug — it (1) recovers the terminal's escape parser FIRST,
before any other escape, with a write-only `CAN` (`0x18`) + `ST` (`ESC \`) + SGR
reset (`CSI 0m`): a child that exited MID control-string (a dangling DCS/OSC/CSI
with no terminator) leaves the parser swallowing input, so were the re-init
escapes to run ahead of the recovery they would be eaten as string content and
every downstream SGR code would render as literal text (the reported leaked
`[39m` cascading over the board); (2) returns keyboard/cursor/minor-mode state to
a known-good baseline — pops the kitty keyboard progressive-enhancement stack
(`CSI < 1 u`) AND absolute-disables its flags (`CSI = 0 u`), resets the cursor
shape (`CSI 0 SP q`) and shows it (`CSI ?25h`), soft-resets remaining minor DEC
modes (`DECSTR`, `CSI ! p`), and forces normal cursor keys (`DECCKM`-off,
`CSI ? 1 l`); (3) confirms raw mode; (4) disables input modes the child may have
leaked (bracketed paste, focus reporting); (5) round-trips
`LeaveAlternateScreen`→`EnterAlternateScreen` to force a *fresh* alt buffer,
re-arms mouse capture **and bracketed paste**, clears the visible screen with
`Clear(ClearType::All)` (`CSI 2J`), and purges the native scrollback with
`Clear(ClearType::Purge)` (`CSI 3J`). Every escape it emits is **write-only** —
the seam issues NO cursor-position (DSR `CSI 6n`) query.

Steps (4) and (5) are one helper (`reset_child_modes_and_reassert_board`) because
the ORDER between them is a contract, not an accident: bracketed paste is turned
OFF by (4), clearing whatever level the child left, and back ON by (5), the
board's own enable. Reversed, the board returns from every resume with paste
disabled and the keystroke-stream bug silently comes back. Composing them in one
`Write`-generic function is what lets a unit test pin `?2004l` before `?2004h`
against real production code rather than against an order the test chose.

The kitty keyboard reset in step (2) is the priority fix for the reported
*still-unstable* `Ctrl-Z`: a `claude` child that pushed progressive enhancement
and exited without popping leaves an enhancement level active that re-encodes
ordinary keys (release events, alternate reports) and scrambles the board's
input — a mode none of the other seams ever cleared, so it persisted across the
round trip. A single pop clears only ONE stack level and the depth is UNKNOWABLE
without a `CSI ? u` query (a DSR-class query forbidden on this write-only path),
so the seam pairs one pop with an absolute `CSI = 0 u` off — the robust
write-only way to reach "no enhancement" regardless of residual depth; both are
harmless no-ops on terminals lacking the protocol. Step (2) runs BEFORE step (5)
deliberately: `DECSTR` is a soft reset over DEC ANSI modes only
(DECCKM/DECOM/DECAWM/DECTCEM/SGR/…) and does NOT touch the xterm private modes
`?1049` (alt screen) or `?100x` (mouse), so re-asserting those afterwards still
wins — verified in a tmux repro where the board renders and mouse scroll still
work after a dirty `Ctrl-Z` return.

The write-only discipline is deliberate: in ratatui 0.30
`Terminal::clear()` first calls `get_cursor_position()`, which emits `CSI 6n` and
blocks reading the reply; on a dirty `Ctrl-Z` hand-back the reply is lost,
crossterm times out (~2s), and the error crashed snapback with "The cursor
position could not be read within a normal duration". A write-only `2J` is
sufficient because `init_terminal` hands `run_inner` a brand-new terminal with
empty buffers on every board (re)entry, so the first `draw` after the physical
`2J` repaints every cell — no back-buffer reset needed. The `?1049` round-trip
and the `3J` purge are what heal the reported `Ctrl-Z` dirty-exit corruption — a
bare visible-screen `2J` alone left stale cells and native scrollback showing
through — while the `CAN`+`ST`+SGR-reset prefix heals the related `Ctrl-Z`
variant where the child exits mid control-string and the board's own escapes
render as literal text. The recovery bytes are harmless no-ops on a clean
terminal, so they are prepended unconditionally on every board (re)entry. `CAN`
and `ST` have no crossterm typed command, so this seam is the one narrow spot the
terminal-management layer writes raw control bytes; the "never embed ANSI
escapes" styling rule governs `view.rs`/`preview.rs` presentation, not terminal
parser recovery here.
