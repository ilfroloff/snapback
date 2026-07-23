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

You need **`claude` on your `PATH`** to resume into. Everything else is one
command.

```sh
npx snapback-tui install
# or
bunx snapback-tui install
```

That installs prebuilt binaries — **no Rust toolchain required** — and then gets
out of the way: `install` puts the native `snapback` and `sb` on your `PATH`, so
you run them directly with no Node in between.

```sh
snapback       # and you're in
```

Prebuilt for macOS (arm64/x64) and Linux (x64/arm64). To install somewhere other
than `~/.local/bin`, set `SNAPBACK_INSTALL_DIR`. To uninstall, delete the two
binaries — `install` prints the exact command. You can also run it without
installing (`npx snapback-tui`), which is fine for a look, though the installed
binaries are the better way to live with it.

> The npm package is named **`snapback-tui`**, not `snapback`: the bare name on
> npm belongs to an unrelated package. The commands it installs are still
> `snapback` and `sb`.

### From source

Needs the **Rust toolchain** ([rustup.rs](https://rustup.rs)):

```sh
# from a local checkout (installs snapback and its sb alias onto your PATH):
cargo install --path .

# or straight from GitHub, latest main:
cargo install --git https://github.com/ilfroloff/snapback

# or a specific pinned release (reproducible):
cargo install --git https://github.com/ilfroloff/snapback --tag v0.2.0
```

Releases are tagged `vX.Y.Z` — pick one from the repo's Releases page. Every form
above installs the same program under two names, `snapback` and the short alias
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
| `←` / `→` | **Fold** / **expand** a stack of look-alike rows that are really one conversation — a row marked `(+N)` stands for `N` more |
| `Enter` | **Resume** the selected session, returning to the board when it exits. On a **running** session it opens an **Attach / Fork / Cancel** choice instead |
| `Ctrl-F` | **Fork** the selected session into a copy — available for any session, running or not |
| `Ctrl-N` | **Start a new session** in the launch directory; if you have Claude Code agents defined, pick one first (or `default (no agent)`) |
| `Ctrl-R` | **Quick reply** — send a one-shot message to the selected session without leaving the board; a finished (`done`) or waiting (`needs input`) background agent is stopped first (with a confirm for a waiting one) so the reply lands in place. Opens a compose box (`Enter` sends, `Ctrl-J` / `Alt+Enter` newline, `Esc` cancels) |
| `Ctrl-K` | **Stop / interrupt** the selected session's live background agent (`claude stop`). A finished (`done`) agent stops immediately; any other live agent (`working`, `needs input`, `idle`) confirms first, since stopping ends the live job (its conversation is kept). An interactive session running in another terminal can't be stopped from here |
| `Ctrl-X` then `x` / `d` / `h` | **Leader chord** for hide & delete — `x` **hides** the selected session (reversible, persisted), `d` **hard-deletes** it after a confirmation, `h` toggles **show hidden**. Any other key cancels the chord |
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

### Getting back to the board from inside a session

Once you've resumed into a Claude Code session, the tidy ways back to snapback are
slash commands you type in Claude, not a snapback key:

- **`/bg`** — detaches the session so it keeps running as a background agent and
  drops you straight back onto the board. It behaves the same whether you resumed
  a regular session or attached to a running one, and the session reappears on the
  list with a live `bg` badge — so you can Attach, quick-reply (`Ctrl-R`), or fork
  it later.
- **`/exit`** — ends the session and returns you to the board.

Prefer either over `Ctrl-Z` as a way out: it only detaches cleanly when you're
*attached* to a background agent (Claude Code intercepts it). In a regular
interactive session it's an OS suspend (`SIGTSTP`) that can hand the terminal back
dirty — snapback repaints from a known-good state on return, but `/bg` (keep it
running) and `/exit` (end it) are the clean exits.

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

**No more look-alike duplicates.** Every time you hand a prompt to a background
agent, Claude Code quietly copies the session into a new file and carries on
there. Both copies keep the same name, the same folder and the same branch — so
one conversation shows up as two, three, four rows you can't tell apart, drifting
further apart in the list as the day goes on.

snapback spots that they're the same conversation (by what's inside the files,
not by their names) and shows you **one row**, marked `(+N)` for the `N` copies
behind it. Press `→` and they fan out underneath it, oldest work included, no
matter how far apart in time they landed; `←` tidies them away again. Each copy
says how many messages it holds — `6 msgs` next to `171 msgs` — so you can see
which one is a stub the hand-off left behind and which one holds the real work.
The names can't tell you that; they're identical.

Nothing is hidden from you and nothing is thrown away — every copy is still a
real session you can resume, and that matters: a session that's running in the
background can't be plain-resumed, so the older copy is often the one that
*will* open. It's one keypress away instead of lost in a row of twins.

**Start a new session, with an agent.** `Ctrl-N` starts a fresh session in the
folder you launched from. If you keep Claude Code agents defined, it offers a
quick picker so the new session can start bound to one, and it remembers your
last pick for the session so a repeat is just `Ctrl-N`, `Enter`.

**Quick reply without leaving the board.** Sometimes you just want to ask
yesterday's session a fast question. `Ctrl-R` opens a compose box for the selected
session and sends your message with a one-shot `claude -p` — it replays the full
context, appends the exchange in place, and the reply shows up in the preview, all
while the board stays up. The box is a real multiline editor — arrows move the
caret, long lines soft-wrap, and it grows from one line as you type (`Ctrl-J` or
`Alt+Enter` for a newline, `Enter` to send). The moment you send, your message
appears in the preview under a **you** turn, followed by a live **claude
sending… / cooking…** placeholder — so the exchange reads normally while the reply
is still in flight. The placeholder is replaced in place as `claude` writes the
real turns, and the status line reports what the reply cost (or the reason if it
fails).

Background agents get special handling, because `claude` won't resume a session
it's still holding as an agent. A **finished** (`done`) agent is stopped first
(its conversation is kept) so the reply can land in place; a **waiting**
(`needs input`) agent asks you to confirm before it's stopped. An agent that's
actively **working** is left alone — use Attach to answer it in its own channel,
or Fork (`Ctrl-F`) to branch a copy.

**Hide & delete.** `Ctrl-X` is a leader chord for trimming the board: press it,
and a hint shows the follow-ups — `x`, `d`, `h` — while any other key cancels.

- `Ctrl-X x` **hides** the selected session. This is the reversible default: the
  session stays on disk, it just drops off the board. The hidden set is
  remembered across restarts, so a session you hide stays hidden next time. Press
  `Ctrl-X x` again on a revealed row to un-hide it.
- `Ctrl-X h` **toggles showing hidden sessions**. Hidden rows come back dimmed and
  marked `[hidden]`, still carrying their live badge if their agent is running —
  hiding is a visibility choice, not a claim that a session is finished.
- `Ctrl-X d` **hard-deletes** the selected session — physically removing its
  transcript from disk. Because that is irreversible, it asks first with a
  confirmation prompt (defaulted to Cancel), and it refuses a session whose agent
  is currently running rather than pull a file out from under it. Deletion removes
  exactly that session's own `<id>.jsonl` and its sibling `<id>/` directory of
  subagent transcripts — nothing else.

Hiding is the only thing snapback ever writes for itself. The list of hidden
session ids lives in its own config directory —
`$SNAPBACK_CONFIG_DIR/state/hidden_sessions` if you set `SNAPBACK_CONFIG_DIR`,
otherwise `~/.config/snapback/state/hidden_sessions` — never inside the Claude
Code session store, which snapback otherwise only reads.

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
