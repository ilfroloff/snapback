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

## Stack

| Concern | Crate | Pin | Notes |
| --- | --- | --- | --- |
| Terminal UI | `ratatui` | `=0.30.2` | crossterm backend |
| Terminal control + input | `crossterm` | `=0.29.0` | matches ratatui 0.30 |
| Substring matcher | `nucleo` | `=0.5.0` | isolated in `src/search.rs` |
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

Data-core-first: the framework-independent `store` layer is fully unit-tested
before any TUI code runs on top of it.

| Module | File(s) | Responsibility |
| --- | --- | --- |
| `main` | `src/main.rs` | Persistent-dashboard loop: parse args, load store, run the board, spawn `claude` on resume, reload, repeat. Hidden `--print-list` dump. |
| `cli` | `src/cli.rs` | Argument parsing (`--all`/`-a`, `--help`/`-h`, hidden `--print-list`); resolves the canonical launch dir. |
| `store` | `src/store/mod.rs` | Data core entry: `Session` model + `SessionStore::load{,_from}` pipeline; sorts repo→branch→timestamp-desc. |
| `store::discover` | `src/store/discover.rs` | Store-root resolution + depth-pinned file enumeration (the subagent-exclusion rule). |
| `store::parse` | `src/store/parse.rs` | Fail-soft, streaming per-file JSONL scan → `ParsedFile`. |
| `store::group` | `src/store/group.rs` | `repo_of(cwd)` repo/branch grouping heuristic (worktree collapse). |
| `store::label` | `src/store/label.rs` | Label preference (summary → first real user prompt → session id). |
| `store::preview` | `src/store/preview.rs` | Transcript → `RenderedPreview` (styled ratatui `Text` + clickable `LinkRegion`s), self-contained markdown pass. |
| `search` | `src/search.rs` | The **only** place `nucleo` is called: substring index, incremental re-filter, highlight seam. |
| `agents` | `src/agents.rs` | Live-agent detection via `claude agents --json` (fail-soft parse). |
| `watch` | `src/watch.rs` | Debounced FS watcher + `EventLoop` that merges input/watcher/tick/agents onto one channel. |
| `resume` | `src/resume.rs` | Resume/fork/attach hand-off: re-read authoritative parts, existence gate, spawn `claude`, return. |
| `tui` | `src/tui/mod.rs` | Terminal setup/teardown (+ panic hook) and the draw/event `run` loop. |
| `tui::app` | `src/tui/app.rs` | The `App` model — all TUI state, pure state transitions, no terminal I/O. |
| `tui::update` | `src/tui/update.rs` | Elm-style event→state dispatch: `key_to_action`, `handle_event`, mouse routing (wheel scroll, splitter drag, preview link click-to-open), overlay state machine. |
| `tui::view` | `src/tui/view.rs` | Rendering: two-pane grouped list + preview, header/search/help lines, running-session overlay. Owns the pure wrap-mapping (`wrapped_line_height`/`link_at`) that hit-tests a click to a preview link. |

## Runtime architecture

### The persistent-dashboard loop (`main`)

`main` owns the `App` across the whole session and calls `tui::run(&mut app)` in
a loop:

- `Outcome::Quit` → break.
- `Outcome::Resume(ready)` → `tui::run` has already torn the terminal down;
  `resume::launch` spawns `claude` as a **child** that inherits the TTY, waits
  for it, then the store is reloaded and the loop calls `run` again (which
  re-initializes the terminal). Quitting the resumed `claude` drops the user
  back onto the board — `snapback` never replaces its own process image.

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
   visibility (does nothing costly).
4. **Agents poller** — an off-UI-thread `claude agents --json` poll (~1s)
   delivering `AppEvent::LiveAgents`, so the shell-out never blocks rendering.

### The load pipeline (`store`)

`discover` (depth-pinned, subagent-excluding) → `parse` (fail-soft, per file in
parallel via rayon) → derive (`label`, `repo`, timestamp, `content_index`) →
sort repo↑ / branch↑ / timestamp↓. Every correctness constraint lives here and
is covered by unit tests. See [DOMAIN.md](DOMAIN.md) for the data model.

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
full re-init — it (1) recovers the terminal's escape parser FIRST, before any
other escape, with a write-only `CAN` (`0x18`) + `ST` (`ESC \`) + SGR reset
(`CSI 0m`): a child that exited MID control-string (a dangling DCS/OSC/CSI with
no terminator) leaves the parser swallowing input, so were the re-init escapes to
run ahead of the recovery they would be eaten as string content and every
downstream SGR code would render as literal text (the reported leaked `[39m`
cascading over the board); (2) confirms raw mode; (3) disables input modes the
child may have leaked (bracketed paste, focus reporting); (4) round-trips
`LeaveAlternateScreen`→`EnterAlternateScreen` to force a *fresh* alt buffer,
re-arms mouse capture, clears the visible screen with `Clear(ClearType::All)`
(`CSI 2J`), and purges the native scrollback with `Clear(ClearType::Purge)`
(`CSI 3J`). Every escape it emits is **write-only** — the seam issues NO
cursor-position (DSR `CSI 6n`) query. That is deliberate: in ratatui 0.30
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
