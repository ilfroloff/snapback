# snapback — user guide

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

Sessions are read from `~/.claude/projects` by default; set `CLAUDE_PROJECTS_DIR`
to read them from elsewhere.

There is no separate "search mode": you always start in browse, and typing
filters the list live. `Tab` widens the match from name-only to name+content.

## Keys

| Key | Action |
| --- | ------ |
| `↑` / `↓` | Move the selection |
| `←` / `→` | Move the **cursor in your search** one character, to fix a typo without retyping the rest. The list and the preview stay where they are |
| `Alt+←` / `Alt+→` (`⌥←` / `⌥→`) / `Alt+B` / `Alt+F` / `Ctrl-←` / `Ctrl-→` | Move the **cursor in your search** one **word**, landing where a word jump in the reply box does: forward goes to the start of the next word. Depending on the terminal, `⌥←` / `⌥→` arrives as `Alt+←` / `Alt+→` or as `Alt+B` / `Alt+F`, so both are bound; `Ctrl-←` / `Ctrl-→` does the same without Option. The list and the preview stay where they are |
| `Enter` | **Resume** the selected session, returning to the board when it exits. On a **running** session it opens an **Attach / Fork / Cancel** choice instead |
| `Ctrl-F` | **Fork** the selected session into a copy — available for any session, running or not |
| `Ctrl-N` | **Start a new session** in the launch directory; if you have Claude Code agents defined, pick one first (or `default (no agent)`). Then a **draft box** opens for the session's first message: `Enter` launches it with `claude --bg` and leaves you on the board, `Ctrl-O` runs it interactively instead, `Ctrl-L` picks its model, `Ctrl-J` / `Alt+Enter` newline, `Esc` cancels, and a `/` or `@` opens the pick list (the `/` or `@` row below). Your message is sent as the session's first turn either way |
| `Ctrl-O` (in that picker) | **Start the highlighted agent interactively at once**, skipping the draft — the same thing `Ctrl-O` means inside the draft box, so either route out of the picker is one keypress |
| `Ctrl-R` | **Quick reply** — send a one-shot message to the selected session without leaving the board. A background agent whose run is over (`done`, `stopped`, `failed`) is stopped first so the reply lands in place; a waiting one (`needs input`) asks you to confirm that stop; one that is still live (`working`, `idle`, `interrupted`, or a state this version doesn't recognize) is left alone and refused, and so is a session with no background job to stop first (a `live` one, for instance); the refusal suggests `Ctrl-K` or Fork instead. While a session's own reply is still being sent, `Ctrl-R` on that session is refused until it lands; replies to other sessions can go out at the same time. Opens a compose box (`Enter` sends, `Ctrl-L` picks the model, `Ctrl-J` / `Alt+Enter` newline, `Esc` cancels, and a `/` or `@` opens the pick list — the `/` or `@` row below) |
| `Ctrl-L` (in a reply or draft box) | **Pick the model** — and, with `←` / `→`, its **effort** — for **this message only**. The box's bottom border names what it will run on: a reply says `model: session (Opus 5.5)`, the model that session last answered with, which Claude Code normally keeps on its own; a draft says `model: default (opus[1m]) (new sessions only)` when your Claude Code settings name a model. Either says plain `model: default` when there is nothing to name. `--model` / `--effort` are sent only when you pick something; the picker's first row goes back to the default. Every new box starts at the default, and nothing is remembered. `Enter` resume, `Ctrl-F` fork and Attach never send a model |
| `/` or `@` (in a reply or draft box) | **Pick a skill, command, file or agent** — the same list in a `Ctrl-R` reply and a `Ctrl-N` draft. `/` as the very first character lists the skills and commands Claude Code offers in that folder, built-in ones included; `@` at the start of a word lists files and folders (`@src/tu` narrows inside `src/`) and, for a top-level `@`, the folder's agents, inserted as `@agent-<name>`. A skill, command or agent shows its description on the right. The letters you type narrow the list to what holds them anywhere in its name or, for a skill, command or agent, its description, matched like the search box (an uppercase letter matches exactly): names that start with them come first, then other name matches, then description matches, with files and folders above agents; a file or folder whose name starts with `.` shows only once you type its leading `.`. While it is open `↑` / `↓` choose, `Enter` or `Tab` picks (a folder reopens the list one level down), and `Esc` closes only the list — never the draft; with no list open `Enter` and `Esc` do what the box's own row says. Where the list comes from is under *Quick reply without leaving the board* |
| `Ctrl-K` | **Stop / interrupt** the selected session's live agent. On a background agent it runs `claude stop`: one whose run is over (`done`, `stopped`, `failed`) stops immediately; every other live agent (`working`, `needs input`, `idle`, `interrupted`, unrecognized) confirms first, since stopping ends the live job (its conversation is kept). A session with no background job (a `live` one, typically) has no job to stop, so if Claude Code reports a process id for it, `Ctrl-K` offers to send that process a **SIGTERM** instead: the confirmation shows the pid, and nothing is sent unless Claude Code still reports that same pid when you press `Enter`. A session that isn't running as an agent has nothing to stop, and neither does one Claude Code reports with no job and no process id that can be signalled |
| `Ctrl-X` then `x` / `d` / `h` / `r` / `y` / `f` | **Leader chord** that acts on the selected row (`x`, `d`, `y`, `f`) or on the whole board (`h`, `r`) — `x` **hides** the selected session (reversible, persisted), `d` **hard-deletes** it after a confirmation that can take just that row or its whole `(+N)` stack, `h` toggles **show hidden**, `r` **re-reads every transcript from disk**, `y` is **copy session ID**: the selected session's full id goes to your clipboard and shows on the status line, `f` **folds** / **expands** a stack of look-alike rows that are really one conversation — a row marked `(+N)` stands for `N` more; `f` opens it, and folds it back from any of its rows. Any other key cancels the chord |
| `←` / `→` (in the model picker) | **Step the highlighted model's effort** down / up: `default effort` (no `--effort`, your settings decide) → `low` → `medium` → `high` → `xhigh` → `max`, wrapping round both ways. `Enter` sets the model and the effort together into the box; `Esc` goes back to the box with your text and its previous choice untouched. They do nothing on the picker's first (default) row, and nothing in the agent picker — and inside either picker they never move the search cursor on the board underneath |
| `Tab` | Toggle search: **name-only ↔ name+content**. Widening to content also opens the preview on the most recent match, the same way typing does |
| `Ctrl-A` | Flip scope: **current folder ↔ project** — the project being the repo you launched in and all of its git worktrees. Started with `-a` it is a three-stop cycle instead (current folder → project → all folders), which is the only way to reach all folders |
| `Shift-←` / `Shift-→` | Change the **layout** one step — how the screen is split between the session list and the transcript preview, list:preview: `0:1` (preview only) · `1:3` · `1:1` · `3:1` · `1:0` (list only). You start at `1:1`; `Shift-←` gives the preview more room, `Shift-→` gives the list more, and a press at either end does nothing. Works with or without a search typed. The preview keeps your place as it resizes; coming back from `1:0` it opens on the newest turn. At `0:1` the preview's title names the selected session, and `↑` / `↓` still move between sessions |
| `PgUp` / `PgDn` | Scroll the preview a full page |
| `Ctrl-U` / `Ctrl-D` | Scroll the preview a quarter page |
| `Ctrl-T` / `Ctrl-E`, `Home` / `End` | Jump the preview to the top / bottom. Every preview scroll key (`PgUp` / `PgDn`, `Ctrl-U` / `Ctrl-D`, `Ctrl-T` / `Ctrl-E`, `Home` / `End`) also works while a quick-reply box is open and leaves your text and cursor alone; there they replace the editor's own meaning (`Ctrl-U` delete to line start, `Ctrl-D` delete forward, `Ctrl-E` / `Home` / `End` / `PgUp` / `PgDn` caret moves — the arrows and `Ctrl-A` / `Ctrl-F` / `Ctrl-B` still move it). A new-session draft keeps all of them as editor keys, since its pane shows no transcript (on a MacBook keyboard without dedicated `Home`/`End` keys, `fn+←` / `fn+→` reach the same two) |
| `Shift-↑` / `Shift-↓` | Walk the preview through the lines your query marks — previous / next. Only bound while something IS marked in the previewed transcript; with nothing marked they stay plain **move the selection**, so they never take a key away from you (and a terminal that swallows the modifier still moves). One stop per marked **line**, not per occurrence: a line saying your query twice is marked twice and stopped at once |
| mouse wheel | Scroll the pane under the pointer — except while a compose or draft box is open, when the session list stops taking notches: a wheel over it does **nothing**, so the session you are writing to can never slide out from under you. Everywhere else it is unchanged, and a notch anywhere but the list still scrolls the transcript |
| drag inside the preview | Select transcript text (in reading order, like your terminal's own selection, but only the transcript's drawn text — never the empty space right of a line, and never the session list beside it) and copy it to the clipboard when you let go. Hold the drag past the preview's top or bottom edge and it keeps scrolling that way — faster the further past the edge you hold it — with the selection growing until you let go, so it can copy far more than one screen. A drag over empty space alone copies nothing. A drag that starts on a link or a folded node selects it rather than opening or unfolding it. Works while a reply box is open — it neither moves your cursor nor touches what you typed — but not while a new-session draft is, whose placeholder replaces the transcript; and never from the pinned row at the top of the pane |
| double-click in the preview | Select the **word** under the pointer and copy it when you let go, exactly as a drag copies. Two clicks on the same cell within half a second; words follow Unicode word boundaries, so `view.rs`, `don't` and `snake_case` are one word but `sess-link` is two, and paths and URLs split at their punctuation. The first click still does what a click does — opens a link, or unfolds a folded node — and the second only selects, so a node you double-click is unfolded once and stays open. Double-clicking blank space copies nothing; a quick third click keeps the word. Works while a reply box is open, like a drag |
| click a preview link | Open its url in your browser — when you let go, so a drag that starts on a link selects it instead. `http`/`https` only: a link of any other scheme opens **nothing** and instead reports a refusal on the status line that names the link (`not opening <url> - only http/https links open`), so an underlined label that quietly does nothing never leaves you guessing which of the two happened. That refusal is **sticky** — it stays up until your next actionable keypress. Works while a reply box is open |
| click a folded node | Unfold it where it sits — when you let go, like a link — and click it again to fold it back. Two kinds of turn arrive folded to one line: a subagent's hand-back — the `◆ message from @…` node, otherwise ~95 rows of `<agent-message>` frame attributed to **you** — and context Claude Code added on your behalf, such as the instructions a skill or slash command expands into — the `◇ added by claude code` node, otherwise often thousands of rows, also attributed to **you**. Folded each costs one line, and its text is still one click away. Which nodes you left open is remembered for this run only — snapback writes nothing for it. Works while a reply box is open |
| `Backspace` | Delete the query character before the cursor |
| `Alt+Backspace` (`⌥⌫`) / `Ctrl-W` / `Alt+H` | Delete the query **word** before the cursor — one whole search term, so a path or a branch name goes in a single press instead of character by character (`Backspace` alone still takes one character); whatever follows the cursor stays. All three keys do the same thing, because which of them your terminal actually sends depends on how it treats **Option**; they are also the same three the reply and draft boxes word-delete on, so the gesture is bound wherever you type. What a press *cuts* differs on purpose: on the board it takes a whole search term, so `feature/fold-fork-lineages` goes in one press, while in a reply or draft box the cut stops at punctuation and takes only `lineages` |
| any printable char | Type to search, at the cursor |
| paste (`Cmd`/`Ctrl-V`, middle-click) | Your terminal's own paste, taken as **text**: into a compose or draft box at the cursor, **newlines intact** (no more sending just the first line); on the board, into the query at its cursor with newlines as spaces. It never sends, resumes, or confirms |
| `Esc` | Clear the search query; with nothing typed, quit |
| `Ctrl-C` | Quit (always) |

The `Ctrl-X y` copy goes through your OS clipboard tool — `pbcopy` on macOS;
`wl-copy`, `xclip` or `xsel` on Linux — and the status line says
`Copied session ID …` only when that tool reports success. Over SSH, with no
tool to use, or when the tool fails, it falls back to a write-only OSC 52
escape, which reaches your clipboard only if the terminal — and tmux, via
`set -g set-clipboard on` — lets programs set it; that line says
`Sent session ID …`, never `Copied`. Either way the full id stays on the status
line until your next key, so you can select it by hand.

Mouse mode is on so the wheel can scroll, a preview link can be clicked open and
a folded node clicked to unfold — each click acting when you let go, since only
then is it a click rather than the start of a drag.
**Drag inside the preview** to select transcript text — the selection is
reverse-videoed and copied to your clipboard on release, the same way `Ctrl-X y`
copies: through your OS clipboard tool, or over SSH as an OSC 52 escape. Only
drawn text is selected, the way an editor highlights: each line stops at its last
character rather than running to the pane's edge, and a drag over empty space
alone selects nothing and leaves your clipboard as it was. To select more than
fits on screen, hold the drag just past the preview's top or bottom edge: the
transcript glides that way in small steps (about a screen a second, faster the
further past the edge you hold it) and the selection grows with it until you let
go — then all of it is copied, not just the part still on screen. The selection stays
on the same text while the pane moves; scrolling the wheel, pressing any key or
resizing the terminal ends it. A reply you are still sending is not selectable.
The status line briefly says `Copied selection (N lines)`, or `Sent selection …`
when it went out as OSC 52. To select across both panes, or if your terminal
ignores OSC 52, hold **Shift** (or **Option/⌥** on iTerm2 and macOS Terminal)
for a native selection instead. The
header shows the active scope, the search mode, and a
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
actually said or done in it — not just by what it was titled. Several words have
to turn up **near each other** in a transcript, not merely somewhere in the same
one: two words that drift thousands of lines apart are a coincidence, not
something you remember, and treating them as a match put most of the board on
screen for almost any pair. Common words are held to that same distance — an
everyday word turns up on nearly every page, so letting one off would put the
board back on screen. Paste a remembered snippet in and the search widens
to fit what you pasted, so a copied line still finds where it was said. A session
whose **name** carries every word matches however far apart the rest of it sits,
and a word in the name counts as near the transcript's opening lines.
The matched text
is highlighted in the list **and marked in the preview**, so a content hit shows
you where it was said instead of leaving you to scroll for it. In content mode
the preview also **scrolls itself onto the most recent match** as you type, as
you move between rows, and the moment `Tab` widens the search, and `Shift-↑` /
`Shift-↓` walk back and forth through the rest — so finding a hit costs no
scrolling at all. Content search reads what was **said** in a session — what you
typed and what came back — and not the instructions Claude Code added around it:
a slash command is found by its name and the arguments you gave it (`/cr-review
PR #157`), never by the skill text it expanded into, so a query that happens to
share a skill's wording no longer matches every session that ran that skill. It
does read some parts the preview folds away — an injected reminder, a slash
command's output, a subagent's report — so a hit occasionally lands somewhere the
pane cannot show it; the board says so rather than leaving you looking at an
unmarked pane.

The preview follows the newest turn of a session that is still being written —
until you position the pane yourself. Scroll it, jump to a match, or press `Home`
(or `Ctrl-T`), and it stays exactly where you left it — including if you scroll
back down onto the newest turn, which parks the pane there rather than
resubscribing it. `End` (or `Ctrl-E`) is how you hand it back. Until you do, only
selecting a row, typing, `Tab`, `Shift-↑` / `Shift-↓`, bringing the pane back
from the `1:0` layout, or a quick reply of your own finishing moves it, never an
autorefresh. Changing the layout between the other stops keeps your place: the
line at the top of the pane stays at the top at the new width.

**Autorefresh.** The list keeps itself current as you work: new sessions appear,
finished ones update, deleted ones drop out — all in place, with your selection
and scroll position preserved. It re-reads only the transcripts that actually
changed, so a board left open beside a busy agent costs close to nothing, however
many sessions you have accumulated. `Ctrl-X r` forces a full re-read if you ever
want one.

**Agent sessions at a glance.** Every session Claude Code is running — or has
recently finished running — as an agent carries a colored badge: a dot and a
short tag that share one color, so you can read the state of your agents straight
off the list. The one exception is a session waiting on you, whose dot becomes a
red `!` beside its yellow tag.

- **yellow, with a red `!`** — it **needs input**: stopped, waiting on you to
  answer.
- **green** — nothing is wanted from you: the session is either idle or finished.
  The word beside the badge says which.
- **gray, pulsing** — working right now, or reporting a state this version doesn't
  recognize (shown as busy rather than hidden behind a steady dot).
- **gray, steady** — **interrupted**: a background agent Claude Code still lists
  as working while its own status for it reads idle, so the badge holds still
  instead of pulsing as if a turn were in flight.
- **dim gray, steady** — the agent has ended: it was stopped or its run failed.
  The word beside the badge says which.

The short tag names the kind of agent. `bg` is a background agent Claude Code
runs as a job. `live` is a session Claude Code reports as running with **no
background job** behind it. What sits behind a `live` tag varies: it has been a
one-shot `claude -p` reply still in progress, and it has been an ordinary
interactive Claude Code session. So snapback tells you only what Claude Code
reports about it and never guesses where it is running. With no job, a `live`
session can't be attached to or stopped with `claude stop`; `Ctrl-K` in the
[key table](#keys) covers what the board can do instead.

The pulse is the tell for activity, and it is the *first* thing to read — not the
shade. Only a working (or unrecognized) badge pulses, once a second, and only its dot,
which fades between the bright and the dim gray rather than blinking out, so no
text on the row ever moves or redraws and a busy board doesn't flicker. That fade
passes through exactly the dim gray an ended agent wears, so a glance at the dot
alone can't tell a working agent from a finished one — but a working dot *moves*
and the other two hold still. Two things keep it unambiguous: the badge's short
tag never pulses, so it always shows the badge's real color; and once you can see
a dot is steady, the shade separates the two at rest — the working gray is the
interrupted one, the dimmer gray is a run that has ended. Colors follow your
terminal's theme.

Open the preview on any session — badged or not — and a row stays pinned above
the transcript, naming **the turn you are reading**: the marker of whichever turn
owns the line at the top of the viewport —
`● claude · @lead · Opus 5.5 · xhigh · 12:55` — so who
spoke, under which agent, on which model at what effort and when stay readable
long after that turn's own marker has scrolled off the top of a long answer. It
is the transcript's own marker line reused verbatim, down to the highlight your
search puts on it, never a second rendering that could drift from the line below.
It tracks the turn as you scroll, in every position the pane can be in, including
the bottom-anchored one it opens at; scroll to the very first turn and the pinned
row names that one, there being nothing above it to name. One exception to *who
spoke*: a message a subagent hands back is not a turn of its own and has no
marker, so while you read inside an expanded one the pinned row names the last
turn above it, not the subagent. That is usually the `● claude` turn that
delegated the work, but it is whichever turn precedes the message — a hand-back
that lands after later turns names the latest of them, and a peer message that
follows a `▶ you` turn names that turn. The `◆ message from …` line that opens
the message is what names its sender. The same goes for context Claude Code added
on your behalf, the `◇ added by claude code` line: it is not a turn either and has
no marker, so inside an opened one the pinned row names the turn above it. The
row steps aside only while something else holds the pane: a quick reply you sent
to that session is still in flight, when the reply's own turns take its place at
the bottom of the transcript, or a `Ctrl-N` draft has replaced the transcript
altogether. And it gives way to one
thing: a background task that session started and that failed. Until you next
write in the session, the pinned row quotes that failure instead of naming the turn
(see *When a job you sent off fails* below).

The session's *status* in words is on the list row instead, as the word beside the
badge. It reports what Claude Code reports, in Claude Code's own words, with two
exceptions. The two states that both mean *the session is waiting on you*
(`blocked` and `waiting`) are spelled out as `needs input`. And a background agent
Claude Code still calls `working` while its own status reads `idle` — the shape of
one that was interrupted and never cleaned up — is labelled `interrupted` (Claude
Code's own word) and held steady. Anything else is passed through as-is rather
than guessed at. That status reaches the pinned row on its own only when the
transcript has no turn to name at all — an empty one, or a session file that can
no longer be read — the one case the preview leads with words rather than with a
turn (a session without a badge has no status to show, so its row stays blank
then). Beside a turn, it appears only for an agent that is still running — one
Claude Code reports a process id for (below).

When Claude Code reports when the session started, the pinned row also says how
long ago that was, as of the board's last check. For a running agent the status
and that age ride after the turn marker on the same row —
`● claude · 10:00  ·  live busy · 46m` — so the turn you are reading and how long
the agent has been at it both stay in view; on a narrow pane the age is cut off
first, then the status, and the turn marker last. A session Claude Code reports no
process id for — typically one waiting on you, stopped, done or failed — pins just
the turn marker, even when its start time is known; so does a running agent with
no age to state. With no turn to name, the status carries the age on its own,
running or not: `live busy · 46m`. A reply stuck for most of an hour then looks
different from one that began a moment ago. The age counts from when the session
started, not from when it last changed state, and it stays off the status line at
the bottom, which is only for what your last keypress did. A failed background
task (below) still takes the whole row: no turn, status or age beside it.

Because a session that's still running can't be plain-resumed, pressing `Enter`
on one offers **Attach** (reconnect to a running background agent), **Fork**, or
**Cancel** — so a live agent is never a dead end. On a `live` session there is no
background job to attach to, so Attach says so and points you at Fork. `Ctrl-K`
can still offer to send its process a SIGTERM. A finished session resumes
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
behind it. Press `Ctrl-X` then `f` and they fan out underneath it, oldest work
included, no matter how far apart in time they landed; the same keys tidy them
away again. Each copy
says how many messages it holds — `6 msgs` next to `171 msgs` — so you can see
which one is a stub the hand-off left behind and which one holds the real work.
The names can't tell you that; they're identical.

Nothing is hidden from you and nothing is thrown away — every copy is still a
real session you can resume, and that matters: a session that's running in the
background can't be plain-resumed, so the older copy is often the one that
*will* open. It's one keypress away instead of lost in a row of twins.

**When a background copy loses its agent.** That same copying step has a bug in
Claude Code ([#80811](https://github.com/anthropics/claude-code/issues/80811)):
the new file sometimes arrives **without the agent the original was bound to**.
The job keeps its name, so nothing looks wrong — but it is no longer running as
the agent you picked.

snapback marks those rows `[unbound]`. It only says so when it can actually tell:
the row has to be a background copy that still carries a job name, while the
**original it was forked from** is right there in the same conversation still
carrying the binding. A background job that simply never had an agent is not
marked, because nothing was taken away from it.

It's a marker and nothing more — there's no key on it and it changes nothing. It
tells you why an agent-bound job may be behaving like a plain one, which is not
otherwise visible anywhere. Expect it to stop appearing once the upstream bug is
fixed.

**When a job you sent off fails.** If a background agent (or a background
command) that a session started fails — it stalled, or hit an API error — Claude
Code drops a short notice into that session and moves on. Nothing on the board
used to say so, so a failed job looked just like one still working.

snapback marks that session's row `[task failed]`, and its preview leads with a
line quoting Claude Code's own account, word for word, with the time it arrived.
That line takes the pinned row, in place of the turn you are reading, and stands
there on its own:

```text
background task failed at 2026-09-21 15:30: Agent "Remediate review findings" failed: Agent stalled: no progress for 600s (stream watchdog did not recover)
```

If the notice doesn't include an account, the session is still marked and the
line just says `background task failed at <time>`.

The mark stays until you next **write** in that session — a prompt you type, or a
`Ctrl-R` quick reply, skill commands such as `/cr-review` included (as a quick
reply, from Claude Code 2.1.278 on; older versions leave no sign it was you).
Other things don't count, because they aren't you: a later notice that some other
job finished, a message from another agent, or a built-in command such as `/exit`
typed on its own. That errs on the side of a mark you've already dealt with, never on
the side of a failure you haven't seen. Two consequences worth knowing: it will
also mark a failure Claude Code already explained in its own reply, and a
background copy of the session (see above) carries the mark over until its own
first prompt.

It's a marker and nothing more — no key, and it changes nothing. A job that was
stopped or killed isn't marked, only one that failed.

**Hand an agent a job and stay put.** `Ctrl-N` starts a fresh session in the
folder you launched from. If you keep Claude Code agents defined, it offers a
quick picker so the new session can start bound to one, and it remembers the last
agent you actually started so a repeat is just `Ctrl-N`, `Enter`, your message,
`Enter`.

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

The draft box has the same `/` and `@` pick list as a quick reply (see *Quick
reply without leaving the board* below), for the folder you launched from. Its
files and folders show at once; its skills, commands and agents appear once
Claude Code's list for that folder arrives. A reply can offer the agents its
transcript recorded in the meantime, but a session that does not exist yet has no
transcript. If Claude Code doesn't trust that folder, the list leaves out the
repository's own skills, commands and agents (why is below).

**Or take the terminal instead.** `Ctrl-O` runs the agent interactively, handing
you the terminal as usual. It works from the draft box (if you change your mind
mid-sentence, your draft comes along as the first turn) *and* straight from the
picker, where it skips the draft entirely. So both ways out are a single
keypress — the background one just happens to be the one `Enter` falls on now.
Before either, `Ctrl-L` in the draft box picks the model the new session starts
on; whichever key then starts it, that choice goes with it (see *Pick the model
for one message* below).

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
`Alt+Enter` for a newline, `Enter` to send, `Ctrl-L` to pick the model this one
message runs on — see *Pick the model for one message* below). Type `/` at the
very start to list the skills and commands Claude Code offers in the session's
folder — built-in ones such as `/compact` included, but not the ones Claude Code
hides from its own `/` menu — or `@` (at the start of a word) to list files and
folders under the session's working directory and, for a top-level `@`, the
folder's agents; picking one inserts `@agent-<name>`, the form Claude Code
documents for naming an agent in a message. Skills, commands and agents show
their description on the right, cut at the box's edge. The letters you type
narrow the list to what holds them anywhere in its name or, for a skill, command
or agent, its description, matched like the search box (an uppercase letter
matches exactly): names that start with them come first, then other name
matches, then description matches, with files and folders above agents; a file
or folder whose name starts with `.` shows only once you type its leading `.`.
While a list is open `↑`/`↓` choose,
`Enter` or `Tab` picks (a folder reopens the list one level down), and `Esc`
closes only the list — it never discards the draft; with no list open `Enter`
sends and `Esc` cancels as usual. The moment you
send, your message appears in the preview under a **you** turn, followed by a live **claude
cooking…** placeholder — so the exchange reads normally while the reply is still
in flight. The placeholder is replaced in place as `claude` writes the
real turns, and the status line reports what the reply cost (or the reason if it
fails). Confirmations and nudges fade after a few seconds; failures and refusals
stay until you press a key, so nothing is silently downgraded. You can reply to
several sessions at once, each tracked on its own, but not twice to the same one:
until a session's reply lands, `Ctrl-R` on that session is refused before a
compose box opens (so nothing you type is lost), while every other row can still
reply. That per-session record is also what lets the hard delete below keep
refusing a session while its reply is still being written.

The `/` and `@` list is Claude Code's own for that folder: the first time you open
a box there, snapback asks `claude` for it in the background — no message is sent
and nothing is written to your sessions — and keeps the answer until you quit.
Until it arrives (normally well under a second) `/` shows nothing yet, and a
reply's `@` offers the agents its own transcript recorded; files and folders
show at once. If `claude` could not answer, it stays that way and the next box
you open in that folder asks again. A command typed in full works without the
list, so a skill you create after a folder's list arrived still works when typed,
and is listed once you restart snapback.

In a folder Claude Code hasn't been told to trust (you never accepted its trust
prompt there, or in a folder whose trust covers it), the list shows your own,
bundled and built-in skills, commands and agents, but not the repository's own
`.claude/` ones. Listing those would mean starting Claude Code with that
repository's settings, which can run commands and set environment variables, just
because a box opened, so there snapback asks with your own settings only. Typed
in full, the repository's own skills still work. To see them listed, run `claude`
in the repository once and accept its trust prompt, then restart snapback.

Background agents get special handling, because `claude` won't resume a session
it's still holding as an agent. An agent whose run is **over** — `done`, or
`stopped`/`failed` — is stopped first (its conversation is kept) so the reply can
land in place. A **waiting** (`needs input`) agent asks you to confirm before it's
stopped, since that abandons an agent that's still live. Anything still live is
left alone and the reply is refused: `working`, `idle`, `interrupted`, or a state
this version doesn't recognize. So is a session with no background job to stop
first, such as a `live` one. The refusal suggests `Ctrl-K` to stop it or Fork
(`Ctrl-F`) to branch a copy; on a background agent, `Enter`'s **Attach** still
reconnects you to it. `interrupted` refuses on purpose even though it
sits still: that badge is snapback's *inference* from Claude Code contradicting
itself, not a report that the run ended, and it isn't worth stopping live work over
a guess. Use `Ctrl-K` if you do want it stopped — it will ask first.

**Hide, delete, copy & fold.** `Ctrl-X` is a leader chord that acts on the
selected row or on the whole board: press it, and a hint shows the follow-ups —
`x`, `d`, `h`, `r`, `y`, `f` — while any other key cancels.

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
- `Ctrl-X y` is **copy session ID**: it puts the selected session's full id —
  the one `claude -r <id>` takes, or a bug report wants — on your system
  clipboard, and shows it on the status line until your next key. On your own
  machine it goes through the OS clipboard tool (`pbcopy` on macOS; `wl-copy`,
  `xclip` or `xsel` on Linux), and the line reads `Copied session ID …`. Over
  SSH, or with no working tool, the id is sent to your terminal as an OSC 52
  escape instead, and the line says `Sent`, not `Copied`: some terminals (and
  tmux without `set -g set-clipboard on`) ignore that escape, and the id on the
  status line is still there to select by hand.
- `Ctrl-X f` **folds or expands** a `(+N)` stack: on the folded row it fans the
  look-alike copies out underneath, and on any row of an open stack it folds them
  back into one. On a row with no copies it does nothing.
- `Ctrl-X d` **hard-deletes** the selected session — physically removing its
  transcript from disk. Because that is irreversible, it asks first with a
  confirmation prompt (defaulted to Cancel). On a row that stands for a `(+N)`
  stack the prompt also offers **Delete lineage** — the whole family of look-alike
  copies at once, which is what hiding already does. Without it, deleting the top
  row would leave the copies behind and the next one would simply take its place,
  so the row never actually left the board. Deletion removes exactly each target
  session's own `<id>.jsonl` and its sibling `<id>/` directory of subagent
  transcripts — nothing else.

  What it refuses is a session something might be **writing**: a `live` one
  (whatever runs behind it, an interactive Claude Code session or a `claude -p`
  reply still in progress, can append to the transcript), a background agent
  Claude Code still has up — working
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

**Pick the model for one message.** In a reply box (`Ctrl-R`) or a new-session
draft (`Ctrl-N`), `Ctrl-L` opens a list of the models *your* Claude Code accepts.
`Enter` sets the highlighted one for that box alone; `Esc` goes back to your text
with the box's previous choice untouched. The pick goes out with that one message
— the reply, or the session the draft starts, whether `Enter` runs it in the
background or `Ctrl-O` interactively — and is gone when the box closes: every new
box starts back at its default, and snapback never writes a pick down.

Nothing else ever asks for a model. `Enter` resume and `Ctrl-F` fork send no
`--model` at all, because a session already has one: when Claude Code resumes a
session it normally restores the model that session last answered with (the
exceptions are listed below). So a session carries on with its own model — and
after a reply you picked a model for, it carries on with that one, because that
is the model that answered last. Attach sends none either: it joins a process
that is already running under a model. The agent picker's own `Ctrl-O` skips the
box, so there is no pick to send, and Claude Code chooses the new session's model.

The box's bottom border always says what snapback expects the message to run on:

- A reply with nothing picked says `model: session (Opus 5.5)` — the model the
  session last answered with, spelled the way the preview's turn markers spell
  it. It says plain `model: default` instead when Claude Code would not restore
  that model — `ANTHROPIC_MODEL` or an `ANTHROPIC_DEFAULT_*_MODEL` (`FABLE`,
  `OPUS`, `SONNET` or `HAIKU`) is set, in your environment or in a settings
  file's `env` block — or when the session has no answering model on record.
- A draft with nothing picked says `model: default (opus[1m]) (new sessions only)`
  when your Claude Code settings name a model, and plain `model: default` when
  they don't. The *new sessions only* part is there because that settings value
  decides what a new session starts on; a resumed one normally keeps its own.
- A pick says `model: opus`, or `model: opus · high` with an effort.

The picker's first row is that same default, spelled out — `session's model
(Opus 5.5)` in a reply box, `default (opus[1m]) (settings)` or `default` in a
draft — with a line or two on why. The settings value is read the way Claude Code
reads it — managed settings, then the launch folder's
`.claude/settings.local.json` and `.claude/settings.json`, then your own
`~/.claude/settings.json` (or `$CLAUDE_CONFIG_DIR/settings.json`), with
`ANTHROPIC_MODEL` beating them all — and read again every time you come back from
a Claude session, so a `/model` pick Claude Code saved as your default shows up in
the next draft.

The label is snapback's best reading, and it is display only: with nothing picked
snapback sends no `--model` and Claude Code decides, so a wrong label never
changes what runs. The cases it is known to get wrong include:

- A session model Claude Code refuses to restore (a retired one, say) still reads
  `session (…)` while Claude Code warns and uses another.
- A provider that doesn't use Anthropic's own model ids (Bedrock, Vertex, Foundry)
  is not modelled.
- A reply to a session bound to an agent whose definition names its own `model:`
  still reads `session (…)`, but Claude Code runs the agent's model instead.
- With `opusplan` or `haiku` as your settings' model, Claude Code keeps that
  instead of restoring a session model that suits it (Opus or Sonnet for
  `opusplan`, Haiku or Sonnet for `haiku`), while the reply still reads
  `session (…)`.
- Project settings are read from the folder you launched snapback in, so a reply
  to a session in another project misses that project's `env` block, and is
  judged by the launch folder's instead.
- A `Ctrl-N` draft for an agent whose definition names its own `model:` starts on
  that model, while the box names your settings' default.
- Settings that don't come from the files above aren't read, so a draft's default
  can miss them: managed settings pushed by MDM or a server, the `env` block in
  `~/.claude.json`, and the repository root's `.claude/settings.local.json` that
  Claude Code also reads when the launch folder isn't that root.

The picker's list of models isn't baked into snapback: it's read from the Claude
Code binary you have installed, in the background at startup. So a model that
arrives in a Claude Code update shows up in the picker too, without waiting for a
snapback release — and
one that goes away stops being offered. If snapback can't read it for any reason
it falls back to a small built-in list rather than showing you nothing. A draft's
pick wins over an agent's own `model:` if you started that agent with `Ctrl-N`; an
explicit choice you just made is treated as the later word.

`opusplan` is the one to notice, because `claude --help` doesn't list it: it runs
Opus while planning and the resting model the rest of the time, which is "plan
with Opus, implement with Sonnet" as a single choice.

**Pick the effort too.** In the picker, `←` / `→` step the highlighted model's
effort through `low`, `medium`, `high`, `xhigh` and `max`, wrapping round to
`default effort` — no `--effort` at all, so the level your Claude Code settings
keep for that model applies. The highlighted row shows where you are
(`fable · high`), and `Enter` sets the model and the effort together; the box
then reads `model: opus · high`, the same way each turn marker in the preview
shows the effort that turn ran at. It goes out right after the model
(`--model opus --effort high`) and never on its own: the first row has no model,
so the arrows do nothing there, and going back to the default clears the effort
with it. Each row keeps its own effort while the picker is open — move away and
back and it's as you left it — and `Esc` throws those changes away. The effort
counts for that one message only: Claude Code keeps a session's model but not its
effort, so a later resume runs at your settings' level for whatever model it
restores. A level the model can't use is quietly lowered by Claude Code (`max` or
`xhigh` become `high`), a model without effort support ignores it, and a
`CLAUDE_CODE_EFFORT_LEVEL` in your environment beats it.

**Readable transcript preview.** Beside the list sits a preview of the selected
session rendered as clean, scrollable markdown — the real conversation, whole,
from its first turn to its last, however long it ran. So you
can confirm it's the right session before jumping back in. Links show in light
blue, italic and underlined — the italic underline echoing how many terminals
mark a url they detect — and are clickable, except in a table too wide for a
narrow pane, which is stacked into `Header: value` lines where a link shows as
plain text, since it can't be clicked there.
A message handed back by a subagent is folded to a single `◆ message from @…`
line — it is that agent's report, not something you said — and context Claude
Code added on your behalf (the instructions a skill or slash command expands
into, a command's caveat) to a single `◇ added by claude code` line; either opens
where it sits on a click.
Dragging over the transcript selects and copies just the transcript text — hold
the drag past the pane's edge and it scrolls on, so the copy can span many
screens; double-clicking a word copies that word.
`Shift-←` / `Shift-→` step through five fixed layouts, from the preview filling
the screen to the list filling it, so a long transcript can take the whole width
and a long list of sessions can too.

Each `claude` turn is marked with **which model actually answered it**, beside
the agent and the time — `● claude · @lead · Opus 5.5 · xhigh · 12:55`. It's
read from the turn itself rather than assumed for the session, because a long
conversation really can change model partway through, and each turn is labelled
with its own. Turns that don't record one are left plain: about a fifth of
sessions record none, and a blank there is normal, not a gap. The marker also shows the effort level
the turn ran at (`xhigh` above), read from the turn the same way. When you
`Ctrl-R` reply, the status line names the answering model next to the cost
(`sent — $0.0136 (Sonnet 5)`) — so if the model that answered isn't the one you
asked for, you can see it rather than assume.
