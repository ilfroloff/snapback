# snapback — browse, search & resume Claude Code sessions across every folder

You run Claude Code in a lot of places: several repos, a pile of git worktrees,
throwaway experiments. Days later you want *that* session back — the one where
you nailed the tricky migration, or left an agent running — but the built-in
`/resume` only shows the **current project**, in creation order, with **no way
to search what was actually said**. So you `cd` around guessing, scroll past
dozens of look-alike titles, and still can't reattach to a session that's
running as an agent.

`snapback` (short alias `sb`) is a single terminal app that puts **every**
Claude Code session on the machine into one searchable list and resumes any of
them in a keystroke. It gives you:

1. **One list for everything** — sessions from every repo and worktree, grouped
   repo → branch (git-log style), instead of one project at a time.
2. **Search by what happened** — filter by session name, or flip a toggle to
   search the **transcript text** and find the session by something that was
   actually said or done in it.
3. **Always current** — the list refreshes itself as sessions are created,
   changed, or removed. No restart.
4. **Focused by default** — you start scoped to the folder you're in; one key
   widens to every folder when you want the big picture.
5. **One keystroke to jump back in** — resume or fork the selected session, or
   start a brand-new one (optionally bound to one of your Claude Code agents),
   all without leaving the board.

---

## Install

`snapback` builds from source, so you need the **Rust toolchain**
([rustup.rs](https://rustup.rs), stable) to install it and **`claude` on your
`PATH`** to resume into. There is no prebuilt binary.

```sh
# from a local checkout (installs snapback and its sb alias onto your PATH):
cargo install --path .

# or straight from GitHub, latest main:
cargo install --git https://github.com/ilfroloff/snapback

# or a specific pinned release (reproducible):
cargo install --git https://github.com/ilfroloff/snapback --tag v0.1.0
```

Releases are tagged `vX.Y.Z` — pick one from the repo's Releases page. All three
forms install the same program under two names, `snapback` and the short alias
`sb`; use whichever you prefer.

Optional override:

- `CLAUDE_PROJECTS_DIR` — where sessions are stored (default `~/.claude/projects`).

## Usage

```sh
snapback       # browse the CURRENT folder's sessions (default scope)
sb             # same thing (short alias)
snapback -a    # browse EVERY folder's sessions, grouped repo → branch
snapback --all # (long form of -a)
snapback -h    # help
```

There is no separate "search mode": you always start in browse, and typing
filters the list live. `Tab` widens the match from name-only to name+content.

## Keys

| Key | Action |
| --- | ------ |
| `↑` / `↓` | Move the selection |
| `j` / `k` | Move the selection — while the query is empty; once you're typing, they're search characters |
| `Enter` | **Resume** the selected session, returning to the board when it exits. On a **running** session it opens an **Attach / Fork / Cancel** choice instead |
| `Ctrl-F` | **Fork** the selected session into a copy — available for any session, running or not |
| `Ctrl-N` | **Start a new session** in the launch directory; if you have Claude Code agents defined, pick one first (or `default (no agent)`) |
| `Tab` | Toggle search: **name-only ↔ name+content** |
| `Ctrl-A` | Toggle scope: **current folder ↔ all folders** |
| `Ctrl-/` | Toggle the transcript **preview** pane |
| `PgUp` / `PgDn` | Scroll the preview a full page |
| `Ctrl-U` / `Ctrl-D` | Scroll the preview a quarter page |
| `Home` / `End` | Jump the preview to the top / bottom |
| mouse wheel | Scroll the pane under the pointer |
| drag the pane border | Resize the list and preview panes |
| click a preview link | Open its url in your browser |
| `Backspace` | Delete the last query character |
| any printable char | Type to search |
| `q` | **Quit** — while the query is empty; once you're typing, it's a search character |
| `Esc` / `Ctrl-C` | Quit |

Mouse mode is on so the wheel can scroll and the pane border can be dragged; to
select/copy text natively, hold **Shift** (or **Option/⌥** on iTerm2 and macOS
Terminal). The header shows the active scope, the search mode, and a
`matched / total` count, with a version on the right — a release build shows the
version number, a local dev build is marked as such.

---

## Features

**Folder scoping.** By default you only see sessions from the folder you're in
right now, so the list stays about the project in front of you. `--all` / `-a`
starts wide, and `Ctrl-A` flips between the two without restarting.

**Search by name or by content.** Typing filters instantly by name. Press `Tab`
to also search inside the transcripts, so you can find a session by what was
actually said or done in it — not just by what it was titled. The matched text
is highlighted in the list.

**Autorefresh.** The list keeps itself current as you work: new sessions appear,
finished ones update, deleted ones drop out — all in place, with your selection
and scroll position preserved.

**Agent sessions at a glance.** Every session Claude Code is running — or has
recently finished running — as an agent carries a colored badge: a dot and a
short tag that share one color, so you can read the state of your agents straight
off the list.

- **yellow** — it **needs input**: stopped, waiting on you to answer.
- **green** — nothing is wanted from you: the session is either idle or finished.
  The word beside the badge says which.
- **gray, pulsing** — working right now.

The pulse is the tell for activity: only the working badge pulses, once a second,
and only its dot — which fades between bright and dim rather than blinking out,
so no text on the row ever moves or redraws and a busy board doesn't flicker.
Colors follow your terminal's theme.

Open the preview on a badged session and it leads with the same status in words,
pinned above the transcript so it stays in view while the transcript scrolls
beneath it — you can see why a session is sitting there before deciding what to
do about it. It reports what Claude Code reports, in Claude Code's own words, with
one exception: the two states that both mean *the session is waiting on you*
(`blocked` and `waiting`) are spelled out as `needs input`. Anything else is
passed through as-is rather than guessed at.

Because a session that's still running can't be plain-resumed, pressing `Enter`
on one offers **Attach** (reconnect to a running background agent), **Fork**, or
**Cancel** — so a live agent is never a dead end. A finished session resumes
normally; its badge tells you it's done without getting in the way.

Which of those you get is decided by asking Claude Code at the moment you press
`Enter`, not by the badge you're looking at. Badges refresh about once a second,
and a session can start or finish in between — so if one is secretly still
running, you get the Attach/Fork choice rather than an error. And if a resume
does fail because the session came back to life underneath you, the board says so
and offers the same choice instead of leaving you to guess.

**Start a new session, with an agent.** `Ctrl-N` starts a fresh session in the
folder you launched from. If you keep Claude Code agents defined, it offers a
quick picker so the new session can start bound to one, and it remembers your
last pick for the session so a repeat is just `Ctrl-N`, `Enter`.

**Readable transcript preview.** `Ctrl-/` opens a preview of the selected
session rendered as clean, scrollable markdown — the real conversation, so you
can confirm it's the right session before jumping back in. Links are clickable.

---

## Docs for AI agents

Contributor guidance for AI coding agents lives in [`AGENTS.md`](AGENTS.md)
(the entry point + critical rules) and [`docs/agents/`](docs/agents/) (deeper
reference: architecture, the session-store domain, implementation patterns, and
operations). When the code structure changes — modules, commands, keys, flags,
or on-disk-format handling — refresh those files with the `project-agent-docs`
skill rather than hand-patching them.
