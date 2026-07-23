# Architecture

Static structure: what the pieces are and how they connect. For the rules on
*building new things*, see [PATTERNS.md](PATTERNS.md); for the Claude Code
session model, see [DOMAIN.md](DOMAIN.md).

## Identity

`snapback` (short alias `sb`) is a single self-contained Rust binary: a
[ratatui](https://ratatui.rs) TUI that browses, searches, and resumes **Claude
Code** sessions stored as JSONL under `~/.claude/projects/`. It exists because
the built-in `/resume` picker is per-project with no cross-folder view and no
content search. It depends on nothing outside its own `Cargo.toml`.

That last sentence still holds despite `npm/` existing. The npm package is a
**distribution channel, not a dependency**: it carries prebuilt binaries and
copies one onto your `PATH`. The installed program is the same self-contained
Rust binary, with no node anywhere near it. Nothing under `src/` may ever import,
shell out to, or assume node.

## Stack

| Concern | Crate | Pin | Notes |
| --- | --- | --- | --- |
| Terminal UI | `ratatui` | `=0.30.2` | crossterm backend |
| Terminal control + input | `crossterm` | `=0.29.0` | matches ratatui 0.30 |
| Compose editor | `ratatui-textarea` | `=0.9.2` | multiline input for the quick-reply compose zone; isolated in `src/tui/compose.rs`; default features only (`search`/`regex` NOT enabled) |
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
| `cli` | `src/cli.rs` | Argument parsing (`--all`/`-a`, `--help`/`-h`, hidden `--print-list`); resolves the canonical launch dir. |
| `store` | `src/store/mod.rs` | Data core entry: `Session` model + `SessionStore::load{,_from}` pipeline; sorts repo→branch→timestamp-desc. |
| `store::discover` | `src/store/discover.rs` | Store-root resolution + depth-pinned file enumeration (the subagent-exclusion rule). |
| `store::parse` | `src/store/parse.rs` | Fail-soft, streaming per-file JSONL scan → `ParsedFile`. |
| `store::group` | `src/store/group.rs` | `repo_of(cwd)` repo/branch grouping heuristic (worktree collapse). |
| `store::label` | `src/store/label.rs` | Label preference (summary → first real user prompt → session id). |
| `store::lineage` | `src/store/lineage.rs` | Background-fork lineage identity + folding: `lineage_key` (`(repo, branch, root_uuid)`), `head_of` (the newest member), and `fold` — the single entry point, which reduces a display list to the visible indices plus a head→hidden-count map. Presentation-only: it hides indices, it cannot drop a session. Pure and framework-free. See [DOMAIN.md](DOMAIN.md#fork-lineage-storelineage) for the mechanism it models. |
| `store::preview` | `src/store/preview.rs` | Transcript → `RenderedPreview` (styled ratatui `Text` + clickable `LinkRegion`s), self-contained markdown pass. |
| `search` | `src/search.rs` | The **only** place `nucleo` or `memchr` is called: substring index, incremental re-filter, highlight seam. Per keystroke the filter answers MEMBERSHIP with `memchr::memmem` over prebuilt haystacks — no nucleo, no UTF-32 conversion, no ranking (`App::order_filtered` owns display order). Smart case is decided per ATOM, selecting the cased or lowercased haystack. nucleo backs `match_indices` (the highlight) alone. |
| `agents` | `src/agents.rs` | Agent detection via `claude agents --json`, read TWO ways through ONE fail-soft parser. `reported_agents` (`--all`, polled ~1s off-thread) is the DISPLAY signal: `classify` buckets each qualifier into an `AgentActivity` that the preview banner, the list row's translated qualifier (both via `qualifier_copy`), list-badge color and pulse (which alternates that color, never the glyph) derive from. `live_agents` (bare, NO `--all`, one-shot at EVERY hand-off) is the HAND-OFF signal: the bare command IS claude's active list, so MEMBERSHIP is liveness — no inference — and the same records carry the Attach job `id`, so one authoritative read answers both. The split exists because `--all`'s `done` means "the agent reported completion", not "claude will permit `-r`"; inferring liveness from that polled snapshot was a TOCTOU race, and reading an attach id from it was the same bug one layer down. Framework-free: it interprets the value set, while the color it maps to is the view's call. |
| `defined_agents` | `src/defined_agents.rs` | DEFINED-agent discovery for a new session: fail-soft frontmatter scan of `~/.claude/agents/*.md` + `<launch_dir>/.claude/agents/*.md`, deduped (project over user). Distinct from `agents` (live vs. defined). |
| `watch` | `src/watch.rs` | Debounced FS watcher + `EventLoop` that merges input/watcher/tick/agents onto one channel. |
| `resume` | `src/resume.rs` | Resume/fork/attach/new-session hand-off: re-read authoritative parts (or, for a new session, gate on the launch dir + optional `--agent <name>`), existence gate, spawn `claude`, return. Each `Ready` carries its own neutral non-zero hint (resume vs. new-session). |
| `config` | `src/config.rs` | The SINGLE place that reads the environment to resolve snapback-owned paths: `config_dir()` (`$SNAPBACK_CONFIG_DIR` else `~/.config/snapback` — deliberately non-XDG on macOS, so one greppable home on every OS) and `state_dir()` (`<config>/state`, where `hidden` persists). No other module reads the env for these locations; future config/cache/state resolvers land here. |
| `hidden` | `src/hidden.rs` | snapback's OWN persistent state (the ONLY thing it writes for itself): the soft-hidden session id set, in the `state/` subdir under the config dir (path resolved by `config`, file `hidden_sessions`), NOT the read-only store. Pure `parse_hidden`/`serialize_hidden` (newline-delimited, sorted for stable diffs) + fail-soft `load_hidden` / atomic `save_hidden` (temp file + rename). |
| `delete` | `src/delete.rs` | The gated store-mutation core: pure `can_delete` (refuse a live session) and `toggle_hidden` (flip membership, return the new state), plus the thin `remove` driver that unlinks a session's own `<id>.jsonl` AND its sibling `<id>/` dir — only ever that id's own dir, never another path. |
| `send` | `src/send.rs` | Non-UI quick-reply core (`Ctrl-R`): the one-shot `claude -p -r <id> --output-format json` write that appends a reply in place WITHOUT a terminal teardown. Pure, tested decisions — `build_send_argv`, the `reply_gate` state machine (not held → reply; `done` → stop then reply; `needs input` → confirm then stop then reply; `working`/`idle` → refuse), `build_stop_argv` (`claude stop <job-id>` deregisters a held agent so `-p -r` is accepted), the authoritative `plan_send` re-read, and the `status_for_output`/`status_for_failed_send` map that reports failures HONESTLY (claude's own stderr reason, never a false `sent`). One impure `spawn_send` runs the (optional stop +) send on a detached thread (mirroring `resume::open_url`) and delivers one `AppEvent::SendFinished`. Passes NO permission flags (inherits the user's settings). |
| `tui` | `src/tui/mod.rs` | Terminal setup/teardown (+ panic hook) and the draw/event `run` loop; also fires a confirmed quick-reply send (`Outcome::Send`) on a detached thread without tearing the board down. |
| `tui::app` | `src/tui/app.rs` | The `App` model — all TUI state, pure state transitions, no terminal I/O. Owns the fold state: `expanded` (a set of `LineageKey`s, EMPTY by default so every lineage starts folded) and the derived `hidden` head→count map, applied by `lineage::fold` as the last step of `recompute_filtered` so `filtered` holds only VISIBLE indices. |
| `tui::update` | `src/tui/update.rs` | Elm-style event→state dispatch: `key_to_action`, `handle_event`, mouse routing (wheel scroll, splitter drag, preview link click-to-open), the `Ctrl-X` leader chord (`chord_key`), ONE generic modal key machine (`modal_key` → `confirm_modal`) that serves the running-session choice, the new-session agent picker, and the hard-delete confirm (`confirm_delete`) alike, and the `Ctrl-R` quick-reply gate (`reply` classifies a one-shot liveness probe via `send::reply_gate`, then opens compose, opens the `App::pending_stop` "stop the waiting agent?" confirmation, or refuses). `AppEvent::SendFinished` re-anchors the previewed transcript to the newest turn. |
| `tui::compose` | `src/tui/compose.rs` | The quick-reply compose modal: owns the `ratatui_textarea` dependency (the only place it is referenced, like `search` owns nucleo). Pure `compose_key_to_action` (Enter=send, Ctrl-J/Alt+Enter=newline, Esc=cancel, else forward to the editor) + the `handle_compose_key` driver that edits the buffer or resolves a Send into an `Outcome::Send`. |
| `tui::view` | `src/tui/view.rs` | Rendering: two-pane grouped list + preview, header/search/help lines, the single generic modal overlay (`render_modal`, `Row` button strip or `List` picker), the "stop the waiting agent?" confirmation overlay (`render_stop_confirm`), and the which-key chord hint that takes over the help line while a `Ctrl-X` chord is pending. The header's right-aligned version indicator branches on `cfg!(debug_assertions)`: release builds show `v<crate-version>`, dev builds `dev+<git-hash>[-dirty]` (pure `format_version_label`). Owns the pure wrap-mapping (`wrapped_line_height`/`link_at`) that hit-tests a click to a preview link, the pure `preview_split` that carves a REPORTED session's pinned status-banner row off the preview pane (the transcript rect `update` must hit-test against — keyed on having a banner, never on liveness, since a `done` agent has one but is not live), and the badge's palette (`badge_color`, mapping an `AgentActivity` to a named ANSI color). Draws a folded head's `(+N)` and indents an expanded lineage's children, with the pure `fit_label` reserving the marker's columns BEFORE the label's so a narrow pane clips the label instead of the marker. A child's turn count (`child_msgs` / `fit_child_msgs`) is the mirror of that rule: it is drawn WHOLE or dropped entirely, never clipped, since a truncated count reads back as a plausible wrong number rather than a short one. While composing, the pure `compose_uses_bottom_bar` / `preview_compose_split` decide whether the compose zone docks in the bottom of the preview pane or falls back to a full-width bottom bar on a short terminal; `render_compose_zone` draws the bordered `TextArea`. While a quick-reply send is in flight, `sending_tail` appends the optimistic echo turns (`store::preview::pending_reply_turns`) to the previewed transcript and follows the bottom, and `preview_banner` returns `None` so the inline turns replace the pinned banner without desyncing the hit-test. |
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
left-click in the preview is mapped screen→content by `view::link_at` (reusing the
same `wrapped_line_height` wrap model as the scrollbar) against the cached
`LinkRegion`s, and a hit is opened off the render loop via `resume::open_url`.

### Event sources (`watch::EventLoop`)

Four producers merged onto one `mpsc` channel of `AppEvent`:

1. **Input** — a crossterm reader thread (`AppEvent::Input`) that polls with a
   timeout so it can observe a shutdown flag; joined on `EventLoop` drop so the
   reader releases stdin **before** `claude` is spawned onto the same fd.
2. **Watcher** — a recursive `notify` watch over the store root, debounced
   ~200ms; an entire settle batch coalesces to one `AppEvent::SessionsChanged`.
3. **Tick** — a ~250ms `AppEvent::Tick` that drives redraw/autorefresh
   visibility (does nothing costly) and advances `App::tick`, the board's only
   clock — `view::blink_visible` phases the live-badge pulse (a color swap, via
   `view::pulse_color`) and the search cursor off it.
4. **Agents poller** — an off-UI-thread `claude agents --json --all` poll (~1s),
   ONE call per cycle, delivering `AppEvent::ReportedAgents`, so the shell-out
   never blocks rendering. It feeds badges/banner only; the resume gate's
   liveness probe is a separate one-shot at hand-off and is NOT an event source
   (see [PATTERNS.md](PATTERNS.md#6-off-ui-thread-for-anything-that-can-block)).

A fifth, NON-recurring producer joins on demand: a confirmed quick-reply send
(`Ctrl-R`) fires `send::spawn_send` on a detached thread that runs `claude -p -r
<id>` to completion and delivers exactly ONE `AppEvent::SendFinished` on the same
channel (via `EventLoop::sender`). It is a one-shot per send, not a poller, so the
render loop never blocks on the multi-second child.

### The load pipeline (`store`)

`discover` (depth-pinned, subagent-excluding) → `parse` (fail-soft, per file in
parallel via rayon) → derive (`label`, `repo`, timestamp, `root_uuid`,
`msg_count`, `content_index`) → sort repo↑ / branch↑ / timestamp↓. Every
correctness constraint lives here and is covered by unit tests. See
[DOMAIN.md](DOMAIN.md) for the data model.

`root_uuid` and `msg_count` both fall out of the SAME streaming pass — no second
read, no walk, no index — which is why the lineage needs no cache of its own and
why the turn count costs nothing to keep. `msg_count` is counted OUTSIDE the
`CONTENT_INDEX_CAP` guard on purpose: inside it, the count would stop with the
buffer (see [DOMAIN.md](DOMAIN.md#turn-count-storeparse)). `store::lineage` then
runs **above** this pipeline, at display time (`App::recompute_filtered`), never
during the load: folding is a view of the store, not part of it.

### Terminal safety seams

`tui::init_terminal` enters the alt screen + raw mode, wraps ratatui's panic
hook to also disable mouse capture, then enables mouse capture.
`restore_terminal` is idempotent and runs on **every** exit — clean quit, error,
resume hand-off, and panic — so the user's shell always gets a clean terminal
back. This is what makes the resume round trip and re-initialization safe.

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
re-arms mouse capture, clears the visible screen with `Clear(ClearType::All)`
(`CSI 2J`), and purges the native scrollback with `Clear(ClearType::Purge)`
(`CSI 3J`). Every escape it emits is **write-only** — the seam issues NO
cursor-position (DSR `CSI 6n`) query.

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
