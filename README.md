# snapback - Claude Code session browser & resumer

A single, self-contained Rust [ratatui](https://ratatui.rs) TUI for browsing,
searching, and resuming **Claude Code** sessions across every project on this
machine. It installs as two binaries, `snapback` and its short alias `sb`, that
run the same program.

It exists because the built-in `/resume` picker is **per-project** and offers
**no cross-folder view and no content search**. `snapback` gives you:

1. All sessions in one list, grouped **repo -> branch/worktree** git-log-style:
   each folder head (repo + branch) is printed once as a section head, and the
   sessions under it read as one block. Every session row is a real, resumable
   session.
2. **Exact substring search over session name/label - and, on a toggle, over
   transcript content** (what was actually said/done).
3. **Live autorefresh:** the list updates in place as sessions are created,
   changed, or removed - no restart.
4. **Folder scoping:** by default only the sessions launched from the current
   directory are shown; one key (or `--all`) widens to every folder.
5. One keystroke to **resume** (or **fork**) the chosen session via `claude -r`,
   or to **start a new session** in the launch directory (`Ctrl-N`) — optionally
   binding it to one of your Claude Code **agents** via a quick picker.

This is a personal CLI tool, not part of any application. It is a single,
self-contained crate that depends on nothing outside its own `Cargo.toml` and
can be copied anywhere on your `PATH`.

---

## Dependencies

| Tool              | Why                                | Install                                          |
| ----------------- | ---------------------------------- | ------------------------------------------------ |
| Rust toolchain    | builds the binary (`cargo`)        | [rustup.rs](https://rustup.rs) (`rustup`, stable) |
| `claude` on PATH  | the thing we resume into           | (already installed)                              |

That's it - no external CLI tools are required. The fuzzy matcher (nucleo), the
filesystem watcher (notify), and the terminal UI (ratatui + crossterm) are all
compiled into the binary. All crate versions are pinned exactly in `Cargo.toml`.

## Build

From the repository root:

```sh
cargo build --release
# -> target/release/snapback
```

## Install / run

```sh
# install both binaries onto your PATH (into ~/.cargo/bin by default):
cargo install --path .
snapback   # or: sb
```

`snapback` and `sb` are both produced by the one `cargo install` — the same
program under two names, so use whichever you prefer.

```sh
# or run in place without installing (build first):
cargo build --release
./target/release/snapback   # or: ./target/release/sb
```

Optional override:

- `CLAUDE_PROJECTS_DIR` - session store location (default `~/.claude/projects`).

## Usage

```sh
snapback       # browse the CURRENT folder's sessions (default scope)
sb             # same thing (short alias)
snapback -a    # browse EVERY folder's sessions, grouped repo -> branch
snapback --all # (long form of -a)
snapback -h    # help
```

There is no separate "search mode" flag: you always start in browse, and typing
filters live. `Tab` widens the match from name-only to name+content (below).

### Keys

| Key                | Action                                                            |
| ------------------ | ---------------------------------------------------------------- |
| `↑` / `↓`          | Move selection (always)                                          |
| `j` / `k`          | Move selection - **only while the query is empty**; once you are typing, `j`/`k` are ordinary search characters |
| `Enter`            | **Resume** the selected session (`chdir` to its `cwd`, spawn `claude -r <id>` as a child, then **return to the board** when it exits). If the session is **running** (has a live badge), Enter instead opens an **Attach / Fork / Cancel** choice - see below |
| `Ctrl-F`           | **Fork** it instead (`claude -r <id> --fork-session`; also returns to the board). Available for **any** session, running or not |
| `Ctrl-N`           | **Start a new session** in the launch directory (spawns `claude` in the folder `snapback` was launched from, then **returns to the board** when it exits). If you have Claude Code **agents** defined, it first opens an **agent picker** (`↑`/`↓` to choose, `Enter` to start, `Esc` to cancel) with a `default (no agent)` entry first; the last agent you picked is pre-highlighted. With no agents defined it launches a bare `claude` directly - see below |
| `Tab`              | Toggle search mode: **name-only ↔ name+content**                |
| `Ctrl-A`           | Toggle scope: **current folder ↔ all folders**                  |
| `Ctrl-/`           | Toggle the transcript **preview** pane                           |
| `PgUp` / `PgDn`    | Scroll the **preview** up / down a full page (always)            |
| `Ctrl-U` / `Ctrl-D`| Scroll the **preview** up / down a quarter page (always)         |
| `Home` / `End`     | Jump the **preview** to the top / bottom - `End` re-follows the newest turn (always) |
| mouse / trackpad wheel | Scroll the pane under the pointer - the **preview** (or the **list** selection when the pointer is over the list). Line-stepped, not pixel-smooth (see below) |
| left-click + drag the list/preview border | **Resize** the list and preview panes - press and drag the border between them; the chosen width persists across redraws and re-clamps to a sane minimum on both sides, including on terminal resize. Only active while the preview pane is visible |
| left-click a link in the **preview** | **Open** the link's url in your default browser - click an underlined markdown link (or a bare `https://…`) in the transcript. The url opens in the background (via `open` / `xdg-open`); the TUI stays up and never blocks |
| `Backspace`        | Delete the last query character                                  |
| any printable char | Type-to-search (append to the query)                             |
| `q`                | Quit - **only while the query is empty**; once typing, `q` is a search character |
| `Esc` / `Ctrl-C`   | Quit (always)                                                    |

`j`/`k`/`q` are disambiguated by whether the query is empty: in the default
browse state they navigate/quit; once you are typing a query they become
ordinary search input. Arrows, `Enter`, `Tab`, and every `Ctrl-` binding work
regardless of the query, so search is never blocked. (Terminals that map
`Ctrl-/` to the control code `0x1f` are handled too - it still toggles the
preview.)

**Mouse mode is on** so the wheel/trackpad can scroll and the border can be
dragged. Because the terminal captures mouse events, native **text selection /
copy** requires holding a modifier: **Shift** on most terminals, or
**Option/⌥** on iTerm2 and macOS Terminal. Wheel scrolling is **line-stepped** -
the terminal reports discrete wheel notches, not pixel-smooth deltas - so a
trackpad flick moves the preview a couple of lines per notch and the list one
row per notch. Dragging the list/preview border resizes the panes; the width
you choose sticks across redraws (and re-clamps sanely if you resize the
terminal). Left-clicking an underlined link (or a bare `https://…`) in the
preview opens it in your default browser - snapback captures the mouse, so it
resolves the click to the link itself and hands the url to the OS opener in the
background; the board never blocks and the browser opens behind it.

The header line shows the active scope, the active search mode, and a
`matched / total` session count; the bottom line is a one-row keybinding cheat
sheet.

---

## Features in depth

### Folder scoping (`--all` / `-a`, and `Ctrl-A`)

By **default** `snapback` shows only sessions launched from the **directory you
are in right now** (paths are canonicalized first, so symlinks and `.`/`..` are
collapsed before comparing). This keeps the list focused on "sessions for the
project I'm in".

- `--all` / `-a` starts in **all-folders** scope: every session, grouped by
  folder (the classic cross-machine view).
- `Ctrl-A` toggles between the two live, without restarting.

The match is deliberately an **exact folder** match, so a repo's *other*
worktree folders will not appear under the current-folder scope. Switch to
all-folders (or `cd` into the other worktree) to see them.

### Search: name-only vs. name+content (`Tab`)

Typing filters the list incrementally by **exact, contiguous substring** match
(case-insensitive with smart-case): a query matches only where it appears as a
literal run of characters, **not** as a scattered fuzzy subsequence. Two modes,
toggled with `Tab`:

- **name** (default, instant): matches the session's display **label** only -
  ideal for "which session was that".
- **name+content**: also matches the session's **transcript text**, so you can
  find a session by something that was actually said or done in it.

While a query is active, the matched substring is **highlighted** in each list
row's label. A **content-only** match (a term that lives in the transcript but
not in the label) still filters the row in, just without a label highlight.
Highlighting works in both the flat current-folder view and the `--all` grouped
view.

### Autorefresh

A filesystem watch over the session store keeps the list current: new, changed,
and removed sessions appear without a restart, and a burst of rapid writes is
coalesced into a single refresh. On refresh the list updates **in place** - your
**selection and scroll position are preserved**, and if the selected session
disappeared the selection moves to the nearest surviving row.

### Live sessions: badges + Attach / Fork / Cancel

`snapback` detects which sessions are **running right now** and marks them,
without ever blocking the UI. If detection is unavailable it simply degrades to
"nothing is live" - it never crashes the board.

- **Live badges.** A running row shows a compact badge in its own column:
  **`● bg`** for a background agent, **`● live`** for an interactive one, plus a
  dim `blocked` / `idle` qualifier. Non-running rows show nothing there.
- **Running sessions can't be plain-resumed.** A session that is currently
  running as an agent can't be resumed with `claude -r`. So pressing **`Enter`
  on a running row** opens a small in-board **Attach / Fork / Cancel** choice
  (`←`/`→` to choose, `Enter` to confirm, `Esc` to cancel):
  - **Attach** reattaches to the running agent in this terminal
    (`claude attach <job-id>`) - the supported way to reconnect to a live agent.
    `<job-id>` is the **short agent-view id** reported by `claude agents --json`
    (not the full session id), so Attach applies to **background** agents; an
    **interactive** session has no attachable job and the choice refuses with a
    hint to Fork or open it in its own terminal instead.
  - **Fork** branches off a copy of the session.
  - **Cancel** dismisses the overlay and stays on the board.
- **Enter on a non-running row** is an ordinary resume, and **`Ctrl-F` fork**
  stays available for **any** session, running or not.

### New sessions: start with an agent (`Ctrl-N`)

`Ctrl-N` starts a **fresh** `claude` session in the directory `snapback` was
launched from, then returns to the board when it exits. If you have **Claude
Code agents** defined, it first opens a small **agent picker** so the new session
can start bound to one (`claude --agent <name>`):

- The picker lists a **`default (no agent)`** entry first, then your discovered
  agents (name plus a dim description). `↑`/`↓` (or `k`/`j`) choose, `Enter`
  starts, `Esc`/`Ctrl-C` cancel back to the board.
- The **last agent you picked** is pre-highlighted, so repeating a choice is just
  `Ctrl-N` then `Enter`. That memory is **in-memory only** - it resets when you
  quit `snapback`, and nothing is written to disk.
- Agents are discovered **fail-soft** from Markdown files with YAML frontmatter
  under `~/.claude/agents/*.md` (user-level) and `<launch-dir>/.claude/agents/*.md`
  (project-level, which overrides a same-named user agent). Unreadable or
  malformed files are simply skipped.
- This list is a **convenience**: built-in and plugin agents are not on-disk
  files, so the scan cannot see them. Pick `default (no agent)` to launch a bare
  `claude` (and pass the agent yourself), or type/select from what was found.
- If **no** agents are discovered, `Ctrl-N` skips the picker entirely and starts
  a bare `claude` immediately - one keystroke, exactly as before.

### Readable transcript preview (`Ctrl-/`)

Toggle a preview of the selected session's transcript with `Ctrl-/`. It shows
the actual conversation - `you` / `claude` turns, with `tool_use` /
`tool_result` / `thinking` marked - rendered as **readable markdown** (headers,
bold/italic, links, code blocks, lists, and pane-width-fitted tables) instead of
raw JSON, so the
context that matters when resuming stays visible. Each turn is annotated with its
own timestamp. The pane opens anchored to the newest turn and is scrollable
(`PgUp`/`PgDn`, `Ctrl-U`/`Ctrl-D`, `Home`/`End`, and the mouse wheel); selecting
another session re-anchors to its newest turn. A dim scrollbar on the pane's
right border tracks your position and only appears once the transcript
overflows the viewport. Rendered links are underlined; **left-click** one to open
its url in your default browser (the url itself is kept out of the visible text,
never printed as an escape sequence).

---

## Docs for AI agents

Contributor guidance for AI coding agents lives in [`AGENTS.md`](AGENTS.md)
(the entry point + critical rules) and [`docs/agents/`](docs/agents/) (deeper
reference: architecture, the session-store domain, implementation patterns, and
operations). When the code structure changes — modules, commands, keys, flags,
or on-disk-format handling — refresh those files with the `project-agent-docs`
skill rather than hand-patching them.
