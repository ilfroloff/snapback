# Patterns: how to build new things

Active implementation rules that repeat across the codebase. For *what the
pieces are*, see [ARCHITECTURE.md](ARCHITECTURE.md); for the *session format*,
see [DOMAIN.md](DOMAIN.md). These are the conventions to match when editing.

## 1. Fail-soft over external input

The JSONL format is external and undocumented, so treat every read as hostile:

- Parse each line as `serde_json::Value` — **never** hard-typed
  `#[derive(Deserialize)]` structs. Schema drift must never be fatal.
- Skip an unparseable line, a non-object value, or an unreadable file; keep
  going. One bad line never aborts a file; one bad file never aborts the scan.
- The same discipline governs `claude agents --json` (`agents::parse_agents_json`):
  a missing binary, non-zero exit, non-JSON, or a non-array top level all
  collapse to an **empty set**, never a panic. There is exactly one place the
  wire shape is interpreted per source.

## 2. Authoritative-from-file

`cwd` and `sessionId` come from **inside** the file, never decoded from the
`<encoded-cwd>` folder name (the `/`→`-` encoding is lossy). At hand-off time
`resume::read_authoritative` re-reads them fresh (the on-disk file may have
changed since load) via the same `parse::parse_file`, so parsing lives in one
place. A file with no `cwd` is not a resumable session — refuse rather than
guess.

## 3. Pure core, thin impure drivers

Decision logic is pure and unit-tested; side effects sit in thin wrappers over
it. Follow this split when adding behavior:

- Pure, tested: `resume::plan` / `plan_from_parts` / `build_argv` /
  `status_for_exit`; `update::key_to_action` / `wheel_target`; every `App`
  state transition; `view`'s `wrapped_rows` / `clamp_preview_offset` /
  `centered_rect` / `highlight_runs`.
- Thin, impure: `resume::launch` (chdir + spawn + wait), the `watch` threads,
  `tui::run` (draw loop). Keep these small and delegate to tested helpers.

The terminal-up **refusal gate** is an instance of this: `resume::check` runs
the pure predicate while the UI is still drawn, so a refusal becomes a board
status with no teardown flash; only a confirmed `Ready` escalates to
`Outcome::Resume` and the impure `launch`.

## 4. Isolate volatile dependencies

All `nucleo` calls live in `src/search.rs` and nowhere else — the pin is exact
and the API is evolving, so an upgrade touches one module. The rest of the crate
sees only `SearchIndex`, `SearchMode`, and `filter`. Matching is **substring,
not fuzzy**: patterns are built with `AtomKind::Substring` in code (never from
user-typed atom syntax). The filter and the highlight seam score the **same**
`Pattern` so they can never disagree. When you touch search, preserve the
incrementality contract: `set_query` rebuilds only the small pattern per
keystroke; `refresh` rebuilds haystacks only for sessions whose fingerprint
changed.

## 5. Selection and scroll survive reloads

TUI state that must persist across an autorefresh reload is keyed by **stable
`session_id`**, never by list index (`App::selected` is an id; `App::scroll` is
preserved and only clamped). On reload, restore the selection by locating the id
in the new filtered list; if it vanished, clamp the previous position to the
nearest surviving row. Path canonicalization (the scope predicate) runs only on
reload / scope-toggle (`recompute_scope`), never per keystroke.

## 6. Off-UI-thread for anything that can block

The render loop must never block. A shell-out (`claude agents --json`), a FS
watch, and the input read all run on their own threads and deliver `AppEvent`s
onto the merged channel. Threads exit when the receiver drops (bounded to the
board session) and the input reader is **joined on `EventLoop` drop** so it
releases stdin before `claude` is spawned onto the same fd. New background work
follows the same pattern: own thread, `AppEvent` variant, self-terminating on
send failure.

## 7. Restrained, terminal-safe styling

The preview and list are styled with ratatui `Style` only — **never** embedded
ANSI escape sequences. Prefer `Modifier`s (BOLD/ITALIC/DIM/UNDERLINED) plus a
small palette of **named** ANSI `Color`s (they adapt to the user's terminal
theme). Do **not** hardcode RGB (it can vanish on a light background) and do not
syntax-highlight code (code is DIM). The markdown pass in `store::preview` is
hand-rolled and self-contained — no external markdown crate.

