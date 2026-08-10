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
4. **Focused by default** — you start scoped to the folder you're in, and one key
   widens to the whole project (every git worktree of the repo you launched in)
   and back. Every folder on the machine is a step further out, and stays behind
   a launch flag rather than on that key.
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
snapback           # browse the CURRENT folder's sessions (default scope)
sb                 # same thing (short alias)
snapback -p        # browse THIS PROJECT's sessions — the repo you launched in
                   # and all of its git worktrees, grouped by branch under one
                   # project head
snapback --project # (long form of -p)
snapback -a        # browse EVERY folder's sessions, grouped repo → branch —
                   # and put that scope on Ctrl-A, which is the only way to
                   # reach it
snapback --all     # (long form of -a)
snapback -h        # help
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
| `Ctrl-N` | **Start a new session** in the launch directory; if you have Claude Code agents defined, pick one first (or `default (no agent)`). Then a **draft box** opens for the session's first message: `Enter` launches it with `claude --bg` and leaves you on the board, `Ctrl-O` runs it interactively instead, `Esc` cancels. Your message is sent as the session's first turn either way |
| `Ctrl-O` (in that picker) | **Start the highlighted agent interactively at once**, skipping the draft — the same thing `Ctrl-O` means inside the draft box, so either route out of the picker is one keypress |
| `Ctrl-R` | **Quick reply** — send a one-shot message to the selected session without leaving the board. A background agent whose run is over (`done`, `stopped`, `failed`) is stopped first so the reply lands in place; a waiting one (`needs input`) asks you to confirm that stop; one that is still live (`working`, `idle`, `interrupted`, or a state this version doesn't recognize) is left alone and refused. Opens a compose box (`Enter` sends, `Ctrl-J` / `Alt+Enter` newline, `Esc` cancels) |
| `Ctrl-K` | **Stop / interrupt** the selected session's live background agent (`claude stop`). An agent whose run is over (`done`, `stopped`, `failed`) stops immediately; every other live agent (`working`, `needs input`, `idle`, `interrupted`, unrecognized) confirms first, since stopping ends the live job (its conversation is kept). A session that isn't running as an agent has nothing to stop, and an interactive session running in another terminal can't be stopped from here |
| `Ctrl-X` then `x` / `d` / `h` / `r` | **Leader chord** for trimming and refreshing the board — `x` **hides** the selected session (reversible, persisted), `d` **hard-deletes** it after a confirmation that can take just that row or its whole `(+N)` stack, `h` toggles **show hidden**, `r` **re-reads every transcript from disk**. Any other key cancels the chord |
| `Tab` | Toggle search: **name-only ↔ name+content** |
| `Ctrl-A` | Flip scope: **current folder ↔ project** — the project being the repo you launched in and all of its git worktrees. Started with `-a` it is a three-stop cycle instead (current folder → project → all folders), which is the only way to reach all folders |
| `Ctrl-/` | Toggle the transcript **preview** pane |
| `PgUp` / `PgDn` | Scroll the preview a full page |
| `Ctrl-U` / `Ctrl-D` | Scroll the preview a quarter page |
| `Home` / `End` | Jump the preview to the top / bottom |
| mouse wheel | Scroll the pane under the pointer |
| drag the pane border | Resize the list and preview panes |
| click a preview link | Open its url in your browser |
| `Backspace` | Delete the last query character |
| any printable char | Type to search |
| paste (`Cmd`/`Ctrl-V`, middle-click) | Your terminal's own paste, taken as **text**: into a compose or draft box at the cursor, **newlines intact** (no more sending just the first line); on the board, appended to the query with newlines as spaces. It never sends, resumes, or confirms |
| `q` | **Quit** — while the query is empty; once you're typing, it's a search character |
| `Esc` / `Ctrl-C` | Quit |

Mouse mode is on so the wheel can scroll and the pane border can be dragged; to
select/copy text natively, hold **Shift** (or **Option/⌥** on iTerm2 and macOS
Terminal). The header shows the active scope, the search mode, and a
`shown / total` count, with a version on the right — a release build shows the
version number, a local dev build is marked as such.

Both numbers count **conversations**, not files: a folded fork lineage is one
row wearing a `(+2)`, and it counts once on each side — so opening or closing a
`(+N)` never moves the counter.

That **total is this project**, not the whole store: in the default folder scope
it counts every conversation of the repo you launched from, worktrees included,
so `5 / 30 sessions` reads "5 here, 30 in the project" and tells you what
`Ctrl-A` would open up. (`--all` is the exception — showing every repo on the
machine, it counts every conversation on the machine.) In the project and
`--all` scopes with no search typed, the two sides therefore match: `115 / 115`.
Conversations you've hidden with `Ctrl-X x` are not in that total; they're
disclosed after it instead, as `· 3 hidden`, and fold back into the total while
`Ctrl-X h` is revealing them.

### Getting back to the board from inside a session

Once you've resumed into a Claude Code session, the tidy ways back to snapback are
slash commands you type in Claude, not a snapback key:

- **`/bg`** — detaches the session so it keeps running as a background agent and
  drops you straight back onto the board. It behaves the same whether you resumed
  a regular session or attached to a running one, and the session reappears on the
  list with a live `bg` badge — so you can Attach it, fork it, or stop it
  (`Ctrl-K`). Quick reply (`Ctrl-R`) waits until its run is over: while the agent
  is genuinely live, snapback refuses rather than interrupt it.
- **`/exit`** — ends the session and returns you to the board.

Prefer either over `Ctrl-Z` as a way out: it only detaches cleanly when you're
*attached* to a background agent (Claude Code intercepts it). In a regular
interactive session it's an OS suspend (`SIGTSTP`) that can hand the terminal back
dirty — snapback repaints from a known-good state on return, but `/bg` (keep it
running) and `/exit` (end it) are the clean exits.

---

## Features

**Folder scoping.** By default you only see sessions from the folder you're in
right now, so the list stays about the project in front of you. `--project` /
`-p` starts one step wider — the repo you launched in **and all of its git
worktrees**, so work split across worktrees shows up as one project instead of
scattered folders — and `--all` / `-a` starts wide. `Ctrl-A` flips between the
first two without restarting.

All folders is the whole store — every session of every repo on the machine — so
it is the **launch flag's alone**: `--all` / `-a` both starts there and adds it
as a third stop on `Ctrl-A`. Without the flag that key cannot reach it, which
keeps a one-key press inside the project you are working on.

The project scope asks git which worktrees the repo has, and re-asks on every
refresh, so a worktree you add while snapback is running joins the list on its
own. It also keeps the worktrees you have since **deleted** — git can only report
the ones that still exist, so those sessions used to be findable under `--all`
alone. They stay browsable, searchable and hideable; they just can't be resumed,
because the folder they ran in is gone. Outside a git repo — or if git can't
answer — the scope falls back to the repo folder your launch directory sits in,
rather than showing an empty board.

**Search by name or by content.** Typing filters instantly by name. Press `Tab`
to also search inside the transcripts, so you can find a session by what was
actually said or done in it — not just by what it was titled. The matched text
is highlighted in the list.

**Autorefresh.** The list keeps itself current as you work: new sessions appear,
finished ones update, deleted ones drop out — all in place, with your selection
and scroll position preserved. It re-reads only the transcripts that actually
changed, so a board left open beside a busy agent costs close to nothing, however
many sessions you have accumulated. `Ctrl-X r` forces a full re-read if you ever
want one.

**Agent sessions at a glance.** Every session Claude Code is running — or has
recently finished running — as an agent carries a colored badge: a dot and a
short tag that share one color, so you can read the state of your agents straight
off the list.

- **yellow** — it **needs input**: stopped, waiting on you to answer.
- **green** — nothing is wanted from you: the session is either idle or finished.
  The word beside the badge says which.
- **gray, pulsing** — working right now.
- **gray, steady** — **interrupted**: a background agent Claude Code still lists
  as working while its own status for it reads idle, so the badge holds still
  instead of pulsing as if a turn were in flight.
- **dim gray, steady** — the agent has ended: it was stopped or its run failed.
  The word beside the badge says which.

The pulse is the tell for activity, and it is the *first* thing to read — not the
shade. Only a genuinely working badge pulses, once a second, and only its dot,
which fades between the bright and the dim gray rather than blinking out, so no
text on the row ever moves or redraws and a busy board doesn't flicker. That fade
passes through exactly the dim gray an ended agent wears, so a glance at the dot
alone can't tell a working agent from a finished one — but a working dot *moves*
and the other two hold still. Two things keep it unambiguous: the word beside the
badge never pulses, so it always shows the badge's real color; and once you can see
a dot is steady, the shade separates the two at rest — the working gray is the
interrupted one, the dimmer gray is a run that has ended. Colors follow your
terminal's theme.

Open the preview on a badged session and it leads with the same status in words,
pinned above the transcript so it stays in view while the transcript scrolls
beneath it — you can see why a session is sitting there before deciding what to
do about it. It reports what Claude Code reports, in Claude Code's own words, with
two exceptions. The two states that both mean *the session is waiting on you*
(`blocked` and `waiting`) are spelled out as `needs input`. And a background agent
Claude Code still calls `working` while its own status reads `idle` — the shape of
one that was interrupted and never cleaned up — is labelled `interrupted` (Claude
Code's own word) and held steady. Anything else is passed through as-is rather
than guessed at.

Because a session that's still running can't be plain-resumed, pressing `Enter`
on one offers **Attach** (reconnect to a running background agent), **Fork**, or
**Cancel** — so a live agent is never a dead end. A finished session resumes
normally; its badge tells you it's done without getting in the way.

Which of those you get is decided by asking Claude Code at the moment you press
`Enter`, not by the badge you're looking at. Badges refresh every few seconds
while you're active (and stop once the board has sat idle for a minute, picking
back up as soon as you touch it again), and a session can start or
finish in between — so if one is secretly still running, you get the
Attach/Fork choice rather than an error. And if a resume does fail because the
session came back to life underneath you, the board says so and offers the
same choice instead of leaving you to guess.

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

**Hand an agent a job and stay put.** `Ctrl-N` starts a fresh session in the
folder you launched from. If you keep Claude Code agents defined, it offers a
quick picker so the new session can start bound to one, and it remembers the last
agent you actually started so a repeat is just `Ctrl-N`, `Enter`, `Enter`.

`Enter` on a pick — or `Ctrl-N` on its own, if you have no agents defined — opens
a draft box rather than starting anything. The preview pane clears to a
placeholder while you draft — which agent is about to run, the folder it will run
in, and the keys you can press. Nothing else. That blankness is deliberate: the
session doesn't exist yet, and a draft box floating over the last conversation you
had open reads like a reply to *it*.

Type what you want done and press `Enter`: snapback runs `claude --bg`, the agent
starts working in the background, and you never leave the board — it shows up on
the list a moment later with a live badge, ready to `Ctrl-K` stop or `Ctrl-R`
reply to like any other.

**Or take the terminal instead.** `Ctrl-O` runs the agent interactively, handing
you the terminal as usual. It works from the draft box (if you change your mind
mid-sentence, your draft comes along as the first turn) *and* straight from the
picker, where it skips the draft entirely. So both ways out are a single
keypress — the background one just happens to be the one `Enter` falls on now.

One thing to know either way: your draft is sent as the session's **first turn**,
immediately. Claude Code's CLI has no way to put text in the input box for you to
edit before sending — the only mechanism it offers is passing the prompt on the
command line, which submits it — so write it as the instruction you mean, not as
a note to yourself. The status line then reports whether the agent started, and
says so plainly if Claude Code had a complaint (an agent name it doesn't
recognize, for instance, starts the session *without* that agent — snapback tells
you rather than reporting a clean start).

**Quick reply without leaving the board.** Sometimes you just want to ask
yesterday's session a fast question. `Ctrl-R` opens a compose box for the selected
session and sends your message with a one-shot `claude -p` — it replays the full
context, appends the exchange in place, and the reply shows up in the preview, all
while the board stays up. The box is a real multiline editor — arrows move the
caret, long lines soft-wrap, and it grows from one line as you type (`Ctrl-J` or
`Alt+Enter` for a newline, `Enter` to send). The moment you send, your message
appears in the preview under a **you** turn, followed by a live **claude
cooking…** placeholder — so the exchange reads normally while the reply is still
in flight. The placeholder is replaced in place as `claude` writes the
real turns, and the status line reports what the reply cost (or the reason if it
fails). Confirmations and nudges fade after a few seconds; failures and refusals
stay until you press a key, so nothing is silently downgraded.

Background agents get special handling, because `claude` won't resume a session
it's still holding as an agent. An agent whose run is **over** — `done`, or
`stopped`/`failed` — is stopped first (its conversation is kept) so the reply can
land in place. A **waiting** (`needs input`) agent asks you to confirm before it's
stopped, since that abandons an agent that's still live. Anything still live is
left alone and the reply is refused: `working`, `idle`, `interrupted`, or a state
this version doesn't recognize — use Attach to answer it in its own channel, or
Fork (`Ctrl-F`) to branch a copy. `interrupted` refuses on purpose even though it
sits still: that badge is snapback's *inference* from Claude Code contradicting
itself, not a report that the run ended, and it isn't worth stopping live work over
a guess. Use `Ctrl-K` if you do want it stopped — it will ask first.

**Hide & delete.** `Ctrl-X` is a leader chord for trimming the board: press it,
and a hint shows the follow-ups — `x`, `d`, `h`, `r` — while any other key cancels.

- `Ctrl-X x` **hides** the selected session. This is the reversible default: the
  session stays on disk, it just drops off the board. A `(+N)` stack always hides
  and returns whole, so the row genuinely leaves rather than being replaced by
  the next copy behind it. The hidden set is remembered across restarts, so a
  session you hide stays hidden next time. Press `Ctrl-X x` again on a revealed
  row to un-hide it.
- `Ctrl-X h` **toggles showing hidden sessions**. Hidden rows come back dimmed and
  marked `[hidden]`, still carrying their live badge if their agent is running —
  hiding is a visibility choice, not a claim that a session is finished.
- `Ctrl-X r` **re-reads every transcript from disk**. You should not normally need
  it: the board already refreshes itself as files change, and it keeps the reading
  it took of any transcript nothing has written to since. `r` throws that away and
  reads the whole store again, which is the answer if a row ever looks out of date
  — on a network drive with a coarse clock, say. It reports how many sessions it
  landed on, and costs nothing but the re-read.
- `Ctrl-X d` **hard-deletes** the selected session — physically removing its
  transcript from disk. Because that is irreversible, it asks first with a
  confirmation prompt (defaulted to Cancel). On a row that stands for a `(+N)`
  stack the prompt also offers **Delete lineage** — the whole family of look-alike
  copies at once, which is what hiding already does. Without it, deleting the top
  row would leave the copies behind and the next one would simply take its place,
  so the row never actually left the board. Deletion removes exactly each target
  session's own `<id>.jsonl` and its sibling `<id>/` directory of subagent
  transcripts — nothing else.

  What it refuses is a session something might be **writing**: one you have open
  in a Claude Code window, a background agent Claude Code still has up — working
  a turn, sitting idle between turns, or reporting something snapback can't read
  (an unreadable signal never gets to authorize an irreversible delete) — or one
  snapback itself is still replying to. A quick reply (`Ctrl-R`) keeps writing
  after the board comes back, so a delete aimed at that session waits until the
  reply has landed. Fair game is a background agent that isn't churning: one
  *waiting on you*, one Claude Code still reports as working while its own status
  reads idle (**interrupted**), or one that has reported it finished. Claude Code
  keeps listing agents long after they go quiet, and refusing all of them made
  delete useless for almost every row on the board. Two things worth knowing before you
  confirm, and the prompt says both: removing the transcript doesn't stop the
  agent — it stays in Claude Code until you stop it there — and if you later
  attach to it and reply, a new transcript is written under that session. In a
  lineage, members that are still running are skipped and the rest are deleted;
  the board reports the split.

  A lineage delete takes the whole family, including copies you've hidden —
  hiding is a visibility choice, so it doesn't spare a copy here. When some of
  them are hidden the prompt leads with the numbers (`3 in this lineage, 2 of
  them hidden`), so the count on the button is never more than you expected.

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