Ahead of the markdown pass, each message body runs through an **allowlist-driven
control-wrapper collapse** (`store::preview::collapse_control_wrappers`). Claude
Code injects a fixed set of paired pseudo-tags (`<command-name>`,
`<system-reminder>`, `<local-command-stdout>`, `<local-command-caveat>`,
`<task-notification>`, `<persisted-output>`, …); each collapses to a single dim
marker (a slash-command turn renders as `▷ /name args`, a `<local-command-caveat>`
renders as `[command caveat]`). Only names in the `CONTROL_WRAPPERS` allowlist
that have a matching close tag are touched — legitimate angle-bracket content
(open-only placeholders like `<session-id>`, generics like `<String>`,
comparisons like `x < y > z`) is left byte-for-byte literal, and a known opener
with no close fails soft to literal. The collapse is a pure `body -> Vec<Segment>`
function; the thin renderer routes each literal segment through the markdown pass
and each collapsed segment to its marker line.

## 8. Name every constant

No magic numbers. Tunables are named `const`s with a rationale comment near the
top of their module: `DEBOUNCE` / `TICK` / `AGENTS_REFRESH` (`watch`),
`LABEL_MAX` (`label`), `CONTENT_INDEX_CAP` (`parse`), `PREVIEW_LINES` /
`TABLE_MAX_WIDTH` (`preview`), `PREVIEW_WHEEL_STEP` / `LIST_WHEEL_STEP` (`app`).
Add new tunables the same way.

## 9. `#[allow(dead_code)]` is narrow and justified

`snapback` is a **binary** crate, so `pub` does not make an item reachable and
the `dead_code` lint fires on any public API the `main` runtime path does not
call — even when it is fully exercised by unit tests. Where that happens, attach
a **narrowly-scoped** `#[allow(dead_code)]` to the single item with a one-line
reason. **Never** use a crate- or module-wide blanket allow — the lint must stay
sharp everywhere else.

## 10. Keys, actions, outcomes

Input handling is a three-stage pipeline, all terminal-free and testable:

1. `key_to_action(key, query_empty)` → an `Action` (`j`/`k`/`q` navigate/quit
   only while the query is empty; arrows, Enter, Tab, and `Ctrl-*` always act so
   search never blocks navigation).
2. `apply_action` mutates the `App` and returns an `Outcome`
   (`Continue`/`Quit`/`Resume`).
3. Modal state (the running-session overlay) owns the keyboard via
   `pending_live` and its own `live_choice_key` state machine; a mouse wheel is
   handled **before** and **independent of** that gate.

Add a keybinding by extending the `Action` enum + `key_to_action` + `apply_action`
and covering it with a `key_to_action` unit test. Keep the doc-comment key table
in `update.rs`, the `USAGE`/`KEYS` block in `cli.rs`, and the help line in
`view.rs` in sync.

## Testing patterns

Tests are **inline** `#[cfg(test)] mod tests` at the bottom of each source file
(no separate integration crate). Conventions to match:

- **Fixture store**: `tests/fixtures/store/` holds representative JSONL — a
  normal session, a no-summary session, a malformed-line session, a worktree
  cwd, a sidecar (no `cwd`), and a nested subagent. Reach it via
  `env!("CARGO_MANIFEST_DIR")`. Add a fixture when you add a format edge case.
- **Synthetic models**: build `Session`/`LiveAgent` values directly in tests
  (see the `session(...)` helpers) rather than round-tripping through disk.
- **Isolated temp dirs**: watcher/app tests create a unique
  `snapback-<tag>-<pid>-<nanos>` dir under `std::env::temp_dir()` and never
  touch the real `~/.claude/projects`. Clean up with `remove_dir_all`.
- **Test the pure helper, not the impure driver**: exit handling is tested via
  `status_for_exit`, teardown via the `Write`-generic `disable_mouse`, argv via
  `build_argv` — no real `claude` process is ever spawned.
- **Assert structure, not styling**: preview tests flatten `Text` to plain
  strings to check markers, and separately assert `Style`/`Modifier` on specific
  spans.

Every new pure function gets a unit test in the same file.
