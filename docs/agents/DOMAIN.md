# Domain: the Claude Code session store

`snapback` reads an **external, undocumented** on-disk format owned by Claude
Code. Getting this model right is the whole point of the data core; every rule
here is enforced by the pipeline in `src/store/` and its unit tests. Treat the
format as hostile input — see the fail-soft rule in [PATTERNS.md](PATTERNS.md).

## Store root

Resolved by `store::discover::store_root()`:

1. `$CLAUDE_PROJECTS_DIR` if set and non-empty, else
2. `~/.claude/projects`, else
3. `.claude/projects` (last resort if the home dir cannot be resolved).

## snapback-owned state (`src/hidden.rs`)

The store above is READ-ONLY to `snapback` with one exception (hard delete, below).
`snapback`'s one persistent write of its own is the **soft-hidden session id set**,
kept in a SEPARATE directory it owns — never inside the Claude store. Its path is
resolved by the `config` module (`src/config.rs`), the SINGLE place that reads the
environment for any snapback-owned path:

1. `config::config_dir()` = `$SNAPBACK_CONFIG_DIR` if set and non-empty (the
   test/override seam, mirroring `$CLAUDE_PROJECTS_DIR`), else `~/.config/snapback`.
   The default is `~/.config/snapback` on EVERY platform — deliberately NOT
   `dirs::config_dir()` (which is `~/Library/Application Support` on macOS) — so the
   state keeps one predictable, greppable home regardless of OS. Built from
   `dirs::home_dir()` joined with `.config`; home-less fallback is a relative
   `.config/snapback`, never a panic.
2. `config::state_dir()` = `config_dir()/state`, where persistent state lives.

The set is a single file, `<config>/state/hidden_sessions` (default
`~/.config/snapback/state/hidden_sessions`): newline-delimited session ids,
serialized in SORTED order for stable diffs. Reads are fail-soft (a missing file or
an unparseable line ⇒ that entry skipped, an empty set, never a panic); writes are
atomic (temp file + rename), matching the JSONL fail-soft discipline.

Hiding is a **visibility preference, not a status flag**. A hidden session is
still discovered, parsed, and indexed at load — its bytes stay on disk — and is
dropped only in `recompute_filtered` when `show_hidden` is off. Because the set
asserts nothing about run state, a hidden LIVE session keeps its live badge in the
show-hidden view, so it does NOT resurrect the rejected "Completed"/archived
status flag.

## On-disk layout

```
<store-root>/
  <encoded-cwd>/                      # one dir per project cwd
    <session-id>.jsonl                # ← a RESUMABLE session (depth 2)
    <session-id>/
      subagents/
        agent-*.jsonl                 # ← a SUBAGENT transcript (NOT a session)
    agent-title-xyz.jsonl             # ← a SIDECAR (no cwd, NOT a session)
```

`<encoded-cwd>` encodes the project path with `/`→`-`. **This encoding is lossy**
(real dashes are indistinguishable from separators), so it is never decoded to
reconstruct a path. Fixtures illustrating each shape live under
`tests/fixtures/store/`.

**The one write `snapback` makes into this tree itself** is hard delete
(`Ctrl-X d`, `delete::remove`): it unlinks a single session's own `<id>.jsonl` AND
removes its sibling `<id>/` directory (subagents included) when present — never any
other path, and only behind a confirmation modal plus the `can_delete_target`
WRITER guard.

A transcript can still GROW under snapback without snapback writing it: the quick
reply ([`Ctrl-R`](#quick-reply--non-interactive-send-srcsendrs)) appends an
exchange to the same `<id>.jsonl` because the `claude -p -r` CHILD appends it. The
distinction is the rule, not a technicality — snapback opens no session file for
writing, so the format's authorship stays entirely with Claude Code and no
half-written record can be snapback's doing. Everything else in this tree stays
read-only.

The confirm targets the selected session ALONE or its whole
[fork lineage](#fork-lineage-storelineage) — the same grouping the hide flips as
one unit, and for the same reason: deleting only a folded HEAD leaves its members
on disk and the fold simply re-heads to a surviving fork, so the row never leaves
the board. `delete::remove` is still strictly per-session; a lineage is a loop
over it, each member guarded on its own, with refused members skipped rather than
aborting the rest.

That grouping sweeps the FULL store, so a soft-HIDDEN member is counted in the
button's `(N)` and deleted with the rest — hiding is a visibility preference, not
a tombstone. The confirm therefore DISCLOSES the gap instead of narrowing the set:
the pure `tui::app::delete_confirm_message` leads the prompt with `N in this
lineage, M of them hidden` when `N > 1` **and** `M > 0`, so the count is
predictable BEFORE the confirm rather than only explicable after it. Both
conditions matter: a LONE session — hidden or not — has no lineage button, so
there is no `(N)` to be surprised by and the prompt stays exactly as it was.

The counts lead the sentence and the sentence leads the message because the modal
wraps to a constant width and clips each row's TAIL on a narrow terminal (the
button strip is worse — it is never wrapped at all, so the same counts in the label
push `Cancel` off the strip ten columns sooner). The sentence is kept terse for a
second reason: it is wrapped into a box whose height `centered_rect` clamps, so
each wrapped row it adds pushes the button strip off a short terminal one row
sooner. It costs exactly one row, and both prices are pinned by render tests in
`tui::view`.

The guard asks **"is anything writing this file?"**, not "does claude know this
session?". Those are different questions and conflating them was a defect: claude
appends by RE-OPENING the transcript path, so a PARKED background agent holds no
file descriptor on it and has no write to corrupt (measured on one machine: 72 of
74 active records were background and stopped, `lsof` showing zero open
transcripts). Bare membership therefore refused ~97% of the rows claude reports,
including ones idle for weeks. So an OPEN INTERACTIVE session is refused (its next
keystroke appends here) and a background agent is judged by its
[activity bucket](#activity-buckets-agentactivity): `NeedsInput`,
`WorkingButIdle`, `Done` and `Ended` are parked and deletable; `Working`, `Idle`
and `Other` are refused, the last of those because an unreadable qualifier must
not authorize an irreversible unlink.

Those are `can_delete`'s two refusals, and they cover only TWO of the three
writers. The third is snapback's OWN: a
[quick reply](#quick-reply--non-interactive-send-srcsendrs) `claude stop`s the
held job before it runs `claude -p -r <id>` — precisely so `-r` is accepted — so
for the whole span of a send the target is ABSENT from the active list
`can_delete` reads, while a `claude` child snapback spawned appends to that very
transcript. The probe cannot see that writer by construction, and the board stays
fully interactive during a send, so `Ctrl-X d` genuinely is reachable in the
window. What the confirm therefore calls is `delete::can_delete_target`: the
in-flight answer from `App::sending_to` FIRST, because it is the more specific
fact (mid-send the claude-side verdict is `Ok` by construction, so asking it
first would report nothing at all), then `can_delete`. Its refusal
(`DELETE_SENDING_REFUSAL`) names snapback rather than claude, since telling the
user to close a claude window would point at the wrong process. It stays a
COMPOSITION of two facts with two sources and two remedies, not a wider
`can_delete`.

The two FINISHED arms, `Done` and `Ended`, were once thought unreachable here: an
earlier note argued the guard reads the BARE list and that both sampled bare lists
held zero `done` records (across 37 entries and 74 — see the
[sampled distribution](#observed-value-distribution)), so a finished session would
arrive unreported and be allowed by the not-reported arm instead. That inference
from an ABSENCE was wrong. `claude` keeps a `done` background job in its ACTIVE
list for a while before reaping it, so the arm is LIVE rather than contingent —
and the old note's warning against deleting it as dead code stands vindicated.
`delete::can_delete`'s doc comment owns that correction and is the only place the
reasoning is written down.

What is left for a parked agent is **resurrection, not corruption** — attaching
and replying later re-creates the file with only the new lines — and the confirm
modal STATES that rather than refusing over it. The `pid` on the wire is
deliberately unused: it is absent on roughly half the records and does not
correlate with recent writes.

### The three file kinds

| Kind | Location | Has `cwd`? | Resumable? | How it is excluded |
| --- | --- | --- | --- | --- |
| **Session** | `<encoded-cwd>/<id>.jsonl` (depth 2) | yes | **yes** | — |
| **Subagent** | `<encoded-cwd>/<id>/subagents/agent-*.jsonl` (deeper) | yes (the *parent's*) | no | by **location** (depth), not by `cwd` |
| **Sidecar** | e.g. `agent-title-*.jsonl` at depth 2 | **no** | no | dropped when parse finds no `cwd` |

Subagents are the load-bearing hazard: historically ~62% of all `.jsonl` files,
and they carry the parent's `cwd` **and** `sessionId`, so a `cwd`-based guard
cannot tell them apart. Discovery therefore constrains by **depth**: it
enumerates only `.jsonl` files exactly one directory below the root and never
descends into `<session-id>/subagents/`. Do not relax this to a recursive walk.

## JSONL record model

Each line is one JSON object (parsed as `serde_json::Value`). Only a handful of
**stable** fields are relied on; everything else is ignored so schema drift is
never fatal:

| Field | Read as | Used for |
| --- | --- | --- |
| `cwd` | first non-null | authoritative working dir; **absence ⇒ not a session** |
| `sessionId` | first non-null (else file stem) | stable id, resume target, reported-agent join key |
| `gitBranch` | last non-null (`None` ⇒ `(detached)`) | branch grouping level |
| `timestamp` | last non-null, RFC 3339 | sort + display (per-message too, in preview) |
| `type` | `"summary"` / `"user"` / `"assistant"` / `"agent-setting"` / `"agent-name"` | label, preview, content index, [turn count](#turn-count-storeparse) |
| `summary` | on `type:"summary"` | preferred label + searchable text |
| `agentSetting` | on `type:"agent-setting"`, string (fail-soft) | the [bound agent](#bound-agent-storepreview) handle on preview turns (interactive bind — authoritative); read **positionally**, never hoisted |
| `agentName` | on `type:"agent-name"`, string (fail-soft) | the background job's name; a **fallback** bound-agent source for the preview handle, trusted ONLY when it names a known agent (the field also carries free-form titles) — see [bound agent](#bound-agent-storepreview) |
| `message.content` | string **or** typed-block array | user prompt, preview body, content index |
| `isSidechain` | bool | skip sub-agent turns when picking a label/preview |
| `uuid` | per record | a record's identity in the transcript **tree** |
| `parentUuid` | per record; **JSON `null` on the root** | the tree edge; the null-parent record's `uuid` is the fork-lineage identity (see [Fork lineage](#fork-lineage-storelineage)) |

"First non-null" vs "last non-null" is deliberate: identity fields take the
earliest value, activity fields (branch, timestamp) take the most recent.

`parentUuid` is the one field where **absent and `null` are different answers**,
and conflating them breaks the lineage — see [Fork lineage](#fork-lineage-storelineage).

## Derived concepts

### Label (`store::label`)

Preference order, then sanitized (tabs/newlines → spaces) and truncated to
`LABEL_MAX` (180) chars:

1. latest `type:"summary"` title (empty/whitespace summaries ignored), else
2. the first **real** user prompt — skipping `isSidechain` turns and
   `<...>`-wrapped command/system prompts; handles both string and typed-block
   `message.content`, else
3. the `session_id`.

An `ai-title`/`aiTitle` tier is deliberately **not** considered.

### Repo / branch grouping (`store::group`)

The module answers ONE question two ways. `repo_root_of(cwd)` returns the repo
root PATH a cwd hangs off; `repo_of(cwd)` LABELS that same root for a group
head. One marker scan, two consumers — a layout either counts for both or for
neither. The rules below are that scan:

- `*-worktrees[/...]` or `*.worktrees[/...]` → the text before the marker. The
  sibling-suffix layouts; the dot form also covers a `<root>/.worktrees/<branch>`
  container, whose children are branches rather than a nested segment.
- `<root>/.<tool>/worktrees[/...]` → the text before the hidden container. ONE
  generalized rule, not a list of tool names: a **hidden** (`.`-prefixed) segment
  whose IMMEDIATE child segment is `worktrees` marks a worktree container, and
  the repo root is the prefix before that hidden segment. It covers `wtp`'s
  default `.wtp/worktrees`, any user-configured `wtp` `base_dir` such as
  `.agents/worktrees` (this repo's own layout), and git's own `.git/worktrees`,
  without hard-coding a marker per tool — which matters because `base_dir` is
  user-configurable, so an enumeration of literals would miss real layouts.
  A path with nothing before the hidden segment has no root to label and falls
  through to the basename.
- otherwise → the cwd basename. In particular a **visible** `worktrees/` dir is
  an ordinary directory and deliberately does NOT collapse
  (`/Users/me/code/worktrees/thing` → `thing`): a bare `/worktrees/` substring is
  never matched on its own, because the false-positive risk is not worth it. The
  hidden parent is exactly what separates a container from such a directory.

All matched rules are evaluated together and the FIRST occurrence in the path
wins (the minimum match offset), so the result does not depend on rule order.
The winning prefix IS the repo root — what `repo_root_of` returns; a plain
checkout is its own root and comes back untouched. `repo_of` then renders that
root `<parent>/<base>` (the base dir alone is often ambiguous, e.g. `fe`) unless
the parent is empty or equal to the base — but only for a worktree cwd; a cwd
that IS its own root is labelled by its basename alone.

**That asymmetry is why membership must compare ROOTS, not labels.** A repo
(`snapback`) and its own worktree (`ilfroloff/snapback`) carry two different
labels for one project, so a label comparison says "different project" precisely
for the folders the project scope exists to unite.

**Accepted limitations,** both pinned by inline fixtures in `src/store/group.rs`:

- The hidden-container rule matches on shape alone, so any hidden dir with a
  `worktrees` child collapses — `<root>/.cache/worktrees/tmp` labels as
  `<parent>/<root>` even though it holds no worktree. The cost of the narrow
  alternative (missing a user-configured `base_dir`) is the higher one.
- First-occurrence means a marker INSIDE the repo wins over the container rule:
  `<root>/target/wtp-worktrees/<branch>` roots at `<root>/target/wtp`, so its
  sessions fall outside `<root>`'s project scope. Ranking the rules against each
  other would fix that one path and make every other path unpredictable, so the
  first-occurrence rule stands.

This is a **pure string heuristic** — it never runs git, and it is the only
worktree logic in the `store` core. The git-backed question ("which worktrees
does *this* project have right now?") belongs to `src/worktrees.rs` and the
[project scope](#user-facing-modes-tuiapp). That scope uses BOTH: git for the
live set, `repo_root_of` for the removed worktrees git can no longer name. A repo
whose worktrees match no marker here still aggregates correctly through the git
arm, for exactly as long as those worktrees exist.

The branch level comes from the authoritative `gitBranch` (missing ⇒
`(detached)`). Sessions sort repo↑ / branch↑ / timestamp↓; the list renders one
group head per repo→branch group, git-log style.

### Content index (`store::parse`)

An in-memory string of readable transcript text (user/assistant text blocks +
summaries; tool params/thinking omitted), extracted **once at load**; the
name+content search mode searches it without re-reading disk. It is bounded by
`CONTENT_INDEX_CAP` — a **safety ceiling** of 1 MB against a pathological file,
truncated on a UTF-8 char boundary — and not by a working budget.

That distinction is load-bearing, because the buffer keeps the **oldest** bytes:
whatever the bound cuts is the most RECENT work, the same end of the transcript
the [label](#label-storelabel) is already taken from. At its former 64 KB the cut
was routine rather than pathological (measured over 388 session files / 190 MB
raw, 2026-08-13): of 9.38 MB of readable text in the store only 7.97 MB was
indexed, **37 sessions (9.5%) were truncated**, and **275 typed prompts (15.3%)**
sat past the bound where no content search could reach them. The distribution
puts p50 at 12.7 KB and p90 at 63.8 KB — the old bound sat exactly on p90 —
against a p99 of 175 KB and an observed maximum of 252 KB, which 1 MB is ~4× of.

Raising it is affordable because total indexed bytes are bounded by the CONTENT,
never by `sessions × cap`. The haystack is held in **four** live copies, and a
re-derivation has to count all four: this field on the board (`App::sessions`),
its clone in the parse cache (`SessionStore::reload_at` clones the `Session`
INTO the cache on insert and back OUT of it on reuse, and `lib::run` keeps that
store alive beside the board for the whole process), plus `search`'s cased and
lowercased entries below. So indexing the store whole moved it from ~31.9 MB to
~37.5 MB: **+5.6 MB (+18%)** for the 15% of typed work that was previously
unfindable. Those are LENGTH-based figures and so a floor: a freshly parsed
buffer grows by doubling and `String::truncate` does not shrink capacity, so it
carries allocation slack until the next reload hands the board a tightly
allocated clone of the cached copy instead.

Per keystroke, `search` answers **membership only**: does every query atom occur
as a byte substring? `memchr::memmem` (SIMD, no allocation, no UTF-8 → UTF-32
conversion) decides it directly over the prebuilt haystacks. **nucleo is on no
runtime path at all**: it is a dev-dependency, and the same memmem finders answer
the row LABEL's highlight and the preview's marks.

The reason is that `App::order_filtered` imposes the timestamp/group order and
**discards any rank**, over a key (`(Reverse(timestamp), session_id)`) that is a
**total order with zero ties** (measured: 66 entries → 66 distinct keys). A rank
therefore provably could not reach the screen, yet producing one dominated the
keystroke: nucleo's `Utf32Str` conversion allocates a full `Vec<char>` for any
non-ASCII haystack at ~8.6 ns/byte, and **86% of entries are non-ASCII** once
`serde_json` decodes `\uXXXX` escapes (the raw files are pure ASCII — the decoded
`content_index` is what nucleo saw). So scoring every session's content
rebuilt megabytes of UTF-32 on the UI thread to order a list about to be
re-ordered by timestamp. Measured against the real corpus (66 entries, 1.29 MB of
haystack, 2026-07-17), dropping the rank stage moved the worst keystroke `n` from
**14.3 ms → 3.8 µs** and `the` from **13.5 ms → 5.3 µs**, with the whole-corpus
worst case (`zzzqqq`, nothing found, full scan) at **28.7 µs** — below the old
best case. Cost is now linear in haystack bytes with no per-entry conversion, so
the win grows with the corpus.

Each haystack is kept in **two** copies (bounded by the ceiling above) and both
are load-bearing: smart case is decided **per atom**, so an atom carrying an
uppercase char searches the *cased* copy case-sensitively while a lowercase atom
searches the *lowercased* copy. That branch is not an optimization — answering an
uppercase query from the lowercased copy is an inclusion regression (measured:
`NPX` finds 6 entries there, the smart-case rule matches 0).

The candidate set is scope-limited BEFORE any of this. The `App` pushes its
cached in-scope index set into the search pass (`SearchIndex::results_within`)
rather than filtering the whole corpus and intersecting afterward, so in the
default current-folder scope the per-keystroke pass scans only the in-scope
sessions. The index still holds every session (scope is a runtime cycle, and both
wider scopes — `Scope::Project` and `Scope::All` — reach sessions outside the
launch folder), so this narrows the work per keystroke without changing what is
indexed.

There is deliberately **no on-disk index** (YAGNI at the observed few-hundred
sessions — 388 when last measured, 2026-08-13): the whole content corpus is a few
tens of MB held in memory and the gate keeps per-keystroke matching instant, so a
SQLite/FTS index would be pure overhead. The mtime-keyed half of
that idea has since landed **in memory**, where it costs nothing to be wrong —
see [incremental reload](#incremental-reload-storesessionstore) — and it is what
keeps a reload from re-extracting every haystack. The remaining escalation is
unchanged and still not worth it: if the store ever grows into the **thousands**
of sessions, PERSIST that cache (e.g. `~/.cache/snapback/`) so a cold start pays
nothing either; an **FTS5** table over transcript text is the step past that.

#### A content-index position NEVER projects into preview coordinates

The content index and the rendered preview (`store::preview`) are two
INDEPENDENT, lossy extractions of the same transcript, and no offset function
maps one onto the other. They disagree in both directions: the index keeps
sidechain turns and the FULL body of every control wrapper, and drops markers,
timestamps and blank lines; the preview collapses each wrapper to a one-line
marker, drops sidechain user turns, discards a link's url, truncates table cells,
and keeps only the last `PREVIEW_LINES` rendered lines. The gap is wide, not
marginal: a one-off probe over the real store (2026-08-14, 336 sessions / 24,587
query × session hits) put **~17% of readable bytes inside collapsed wrappers
alone**. Read that as an upper bound and a rough one — the probe APPROXIMATED the
renderer (a regex wrapper collapse; no markdown inline stripping, table
truncation or link-url discard) and nothing re-measures it. It is dated evidence
that the gap is large, not a maintained metric.

So an in-preview search mark is derived by RE-SEARCHING the rendered lines —
never by projecting a byte or char offset out of `content_index`. Any such
mapping is a coincidence that holds on short sessions and silently marks
unrelated text on long ones. The cost stays bounded because the re-search is per
PANE (one session's ≤ `PREVIEW_LINES` lines, recomputed only when the query moves
off the cache entry's key), never per corpus — the boundary the per-keystroke
ranking pass was removed from.

The re-search goes through `SearchIndex::atom_match_positions`, the FILTER's own
memmem atoms, and marks PER ATOM. It has to: the filter admitted the session
because every atom occurs SOMEWHERE in the transcript, so a rule demanding all of
them in one rendered LINE marks nothing whenever the words sit on different lines
— and the pane then reads as broken while the board's own nudge claims the match
is elsewhere. The row LABEL's HIGHLIGHT is the other shape (one string, matched as
a whole) and `match_indices` applies the WHOLE-STRING rule over those same
finders — but that is a DRAWING seam, and any question about what a label
CONTAINS goes to the per-atom rule instead (see the nudge below). Two
consequences are normal and correct:
SEVERAL marked runs on one line, and marks as a UNION — overlapping or abutting
runs from different atoms merge into one span.

The pane's search NAVIGATION is per LINE regardless: `Shift-Up`/`Shift-Down` step
between marked LINES, not between occurrences, because a stop is a place to look
and a line is what the jump can scroll to. A line saying the query twice, or
carrying two different atoms, is marked twice and stopped at once.

The two pipelines also disagree about what EXISTS: a query can match the index
and occur nowhere in the rendered preview — the same one-off probe put it near
**one content hit in eight**, again an upper bound, mostly sessions past the
600-line tail cap and text inside collapsed wrappers. That is reported, not
hidden — the board says the match lies outside the previewed transcript, once per
(session, query), on the transient status line. It says so from the KEYPRESS that
changed the query or the selection, and only when NO rendered line holds ANY atom
AND the row LABEL holds none either: a multi-word query whose words landed on
different lines has marks on screen and explains itself, and a hit sitting in the
label is not outside anything. Both halves of that refusal are asked through
`atom_match_positions` — the label one deliberately NOT through the row
highlight, whose WHOLE-STRING rule wants every atom in the label and so came back
empty for a two-word query with one word in the label and the other in a
collapsed turn, making the board announce a match it was drawing one line away.
Raising `PREVIEW_LINES` is not
the fix; it trades a bounded render for a slightly smaller gap and leaves the
collapse cases untouched.

### Incremental reload (`store::SessionStore`)

The store re-reads only what moved. `SessionStore` holds an in-memory
`path -> (FileStamp, Session)` cache and, per reload, reuses the parse of every
discovered file whose `FileStamp` — `(mtime, len)`, compared as a PAIR — is
unchanged. Measured on a 403-file / 182 MB store (2026-08-03): a full parse costs
~0.43 s of CPU, a reload that reuses everything ~9 ms, and one appended
transcript re-reads exactly 1 file of 403. The watcher is recursive over the
whole store root, so before this every write by every `claude` process anywhere
— including the background agents `Ctrl-N` starts — re-parsed the entire store
up to 5×/second.

A second, complementary filter sits in front of this pipeline: the watcher
itself (`watch::SessionWatcher::spawn`) classifies every path in a debounce
batch against BOTH halves of the store's shape rule — `store::discover::store_depth`
for which level it sits at and `store::discover::is_session_path` for whether it
is a session — and calls into `reload` only when the batch is not provably
irrelevant. The classifier and its metadata matrix live in
[ARCHITECTURE.md](ARCHITECTURE.md#event-sources-watcheventloop) and are not
restated here; what matters for the cache is the consequence. A write to an
irrelevant
file elsewhere under the store root — a subagent transcript, a stray non-`.jsonl`
file — never reaches this cache at all, rather than reaching it and reusing every
stamp. An unclassifiable path always falls through to reload, so this filter can
only skip work, never miss a real change.

What the cache may and may not decide is a critical rule, and it is stated ONCE,
in `AGENTS.md` ("THE PARSE CACHE NEVER DECIDES WHICH FILES EXIST — OR WHETHER A
FILE IS A SESSION IT COULD NOT READ"). Read it there; it is not restated here.
What follows is the MECHANISM behind it, in five parts: how each half of that rule
is actually held up (two), the two stamp rules that live only here, and where the
cache lives.

- **How discovery stays uncached.** It runs in full on every reload, and the new
  cache is REBUILT from the discovered set rather than edited in place. So a
  created session is parsed the first reload that sees it and a deleted one
  leaves the board with its entry. The cache answers what a discovered file
  CONTAINS; it is never asked which files exist.
- **What counts as a verdict, and what does not.** A read that FINISHED has two
  possible answers, and both are durable facts about the bytes: this is a
  session, or it carries no `cwd` and so is not one (a sidecar — cached too, or
  every reload would re-read it). A read that FAILED is a third thing and is no
  answer about the file at all: `EMFILE`, a permissions blip, a network home
  directory that blinked. `parse::FileVerdict` keeps the three apart precisely so
  the failure cannot be stored. Stored, it would be re-served for as long as the
  file sits still — and a finished transcript's stamp never moves again, so one
  blip would cost that session for the LIFE OF THE PROCESS. Unstored, it costs
  this reload only and the next one reads the file again.

  A read that dies MID-FILE is the same distinction one level down, drawn the
  same way: non-UTF-8 bytes (`InvalidData`) are a fact about the content, so that
  line is skipped and the transcript around it survives, while an I/O error is
  not, so the file yields NO verdict rather than a truncated one whose
  `msg_count` / `timestamp` / `content_index` would be cached as the session's
  authoritative shape.
- **BOTH HALVES OF THE STAMP, TOGETHER.** An append moves both, but an in-place
  rewrite can move only the mtime and a same-instant truncate-and-rewrite only
  the length.
- **A FRESH MTIME IS NEVER TRUSTED, AND THE WINDOW IS SPENT ON THE WAY IN**
  (`MTIME_SETTLE_WINDOW`, 2 s). Filesystem timestamp granularity is coarse —
  HFS+ records whole seconds, SMB/FAT round to two — and the store may sit on any
  of them, so within one granule a file can be rewritten to the same length and
  still report the same mtime. A parse taken inside that window read bytes the
  stamp cannot vouch for, so it is DISCARDED rather than cached (`cacheable`,
  judged at the PARSE instant). Judging it at the REUSE instant instead reads
  identically and is unsound: mtime floors to 100, a reload at 100.3 parses and
  caches, a write at 100.7 to the same length leaves the mtime still floored at
  100, and a reload at 102.5 sees age 2.5 with a matching stamp and trusts bytes
  that were gone before it ever asked. Wall time alone must never promote a parse
  to trusted. The clocks compared are DIFFERENT clocks (the filesystem's,
  possibly a server's, against the local one), so skew is handled by failing
  toward a re-parse: an mtime in the future is never settled. Cost is bounded to
  the handful of transcripts being written right now, which is exactly where a
  cache is worth least.
- **Where it lives.** In memory: derived, disposable state, not snapback-owned
  state — see the [one file snapback
  writes](#snapback-owned-state-srchiddenrs).

A reload reports the ids it re-read (`Reload::changed`), which is a SUPERSET of
what really differs (a file inside the settle window is listed even if its bytes
did not move). Consumers evict derived caches from it, so over-reporting costs a
re-render while under-reporting would show stale text.

`Ctrl-X r` is the escape hatch: it drops the cache and re-reads everything, so a
filesystem that lies about either half of the stamp can never leave a row wrong
permanently. Nothing else clears it.

### Fork lineage (`store::lineage`)

> **Two different things are called "fork" here.** Keep them apart:
>
> | Sense | Who does it | What it is |
> | --- | --- | --- |
> | **Fork, the hand-off** | **snapback**, when the user presses `Ctrl-F` | `claude -r <id> --fork-session` — a deliberate, asked-for branch of a session. See [Hand-off invocations](#hand-off-invocations-srcresumers). |
> | **Fork, the event** | **Claude Code**, unasked | handing a prompt to a **background** job copies the transcript into a new session file. Nobody requested it and nothing on the board announced it. This section is about *this* one. |
>
> They are unrelated mechanisms that happen to share a verb. Where it matters,
> this doc says "the fork **hand-off**" for the first and "a **background
> fork**" for the second.

#### The mechanism (why identical rows appear)

When a prompt is handed to a **background** job, Claude Code **forks the
transcript**: it copies the foreground file's records **verbatim — identical
record `uuid`s** — into a **NEW `sessionId` file**, stamps that file
`sessionKind: "bg"`, and appends there. The foreground file **stops growing** at
the fork point.

Both files therefore share `cwd`, `gitBranch`, and the first user prompt — so
`label::finalize_label` derives the **same label** for both, and the board draws
two visually identical rows. A lineage grows by one file per background fork, so
a long-running conversation can occupy several look-alike rows.

**snapback is not mis-parsing this.** Verified against the real store: **zero**
duplicate `sessionId`s on disk, zero subagent leaks. Every file is a real,
distinct, **separately resumable** session. This is a presentation problem, not a
data problem — which is why the fold is presentation-only and drops nothing.

The twins are also **not redundant**, so they must never be hidden outright: the
bg copy is what makes `claude -r` refuse (the reason `src/agents.rs` exists), so
the **stalled ancestor is the only plain-resumable copy** of that conversation.
Hiding it irrecoverably would remove a real capability; hence a reversible fold
(`←`/`→`) rather than a filter.

#### `sessionKind`

A file-level field marking how the session runs; `"bg"` identifies the background
copy a fork produced. It is **deliberately not read** by any code path — not for
folding, not for filtering. The lineage is derived from the transcript **tree**
instead, because that is what makes two files provably the same conversation;
`sessionKind` only says what a file *is*, never what it is a copy *of*. Recorded
here because it is the field that explains the duplicate on disk, and the next
author will find it while looking.

#### The transcript is a TREE, not a message list

This is the model the lineage rests on, and the one most likely to be
mis-assumed:

- **Four record types carry `uuid` + `parentUuid`** and form the tree:
  `assistant`, `user`, `attachment`, `system`. Roughly **27% of the tree is not
  conversation**.
- **Types with no `uuid` sit outside the tree entirely** and must be ignored by
  lineage code: `last-prompt`, `mode`, `agent-setting`, `agent-name`,
  `permission-mode`, `file-history-snapshot`. They carry **no `parentUuid` key at
  all** — which is why absent must never be read as `null`.
- **It really branches.** The large majority of files contain a record with more
  than one child; only a minority are strictly linear. **Never assume a single
  chain.** (The lineage code only needs the root, so it never walks children —
  do not add code that assumes linearity.)
- **A file may be a forest** — more than one `parentUuid: null` record. Rare, but
  real; the first in file order wins, deterministically.

#### The root uuid is the lineage identity

`root_uuid` is the `uuid` of the record whose **`parentUuid` is JSON `null`** —
the tree's root — captured in `parse`'s existing streaming pass.

**It is NOT the first `user`/`assistant` uuid**, and that distinction is
load-bearing rather than pedantic:

- **The root is usually not a message.** The conversation typically starts two
  records deep, behind hook-injected context, so the null-parent record is an
  `attachment` in the large majority of files and a `user` in a minority. A tree
  rooted at an `attachment` looks wrong and is not — **do not "fix" it by
  filtering on `type`.**
- **The first-message key is anchored to FILE ORDER**, so it breaks whenever a
  fork's leading user record differs (an edited-and-resent prompt, a
  `<command-message>` turn ordered differently) even though the conversation is
  identical. Measured, it **missed three real forks**, one of which shared 133 of
  ~136 message uuids with its twin.
- The null-parent root is a **structural** anchor: it is copied verbatim into
  every fork, so nothing downstream can move it.

**No false-positive risk**: uuids are minted per record, so two genuinely fresh
sessions never share a root even when a hook injects byte-identical content. Only
a **copied prefix** can collide, and a copied prefix *is* a fork.

**FAIL-SOFT**: a file with no null-parent record yields `None`, which means "no
lineage" — never folded, never dropped. A degraded parse costs a fold, never a
session.

#### The lineage key is `(repo, branch, root)`

Not the root uuid alone. Some lineages span more than one `gitBranch` (none span
more than one `cwd`), and folding across branches would gather members across
branch group heads, breaking `build_rows`' invariant that same-group rows are
contiguous with exactly one head per group. Branch-scoping is also the right
semantic: **a fork onto another branch is different work**, and it keeps its own
row under its own branch's head.

#### What the fold does

Each collapsed lineage shows one **head** — its **newest** member, tie-broken by
`session_id` — wearing a `(+N)` marker for the members it stands in for. Newest,
rather than (say) the largest transcript, because the board already sorts
timestamp-desc and ranks groups by their MAX timestamp: a head chosen any other
way could carry a timestamp below its own lineage's max, and the folded row would
sort incoherently against the very rows it represents.

Expanding (`→`) **gathers** the other members immediately beneath their head
rather than leaving them at their own timestamp slots. That is deliberate, and it
is not what plain filtering does: time scatters a lineage — the bg head keeps
working while its stalled ancestor strands hours or days back, with unrelated
rows between — so an un-gathered member surfaces as an indented, label-less row
far from the head that explains it, which moves the "I can't tell these apart"
complaint rather than solving it.

A gathered member draws as an indented **child row** carrying only what actually
DIFFERS from its head: its own timestamp, its badge, the first 8 chars of its
`session_id`, and its [turn count](#turn-count-storeparse). The count is the one
field there carrying real information. A lineage's members share a label BY
CONSTRUCTION — that identity *is* the reported bug — so `6 msgs` beside
`171 msgs` is what says which member is a stalled stub and which holds the work;
a timestamp and an id only ever say WHICH member. The row **reports** and
predicts nothing about whether a member will plain-resume: that is the hand-off
probe's question, asked at hand-off (see [Why the gate does not read the `--all`
map](#why-the-gate-does-not-read-the---all-map)).

Folding is **content-derived and never liveness-gated**. That is deliberate and
easy to "improve" wrongly: the agents poll fires every `watch::AGENTS_REFRESH`
(5s while the board is active), so folding on whether a twin is live would
restructure the list on every poll, with rows appearing and vanishing under the
cursor.

`--print-list` prints **every** session, unfolded — it is a discovery/parse dump,
and folding it would hide exactly what it exists to verify.

#### Observed store shape

A **sampled observation dated 2026-07-15 on one machine — NOT a contract**, in the
same spirit as the agent distribution below. It records *provenance*: what the
lineage rules were built against.

Across **270** sessions: every file yielded a root uuid (270/270); the null-parent
record was an `attachment` in **209**, a `user` in **60**, a `system` in **1**;
**183** files branched (2,042 branch points store-wide), **87** were linear;
**2** were a forest; **zero** had a dangling `parentUuid` (one pointing outside
its file). Tree records by type: `assistant` 16,564, `user` 9,609, `attachment`
3,992, `system` 1,855. **28** lineages had more than one member before
branch-scoping; **19** survived it, folding **23** rows away by default.

**The store is live and it drifts** — treat every number above as a snapshot and
re-measure before relying on one. Observed during this work: four sessions
**vanished from disk mid-implementation** (269 → 265 total), and the cross-branch
root count moved from 8 to 5, with both of the originally named example roots
gone entirely. The **relationships** are what the code relies on and they are
structural: a background fork copies uuids verbatim; a root uuid never spans a
`cwd`. The counts are not.

That every file yields a root today is likewise a statistic, not a licence: the
`Option` and its never-folded fallback stay regardless, and are pinned by the
`sess-rootless-1` fixture rather than by the store.

### Turn count (`store::parse`)

`Session::msg_count` is how many **conversation turns** a transcript holds:
records typed `user` or `assistant`, counted in `parse`'s existing streaming
pass. Its consumer is the expanded lineage
[child row](#fork-lineage-storelineage).

**Turns are a NARROWER set than tree records, and the two are deliberately not
unified.** Four types carry `uuid` + `parentUuid` and form the tree
(`assistant`, `user`, `attachment`, `system`, [above](#the-transcript-is-a-tree-not-a-message-list)),
but roughly a quarter of it is not conversation: `attachment` context is injected
by hooks and `system` records are notices — nobody typed them and claude did not
answer them. Counting tree records would inflate a stub that holds no work into
something that looks like it does, which is the exact question this number exists
to answer. Do **not** collapse the two tests into one "tree record" predicate;
they are separate notions that happen to overlap.

**It is a real counter, never read off `content_index`.** That buffer stops at
`CONTENT_INDEX_CAP` ([above](#content-index-storeparse)), so a count derived from
it would silently stop at the ceiling and understate exactly the long sessions
most worth telling apart. The counter sees every record, so the ceiling cannot
reach it (pinned by `msg_count_keeps_counting_past_the_content_index_cap`, whose
fixture is sized OFF the ceiling so that retuning it cannot quietly leave the
fixture too small to prove anything).

**FAIL-SOFT**, like every field here: a file with nothing said in it counts 0 and
stays a session, and a missing or non-string `type` simply does not count rather
than panicking.

### Reported agents (`src/agents.rs`)

`claude -r <id>` refuses to plain-resume a session that is **currently running**
as an agent. `claude agents --json` (a TTY-free JSON array) is the only
machine-readable window onto that, and `snapback` reads it **twice, differently**
— one parser (`parse_agents_json`), two questions:

| Reading | Command | Asked | Question |
| --- | --- | --- | --- |
| **Board signal** (`reported_agents`) | `--json --all` | polled off-thread every `watch::AGENTS_REFRESH` (5s), skipped once the board has been idle past `watch::AGENTS_IDLE_AFTER` (60s) | "what should each row's badge say?" |
| **Hand-off signal** (`live_agents`) | `--json` (**no `--all`**) | one-shot at EVERY hand-off | "will `claude -r` refuse *right now*?" **and** "what job id does `claude attach`/`claude stop` take?" |

The hand-off reading serves FOUR gates, not just Enter: resume, Attach, the
`Ctrl-R` [reply gate](#quick-reply--non-interactive-send-srcsendrs) and the
`Ctrl-K` [interrupt gate](#interrupt--stopping-a-live-agent-ctrl-k-srcsendrs).
The last two also CLASSIFY the record (via `agents::classify`) rather than reading
membership alone — the only place a bucket informs an action rather than a pixel,
and it is still claude's own fresh answer, never the polled `--all` map.

The hand-off reading returns the **records**, not bare ids, so both of its
questions are answered by ONE authoritative read: liveness is membership, and the
attach target is the matched record's own `id`. The rule is uniform — **every
hand-off re-asks claude; nothing hands off on polled data.**

Both join to sessions by the **full** `sessionId`. Fields used: `kind`
(`background`→`bg`, `interactive`→`live`), `id` (the **short agent-view job id**,
e.g. `ca56b543` — distinct from the full `sessionId`; present only on
**background** agents, so an interactive session has nothing to attach to;
the authoritative target for Attach **when read from the `live_agents` probe** —
never from the `--all` map, which is a stale snapshot of a job that may have
ended), `state`/`status` (the activity qualifier, see below), `name`. Parsing is
fail-soft: any failure ⇒ empty map.

The board's shell-out passes **`--all`** because the bare command lists a finished
job only until claude reaps it from the active list (a just-`done` background job
lingers there for a while, then drops out): without the flag a finished session's
badge would vanish the moment claude reaps it, so `--all` is what keeps every
`done` session — reaped or not — reliably badged.

#### Why the gate does not read the `--all` map

**`--all`'s `state: "done"` means "the agent reported completion", NOT "claude
will permit `-r`"** — the two can disagree transiently, **claude is the only
authority**, and the gate therefore probes claude's active list at hand-off
rather than inferring from a polled snapshot.

That finding was paid for: the gate used to read the polled map and infer
`state != "done"` ⇒ live. It agrees in steady state (active = 37, `--all`-not-done
= 37, exactly), but it is a **guess about claude's gate**, and the snapshot is up
to **~5.3s stale** (a ~0.3s poll then a 5s sleep) while the board is active, and
unboundedly stale while the board has been idle past `AGENTS_IDLE_AFTER` (60s),
since the poll is skipped entirely until the next activity event. Claude
re-evaluates liveness at *spawn* time, so when the two disagreed the user
pressed Enter on a `● bg done` row and got claude's refusal instead of a
resume — a TOCTOU race.

The bare command needs no inference because **it IS claude's active list**:
membership is liveness, structurally. So the two readings are kept apart on
purpose, and the split is load-bearing in both directions:

- Adding `--all` to the probe would report every finished session as live and
  divert all ~123 of them into the overlay instead of resuming.
- Gating on the `--all` map's membership would do the same.
- Gating on its *bucket* is the race described above.

`live_agents` **fails soft toward "not live"** (empty map ⇒ plain resume), which
is the **opposite** direction from the display classifier's fail-toward-active
posture. That is deliberate: a classifier facing an unknown bucket should assume
"might be running", but membership has no bucket to be unsure about — the only
error left is "we could not ask", and claude's own check backstops that one step
later. Degrading toward *let claude decide* is correct.

At the **Attach** hand-off that same direction collapses two premises: an empty
answer means "the agent finished" and "we could not ask" alike. Both must refuse
(`resume::ATTACH_NOT_LIVE`) — without an authoritative id, `claude attach` would
be handed a dead or absent job — so the copy states what was **observed** ("claude
no longer reports this session as a running agent") rather than a cause the probe
cannot distinguish, and points at the routes valid either way (resume re-probes
and is backstopped by claude; a fork of a finished session is an ordinary fork).

The map is named for what it holds (`App::reported_agents`), so no identifier
claims liveness it cannot back. Its authority is **rendering, and nothing else**:
badges and the banner draw from it precisely because a render must never shell
out. The Attach **job id** was once read from it too — that was the same
stale-snapshot bug as gating liveness on it, one layer down, and worse: the
overlay can sit open indefinitely, so the staleness window is unbounded rather
than ~5.3s. It now comes from the `live_agents` probe taken at the hand-off.

#### Activity buckets (`AgentActivity`)

The `state`/`status` value set is **undocumented**, so it is interpreted in
exactly ONE place: `classify` buckets the resolved qualifier (`state`, else
`status` — `ReportedAgent::qualifier`'s precedence) into an `AgentActivity`.
Every qualifier-shaped output derives from that enum, so they cannot drift apart:

| Bucket | Qualifier(s) | Badge color | Badge glyph | Dot pulses | Banner / row reads |
| --- | --- | --- | --- | --- | --- |
| `NeedsInput` | `blocked`, `waiting` | `Yellow` (label/phrase) | `!` (`Red`) | no | `needs input` (translated — both tokens) |
| `Idle` | `idle` | `Green` | `●` | no | `idle` (verbatim) |
| `Working` | `working`, `busy` | `Gray` | `●` | **yes** (-> `DarkGray`) | verbatim (`working` / `busy`) |
| `WorkingButIdle` | `state`=`working`/`busy` **AND** `status`=`idle` | `Gray` | `●` | **no** | `interrupted` (**translated**, no wire token) |
| `Done` | `done` | `Green` | `●` | no | `done` (verbatim) |
| `Ended` | `stopped`, `failed` | `DarkGray` | `●` | no | verbatim (`stopped` / `failed`) |
| `Other` | anything else, or none | `Gray` | `●` | yes (-> `DarkGray`) | verbatim, or the kind label alone |

The **Badge glyph** column is a second, SHAPE channel on top of the color one:
`NeedsInput` marks its badge with `!` (via `tui::view::badge_glyph`, chosen by
bucket) instead of the `●` every other bucket draws, so the ONE row asking for
the user still reads as different in a monochrome terminal, or to a color-blind
reader, where the yellow-only signal does not. It is plain one-cell ASCII, so it
renders everywhere and shifts no layout. The glyph is bucket-derived and stable
across pulse phases — the pulse changes only COLOR, never the symbol (see below).

That `!` also **reddens**: `tui::view::badge_glyph_color` gives it `Red` while the
kind label and qualifier keep the bucket's `Yellow`, so only the single glyph cell
diverges (see the parenthetical in the **Badge color** / **Badge glyph** columns).
Red is an ACCENT layered on the shape channel — one steady cell, NOT a row-wide or
pulsing alarm, which the design avoids because nearly every active agent is
`blocked` and an alarm on all of them would cry wolf.

The **Banner / row reads** column is one phrase with two consumers: `classify`
feeds a single `agents::qualifier_copy`, so the preview banner
(`friendly_status`, kind label fused in) and the board **list row** speak the
SAME translated copy — the row no longer prints the raw token. Only the WEIGHT
differs, and that is a `tui::view` rendering call, not a bucket property:
`NeedsInput` draws its `needs input` at the badge's own color + `BOLD` (as loud
as the dot and kind label), every other bucket stays `DIM`.

**`WorkingButIdle` is the only bucket classified from the raw `state`/`status`
PAIR rather than the collapsed qualifier, and the only translated one with no
wire token behind it** (`NeedsInput` relabels tokens `claude` actually sends;
this one names a contradiction `claude` never spells out). When `claude`
interrupts a background agent it
does **not** reconcile the job: `claude agents --json` keeps reporting
`state=working` while that same record's `status` reads `idle`. `classify`
detects that self-contradiction **before** the qualifier collapse (which would
otherwise hide it as a plain `Working`) and buckets it here. It then renders the
working `Gray` but **STEADY** — the absent pulse, not a second color, is what
sets it apart — and translates the phrase to `interrupted` (there is no
`interrupted` token on the wire; the word names the contradiction, in `claude`'s
own vocabulary). Like every other bucket it never answers "live?" and never
gates resume/attach, which stay on `live_agents` membership — but it is **not**
display-only, and its two non-display consumers read it DELIBERATELY DIFFERENTLY.
At the `Ctrl-R`/`Ctrl-K` gates it is granted **no action of its own**: it rides
with the LIVE states rather than with `Ended`, so nothing is ever stopped on the
strength of the inference. At the hard-delete writer guard (`delete::can_delete`)
it is an ALLOW arm, because that gate asks the narrower "is a write in flight?"
and `claude` appends by re-opening the path — so retuning what lands here widens
an irreversible unlink and not just a steady dot. The internal name stays
descriptive (`WorkingButIdle`) precisely because the signal cannot prove the
*cause* the UI word implies; the accepted false-positive (a healthy agent briefly
at `working`/`idle`) self-heals to `Working`+pulse on the next poll once `status`
flips to `busy`.

**Every column above is a DISPLAY decision, and no bucket answers "live?".** The
table once carried a liveness column and it was the bug: liveness is not a
property of a qualifier, it is `live_agents`' membership answer straight from
claude (see above). Resume and attach ride that membership too, and nothing in
this table may widen them.

The BUCKET itself, though, is no longer display-only. It has exactly TWO
non-display consumers, and both classify only a record the probe JUST returned,
so even there liveness is membership and the bucket answers the narrower "what is
it doing right now":

* the hard-delete writer guard (`delete::can_delete`, see
  [the one write into this tree](#on-disk-layout)), which asks "is a WRITER
  present?" and allows `NeedsInput` / `WorkingButIdle` / `Done` / `Ended` while
  refusing `Working` / `Idle` / `Other` — two of the gate's THREE refusals, the
  third (snapback's own in-flight quick reply, added by
  `delete::can_delete_target`) reading no bucket at all. That gate is
  IRREVERSIBLE, so a change to `classify` is now weighed against it as well as
  against the badge it draws;
* the `Ctrl-R`/`Ctrl-K` stop routing (`send::reply_gate` /
  `send::interrupt_gate`), which is reversible by comparison — it ends a job but
  keeps the conversation.

ALL FOUR of those allow arms can fire: the BARE list the guard reads carries a
`done` job for the whole window before claude reaps it, so retuning `Done` moves
an irreversible unlink exactly as retuning `NeedsInput`, `WorkingButIdle` or
`Ended` does — never just a badge. Why that arm once read as unreachable is
settled at [the one write into this tree](#on-disk-layout) and is not re-argued
here.

A pulsing bucket alternates its dot between the badge color and the dim partner
shown above; the badge glyph (`●`, or `!` for `NeedsInput` — see the **Badge
glyph** column) is drawn in every phase, and a steady bucket simply holds the
badge color. The pulse is a COLOR change, never a glyph swap; the glyph a row
draws is chosen once by bucket and never varies with the phase — the rule and the
reason it is not negotiable live in
[PATTERNS.md](PATTERNS.md#7-restrained-terminal-safe-styling).

Two tokens per bucket is deliberate: `waiting`/`blocked` and `busy`/`working` are
one concept each under two spellings, and splitting them would give the same
agent a different badge depending on which token the wire used.

`Other` is the fail-soft posture made visible: an unknown qualifier is passed
through to the user rather than dropped or relabeled, and counts as ACTIVE, so
schema drift never hides a busy session behind a steady dot. The colors read as
urgency — yellow needs you, green is ready (idle or finished), gray is a working
base, dim gray is at rest.

**The PULSE — specifically its ABSENCE — is what marks activity, not the shade of
the dot at any one instant.** That distinction is load-bearing because
`view::BADGE_ENDED` and `view::BADGE_WORKING_DIM` are the SAME named color
(`DarkGray`): a `Working`/`Other` dot therefore sits at exactly the `Ended` shade
for half of every ~1s cycle, so a snapshot of the dot cannot tell a churning agent
from a dead job. What separates them is that one MOVES and the other holds. Two
things keep the row readable anyway, and neither is the dot's instantaneous color:

- **Only the dot pulses.** The kind label and the qualifier phrase always carry
  `badge_color`'s base (`render_list` styles them off `base`, never
  `pulse_color`), so a `Working` row's label stays `Gray` while an `Ended` row's
  reads `DarkGray` in every phase — a stable channel through the dot's off phase.
- **Among the STEADY dots, shade then separates the two resting gray buckets**:
  `WorkingButIdle` holds the working `Gray`, `Ended` holds the dim `DarkGray`.

So: gray dot that moves ⇒ churning (`Working`/`Other`); gray dot that holds ⇒
`WorkingButIdle`, the interrupted agent claude never reconciled; dim gray dot that
holds ⇒ `Ended`. Read the pulse first, the shade second — never the shade alone.

`Ended` is the counterpart to that default, and the two must not be conflated:
`stopped` and `failed` are KNOWN terminal tokens, so they are recognized and read
STEADY (the job is over, nothing is in flight to animate) and `DarkGray` (dim, and
deliberately not `Done`'s green — a stopped or failed job did not necessarily
finish cleanly). Their raw token still passes through verbatim. The fail-soft
ACTIVE default is PRESERVED for genuinely-unknown tokens — `Ended` only carves the
two real terminals out of `Other`, so a dead job stops pulsing as if live while
true schema drift still errs toward showing activity.

**`Ended` and `WorkingButIdle` are two DISTINCT resting buckets, never collapsed
into one.** Different causes: `Ended` is claude REPORTING a terminal token
outright, `WorkingButIdle` is a contradiction claude never reconciled. Different
colors: dim `DarkGray` versus the working `Gray`. Since both hold STEADY, shade is
what separates the two of THEM from each other — but only once the absent pulse has
already ruled out a churning agent, whose dot passes through that same `DarkGray`
every cycle (above). They also gate differently, and that is the sharper split:
`Ended` is stoppable evidence, `WorkingButIdle` is not (see
[Quick reply](#quick-reply--non-interactive-send-srcsendrs)). They are further
**disjoint by construction** —
`WorkingButIdle` requires `state`=`working`/`busy`, which `stopped`/`failed` can
never be — so neither can shadow the other whichever order `classify` tests them
in, and a `stopped` job whose `status` also reads `idle` still buckets as `Ended`.

#### Observed value distribution

**Sampled observations on one machine — NOT a contract.** The value set is
undocumented and may drift at any claude release; this records *provenance*, so the
next author knows what the buckets were built against and that each sample is a
snapshot, not a guarantee. Add a row here when a fresh probe is taken, rather than
citing it only from the code that relied on it.

Sample A, dated 2026-07-14:

| Command | Entries | `state` | `status` |
| --- | --- | --- | --- |
| `claude agents --json` | 37 | `blocked`×34, `working`×1, absent×2 | `idle`×19, `busy`×1, `waiting`×1, absent×16 |
| `claude agents --json --all` | 160 | `done`×123, `blocked`×34, `working`×1, absent×2 | — |

Sample B, dated 2026-07-28 — the bare list only, taken when the delete guard was
re-derived:

| Command | Entries | `state` | `status` |
| --- | --- | --- | --- |
| `claude agents --json` | 74 | `done`×0; 72 background `blocked`/`waiting`, 2 interactive | — |

Notes across both samples: `done` occurred **only** under `--all` — **zero**
occurrences in either bare list (37 entries, then 74) — which is the direct
evidence for the flag. It is NOT evidence that a bare list cannot carry `done`:
claude holds a finished job there until it reaps it, so both probes simply landed
outside that window. Reading the absence as a guarantee is what once had the
`Done` delete arm written up as unreachable, and it was wrong — sample a
distribution to learn what the values ARE, never to conclude which ones a gate
can never see. The token `running` does **not** exist in the observed domain — an
earlier revision guessed it, and the resulting dead match arm is why this table is
recorded here rather than inferred.

Sample A's 37 vs. 123 split is also why the two readings must stay apart: `--all`
carries **123** records the gate must never treat as live.

**Known, accepted risk:** `parse_agents_json` is **last-one-wins per
`sessionId`**. Sample A showed **zero** duplicate `sessionId`s across all 160
entries, but that is a sampled observation, not a structural guarantee. If it
ever broke and a `done` record overwrote an active one, the row's **badge** would
read `done` while the session ran. That is now cosmetic rather than behavioral:
the resume gate does not read this map, so Enter still probes claude and still
routes to Attach/Fork correctly. Deliberately **not** engineered around (YAGNI);
recorded so the failure mode is recognizable if it ever surfaces.

### Bound agent (`store::preview`)

A transcript records which agent it ran under, and the preview pane renders that
name as a DIM `@handle` on the `claude` turn marker (`● claude · @lead · 12:55`).
The name comes from **two** record types, because interactive and background
launches persist it differently:

| Record | Emitted by | Value | Trust |
| --- | --- | --- | --- |
| `{"type":"agent-setting","agentSetting":"<name>"}` | an **interactive** session bound to an agent | always a clean handle (a handful of values store-wide) | **authoritative** — rendered verbatim |
| `{"type":"agent-name","agentName":"<name>"}` | a **background** agent job | the job's display name — the agent handle when defaulted, but ALSO free-form titles ("Plan Node.js and Nest.js upgrade migration") | **fallback** — rendered only when it names a known agent |

`agent-setting` wins when both are present. `agent-name` is the majority signal on
long-running background sessions (which carry no `agent-setting` at all), so
ignoring it would leave exactly those sessions bare — but the field is shared with
free-form job titles, so it is trusted ONLY when the value matches a **defined
agent** (`App::agent_names`, discovered once from `~/.claude/agents/*.md`, passed
into `render`). A title therefore renders bare rather than as a bogus
`@handle`. Both fields are read fail-soft (`.and_then(Value::as_str)`); the catch-all
default `"claude"` and any blank name suppress the handle (the `● claude` marker
already says `claude`).

Attribution is **positional**, not hoisted: the agent (both records) is threaded
through the render loop as streaming state (exactly like the per-message day
rollover), so a turn is labeled with the agent in effect *at that point in the
file*. A late `agent-setting` — a real store shape, where the only record sits on
the last line of a session whose turns began far earlier — must therefore leave
those earlier turns bare. Reading it positionally is both more faithful and strictly
simpler than a file-level hoist (no second read, no new `Session` field), and
neither record carries readable prose, so they never enter the
[content index](#content-index-storeparse).

This is the third and last of **three distinct agent concepts** — keep them apart:

| Concept | Source | What it is |
| --- | --- | --- |
| **Live** (reported) agent | `claude agents --json` (`src/agents.rs`) | a **running process** — drives the board badge and the resume gate (see [Reported agents](#reported-agents-srcagentsrs)) |
| **Defined** agent | `~/.claude/agents/*.md` (`src/defined_agents.rs`) | an **on-disk definition** a new session can be launched under (`claude --agent <name>`, see [Hand-off invocations](#hand-off-invocations-srcresumers)) |
| **Bound** agent | `agent-setting` / `agent-name` records (`store::preview`) | the **agent a recorded session actually ran under** — a preview-only label, this section; the **Defined** set above is what validates the noisy `agent-name` source |

## User-facing modes (`tui::app`)

| Concept | Values | Meaning |
| --- | --- | --- |
| **Scope** | `CurrentFolder` (default) / `Project` / `All` | THREE concentric answers to "which sessions are mine right now", declared widest-last so the variant order is the cycle order. current-folder = sessions whose **canonical** `cwd` exactly equals the canonical launch dir; project = sessions whose `cwd` is EITHER a member of the launch project's live worktree set (`src/worktrees.rs`) OR under the same repo ROOT (see below — two arms, and the scope needs both); all = every session. `All` renders repo→branch group heads; `Project` renders branch groups under the ONE project label instead of per-folder repo labels (see below); `CurrentFolder` is the flat, head-less list, and it ALONE, because it is the only scope that cannot span more than one folder. Selected at launch by `--project`/`-p` or `--all`/`-a`, and flipped by `Ctrl-A` between the first two — `All` joins that key ONLY on a board launched with `-a`, which is the sole route to it (see below). |
| **Search mode** | `NameOnly` (default) / `NameAndContent` | which haystack the substring matcher scores; toggled by `Tab`. |
| **Show hidden** | off (default) / on | whether soft-hidden sessions appear (dimmed, marked `[hidden]`, live badge intact). Toggled by `Ctrl-X h`; a row is hidden/un-hidden by `Ctrl-X x`. The set persists — see [snapback-owned state](#snapback-owned-state-srchiddenrs). |
| **Forced rescan** | `Ctrl-X r` | not a mode: a one-shot that drops the store's parse cache and re-reads every transcript, reporting the count it landed on. The board autorefreshes and reuses unchanged files by itself, so this is the escape hatch for a row that looks stale — see [incremental reload](#incremental-reload-storesessionstore). |
| **Modal** | `Row` \| `List` layout in one `Option<Modal>` | the SINGLE type for a TITLED, choice-bearing overlay. `Enter` on a running session builds the `Attach` / `Fork` / `Cancel` choice (a `Row`); `Ctrl-N` with defined agents builds the agent picker (a `List`); `Ctrl-X d` builds the hard-delete confirm (a `Row`: `Delete this` / `Delete lineage (N)` — offered only for a real multi-member lineage, carrying the member ids resolved at OPEN time — / `Cancel`, default-highlighted on Cancel by that choice's position). Each choice carries a `ModalAction` tag the one confirm handler (`confirm_modal`) routes on. The plain Enter/Esc stop confirmations (`Ctrl-R`, `Ctrl-K`), the compose zone and the `Ctrl-X` chord are separate keyboard owners, NOT `Modal`s — see [PATTERNS.md](PATTERNS.md#10-keys-actions-outcomes). |

The current-folder scope is an **exact** canonical `cwd` match by design: a
repo's *other* worktree folders do not appear there, no matter how the paths
relate. That precision is deliberate and is NOT the only precise option — it is
the narrowest of three. `Ctrl-A` widens to the **project** scope, which is the
answer for a repo's other worktrees, and by default wraps straight back.

**The header counter counts LINEAGES, on BOTH sides, and it measures the
PROJECT rather than what the scope narrowed to.** Every number on that line —
numerator, denominator and the `· N hidden` segment — is a count of
CONVERSATIONS ([fork lineages](#fork-lineage-storelineage)),
never of session files. A lineage is ONE unit throughout: head plus the members
it folds away counts once. All three come from `App::session_counts`
(`count_lineages`), and the renderer does no counting of its own — pairing a
local `filtered.len()` with that call's denominator is exactly what once printed
`115 / 146` on a board that could draw at most 115 rows, a post-fold row count
over a file count.

The denominator is `App::population`: a cached, lineage-GROUPED set that holds
`Scope::Project` membership even while the board is in the default folder scope,
so `5 / 30 sessions` reads "5 conversations drawn, 30 in the project" — what a
`Ctrl-A` would open up. The store's own size, the denominator before that, was
the one number that answered nothing: in a worktree it advertised hundreds of
rows the board would never draw. `Scope::All` is the single exception, because a
board showing every repo on the machine is not about a project: there the
population IS the store. A lineage leaves the denominator for the trailing
segment only when EVERY member is soft-hidden — a partially hidden lineage still
draws a row, so it stays counted and discloses nothing — and the segment is
drawn only when N is non-zero; with show-hidden on the rows are back on the
board and back INSIDE the denominator, so it goes away rather than counting
visible rows twice. `total + hidden` is therefore always the number of lineages
in the population. Consequences worth keeping straight:

- **Numerator == denominator** in `Scope::Project` and `Scope::All` whenever the
  query is empty and show-hidden is off: the board is drawing exactly the
  population it counts. `Scope::CurrentFolder` deliberately does NOT reconcile,
  and must not be "fixed" to — its denominator is wider than its rows on
  purpose, and the gap is the advertisement. A query moves the numerator ALONE.
- **Fold state moves neither number.** Expanding a `(+N)` family re-emits its
  members into `filtered`, so the numerator re-groups that list rather than
  taking its length. This is load-bearing beyond the arithmetic: a fold-sensitive
  number would drift on its own, because `restore_selection` → `reveal_hidden`
  auto-expands on autorefresh whenever a background job appends to a transcript.
- The population, **and its grouping**, are rebuilt **only** in
  `App::recompute_scope`. Deciding membership canonicalizes every `cwd`, the work
  that may not sit on a keystroke or a frame; the grouping is pure and could live
  anywhere, but it allocates a `(repo, branch, root)` key per member over a set
  that under `All` is the whole store, and it changes only when the population
  does. The hidden split is NOT cached with them — it is derived per call from
  `hidden_ids` with hash lookups alone, which is what lets a hide, an un-hide or
  a show-hidden flip stay truthful without re-resolving a single path.
- **Deleting a folded lineage drops the header total by 1 while
  `status_for_delete` reports "3 deleted".** Accepted, and the two surfaces are
  answering different questions: the header agrees with the rows on screen, the
  status line agrees with the files on disk. Do not reconcile them by putting
  files back into the counter.

**`All` is the launch flag's alone** (`Scope::toggled`'s `all_enabled`
parameter, from `cli::Args::all_scope_enabled`). It is the whole store — every
session of every repo on the machine — so it is the widest and least often
wanted answer, and it used to sit MID-cycle where a stray `Ctrl-A` landed on it.
`--all`/`-a` therefore means two things: start there, AND keep it as the third
stop of the key. Three consequences to keep straight:

- The flag is **orthogonal to the last-flag-wins precedence** on the initial
  scope. `-a -p` starts in the project scope and can still reach all folders;
  `-p` alone starts in the same scope and cannot.
- There is deliberately **no in-board chord** for it, and adding one would undo
  this. `Scope::Project` now spans a project's whole history, deleted worktrees
  included, so the middle stop is what a wide press was usually reaching for.
- The empty-list advice is **derived from `Scope::toggled`, not restated**
  (`view::empty_list_message` takes the same flag), so an empty project stops
  offering "Press Ctrl-A to show all folders" on a board where that key cannot.

The project scope is the one scope that asks **git**, and it differs from the
other two in four ways worth keeping straight:

- **TWO membership arms, and it needs both.** `tui::app::in_scope` — a free
  function, not an `App` method, precisely because it reaches for nothing: the
  scope, the session, the launch dir and the worktree set are all PARAMETERS —
  says yes to a session whose `cwd` is EITHER of:
  - a member of the worktree roots `git worktree list --porcelain` reports for
    the launch dir (`src/worktrees.rs`), canonicalized with the same
    `resolve_dir` the exact-cwd test uses. Authoritative, and the only arm that
    can relate folders no string rule could — `git worktree add` accepts any
    path, so a live worktree may sit nowhere near the repo.
  - under the same repo ROOT (`worktrees::project_root`, over the pure
    [`group::repo_root_of`](#repo--branch-grouping-storegroup)). This arm exists
    because git reports what exists NOW: a REMOVED worktree's sessions match no
    live root, and were reachable only under `All`. Measured on this repo's own
    store, that was 47 of 149 project sessions — a third of the project's
    history, invisible in the view meant to show it.

  The two are compared as **root PATHS, never `repo_of` LABELS**. `repo_of`
  spells a plain checkout `<base>` and a worktree `<parent>/<base>`, so a repo
  and its own worktree carry different labels and a label comparison answers
  "different project" exactly where it matters most. Both live on the ONE marker
  scan in `store::group`: `repo_root_of` slices the path, `repo_of` labels what
  it sliced.
- **Autoreloaded, not a launch-time snapshot.** The git set is resolved once in
  `App::new` and RE-RESOLVED on every reload, inside `App::apply_sessions` — so a
  worktree created while the board is running joins the scope on the next
  refresh, with no restart, no polling, and no extra event source (a new worktree
  can only matter once its first session writes a JSONL, which the existing
  debounced watcher already turns into a reload). It is never resolved on the
  render path or on the `Ctrl-A` keystroke: a subprocess on a key press is
  exactly the blocking work that may not sit on the UI thread, so the scope
  predicate reads a CACHE.
- **Fail-soft toward the repo root, never toward "nothing".** An EMPTY worktree
  set means "could not resolve" — no `git` on `PATH`, a launch dir that is not a
  repository, a non-zero exit, non-UTF-8 output — and the root arm carries the
  scope on its own from there: narrower than git could make it (worktrees parked
  outside the root drop out), never "no session matches", so launching with `-p`
  outside any git repo is harmless. The header follows: `project:<label>` prefers
  the label git resolved for the whole set (taken from the MAIN worktree, which
  git lists first) and falls back to the repo ROOT's name
  (`worktrees::project_root_name`) — not the launch dir's, because a worktree
  folder is named after its BRANCH and the list is drawn from the whole project.
- **One project, one group head — unconditionally.** Membership alone does not
  aggregate a project on screen: `Session::repo` is `repo_of`'s label for the
  session's OWN folder, fixed at parse time, so heading groups by that field
  splits one project into two heads (`snapback` and `ilfroloff/snapback`) even
  with every session in scope. `Scope::Project` therefore heads every group with
  the project label instead (`tui::app::group_key`), leaving the branch level
  below it untouched: one head per branch, under one project. It cannot
  over-merge, because membership is already restricted to that project — every
  visible row belongs to it by construction. There is no unresolved case to
  exempt: the root arm has no git dependency, so a project-scoped list always
  spans folders, always draws grouped, and `App::project_head` is always `Some`.
  The override is `Scope::Project`'s ALONE: `Scope::All` shows many projects at
  once and has no single project to name, so it keeps the per-folder labels.
  `App::order_filtered` and `build_rows` share the one `group_key`, or the
  ordering would scatter a group the row builder then re-heads.

Selection is tracked by stable `session_id` so it survives an autorefresh reload.

### Terminal paste routing (`Event::Paste`)

`tui::init_terminal` enables **bracketed paste**, so the terminal delivers a
clipboard drop as ONE `crossterm::event::Event::Paste` carrying the whole string.
`update::handle_paste` routes it through the SAME precedence the key arm uses —
a partial enumeration here is a wrong one, so all six keyboard owners are stated:

| Owner (in precedence order) | What a paste does | Why |
| --- | --- | --- |
| **Modal** (`handle_modal_key`) | ignored | A fixed choice with no text field. Acting would pick an option the user did not choose; falling through would type into a query the overlay is covering. |
| **`Ctrl-X` chord** (`handle_chord_key`) | ignored, chord stays ARMED | The chord resolves on exactly one KEY, hit or miss. A paste carries no completion, and cancelling on one would silently disarm a chord whose hint is still on screen. |
| **Stop confirm** (`handle_stop_confirm_key`) | ignored | A plain Enter/Esc gate; a paste is neither, and must never stop an agent. |
| **Interrupt confirm** (`handle_interrupt_confirm_key`) | ignored | Same. |
| **Compose** (`App::is_composing`) | inserted at the caret, newlines intact (`compose::insert_paste` → `TextArea::insert_str`) | The fix: as keystrokes, the first embedded newline was a bare `Enter` = `ComposeAction::Send`. |
| **Board** | appended to the query, newlines flattened to spaces | The query is one line and `search::gate_atoms` splits it on spaces into substring atoms, so `foo\nbar` becomes exactly the `foo bar` the user could have typed. First-line-only would silently discard input. |

Two rules hold on every path, both in `update::accept_paste`: line endings
normalize to `\n` (`\r\n` collapses to one, a lone `\r` becomes one), and the text
is capped at `PASTE_MAX_CHARS`, counted in **chars** so truncation can never split
a UTF-8 codepoint. Over-long pastes **truncate** rather than being rejected — the
head still lands — and say so on the status line, so it is never silent.

`handle_paste` returns no `Outcome`. A paste is DATA and structurally cannot send,
resume, launch, or quit. There is deliberately **no `Ctrl-V` binding**: the paste
is the terminal's own, which keeps working over SSH and inside tmux where an
app-side clipboard read would not, and `Ctrl-V` stays the editor's page-down.

## Hand-off invocations (`src/resume.rs`)

These are the forks **snapback performs**, on request. They are not the
[background fork](#fork-lineage-storelineage) Claude Code performs on its own —
different mechanism, same verb.

| Action | argv |
| --- | --- |
| Resume | `claude -r <id>` (`<id>` = full `sessionId`) |
| Fork | `claude -r <id> --fork-session` (`<id>` = full `sessionId`) |
| Attach | `claude attach <job-id>` (one-shot reattach; `<job-id>` = the **short agent-view id** from `claude agents --json`, **not** the `sessionId`) |
| New session | `claude [--agent <name>] [<prompt>]` (interactive launch, no `-r` — mints its own id; started in `App::launch_dir` via `Ctrl-N`, optionally bound to a picked agent, and optionally opening on a drafted `<prompt>` — see [the background draft pane](#background-agent-draft-pane-ctrl-n)) |

`claude attach` matches the agent-view **job id** (the short id), not the full
`sessionId` — a full UUID exits 1 ("No job matching"). Only **background** agents
carry that id, so Attach applies to them; an **interactive** live session has no
job id and cannot be attached (the Attach choice refuses with a clear hint,
pointing at Fork or opening it in its own terminal). The short id comes straight
from claude's authoritative `id`; it is never derived by splitting the UUID.

Before any hand-off, `cwd` and `sessionId` are **re-read from inside the file**
(authoritative at hand-off time) and the `cwd` must still exist on disk;
otherwise the board surfaces a refusal (deleted worktrees are common) and stays
up. That gate covers resume AND fork, and it is what bounds the
[project scope's root arm](#user-facing-modes-tuiapp): a removed worktree's
sessions are back on the board, and are browsable, searchable, hideable and
deletable — but not resumable, because the directory they ran in is gone.
Attach still `chdir`s into that authoritative `cwd`, but its argv is keyed on
the agent-view job id rather than the re-read `sessionId`. **New session** is the
exception: it has no source file to re-read, so `resume::check_new` gates on the
existence of `App::launch_dir` itself and uses that dir as the authoritative
`cwd`. All four escalate to the same `Outcome::Resume` round trip.

A new session can also be **bound to a DEFINED agent** (`claude --agent <name>`).
These are DISTINCT from the live/running agents above: they are on-disk
definitions discovered fail-soft (`src/defined_agents.rs`) from Markdown files
with YAML frontmatter under `~/.claude/agents/*.md` (user) and
`<launch_dir>/.claude/agents/*.md` (project overrides user by `name`). The list
is a convenience — built-in/plugin agents are not files, so it is inherently
incomplete; the picker always offers a `default (no agent)` bare launch and never
blocks on it. `Ctrl-N` opens the picker only when at least one agent is
discovered (otherwise it opens the draft pane below directly, with no agent
bound), pre-highlighting the last-STARTED agent, which `App` remembers
**in-memory only** (never persisted).

### Background-agent draft pane (`Ctrl-N`)

Drafting is what a new session DEFAULTS to. `Enter` on the picker's highlighted
row — and `Ctrl-N` itself when no agents are defined — opens the compose editor as
a **draft pane** (`compose::ComposeTarget::NewBackgroundAgent`) rather than
launching, and from there:

| Key | argv | Route |
| --- | --- | --- |
| `Enter` | `claude [--agent <name>] --bg <prompt>` | `Outcome::BgLaunch` → `send::spawn_bg_launch` → one `AppEvent::BgLaunchFinished`. **No teardown** — the board stays up. |
| `Ctrl-O` | `claude [--agent <name>] [<prompt>]` | `Outcome::Resume` → the ordinary teardown round trip, via `resume::check_new`. |

`Enter` therefore lives in the [`send`](#quick-reply--non-interactive-send-srcsendrs)
family, not the hand-off one: `--bg` registers the agent and returns immediately
with no TTY, so tearing the board down for it would buy nothing. An empty draft
refuses `Enter` (a background agent with no prompt does nothing) but is fine for
`Ctrl-O`, which then launches bare — exactly what the picker's own `Ctrl-O` emits.

**The pane shows a PLACEHOLDER, not a transcript.** A draft opens
`App::draft: Option<NewSessionDraft>` alongside the compose editor, and while it
is set `view::draft_card` replaces the previewed transcript with a near-empty card
naming the agent, the launch dir, and the draft's keys. The two fields are
separate on purpose: the editor answers *what the keyboard does*, the draft
answers *what the pane shows*, and the view never reads `ComposeTarget` to decide
the second. Without it the compose box docked over whichever row was selected, so
a new-session draft read as a reply to an unrelated conversation — the DEFAULT
`Ctrl-N` path, and therefore the first thing a user saw. The card's emptiness is
the feature: the session does not exist yet, so anything conversation-shaped there
would be a fiction.

Replacing the transcript drags in two rules PATTERNS owns: the card suppresses the
pinned banner inside `view::preview_banner` (see
[PATTERNS.md §5](PATTERNS.md#5-selection-and-scroll-survive-reloads)) and a draft
counts in `App::overlay_active` (see
[PATTERNS.md §10](PATTERNS.md#10-keys-actions-outcomes)).

`App::open_compose` / `close_compose` / `dispatch_draft` are the ONLY writers of
the pair, so `Esc` (and every refusal) can never clear one and leave the other.
The first two move both fields together; the third is where they deliberately
part, and only there: `Enter` closes the editor and keeps the card, stamped with
the `launch_id` of the dispatch it now reports, because there is nothing left to
type but still no session to preview. That is the `App::sending` shape reused, not
a second mechanism: an identity set at dispatch, matched by a completion event
already on the channel, with no tick, thread, or event source added.

**A card that outlives its editor needs TWO bounds, because its event carries
neither.** `send::spawn_bg_launch` emits exactly one `AppEvent::BgLaunchFinished`,
spawn failures included — but "emitted once" answers neither "for which dispatch?"
nor "will it be delivered?", and the card is the only UI state that has to survive
long enough to care:

- **Which dispatch.** The card is still up when the result lands, but the SURFACE
  underneath may have moved on — the user can open a quick reply (`Ctrl-R`) or a
  second draft while `--bg` runs. So the event carries the `launch_id` back and
  `App::launching_draft` checks it, exactly as `App::sending_to` checks a session
  id before clearing an in-flight send. Without that check a completing launch
  closes whatever compose is open and discards a half-typed message.
- **Whether it arrives.** Delivery is bounded by the board session: `tui::run_inner`
  builds a new `EventLoop` per board and drops the old receiver, so a launch still
  running when the user hands off (`Enter`/`Ctrl-F` on a row stay routable — the
  editor is closed) reports into a dead channel while `lib::run` re-enters the board
  on the same `App`. `update::handle_event` therefore closes the compose surface on
  any outcome that ENDS the board session (`Outcome::ends_board_session`: `Quit` and
  every `Resume`), so the card can never strand the preview on a placeholder for
  sessions it has nothing to do with.

The picker's SECOND verb is that same `Ctrl-O`: it starts the highlighted agent
interactively AT ONCE, skipping the draft. Both verbs therefore stay ONE key at
the picker, which is what makes the default safe — neither flow pays a keystroke
for the other — and `Ctrl-O` reads as "open interactive claude" on the picker and
inside the draft alike. `Ctrl-B` is not bound anywhere.

**`last_new_agent` is written where a launch ACTUALLY happens**, never where a
draft merely opens, so a cancelled draft cannot rewrite it. That is three points:
the picker's `Ctrl-O` (before the existence gate, so it survives a refusal),
`compose::submit_bg_launch` (past the empty-buffer nudge — an empty draft is not a
launch), and `compose::open_interactive`. The memory means "the agent of the last
new session actually started", which is exactly what pre-highlights the next
`Ctrl-N`.

Two constraints shape the rest:

- **The prompt AUTO-SUBMITS as the first turn.** It rides as claude's trailing
  positional, which is the only mechanism the CLI offers — there is no pre-fill.
  See [CLAUDE_CLI.md](CLAUDE_CLI.md#the-trailing-positional-auto-submits-and-there-is-no-pre-fill)
  for the flags that were checked and rejected, and why every user-facing string
  says "run interactively" rather than promising a review.
- **Nothing about the launch is recorded but the agent name.** No virtual/pending
  row is created, and the short job id `--bg` reports is NOT reconciled back to a
  `sessionId` (it isn't one). The new agent reaches the board through the ordinary
  watcher → reload path, and the transcript's own `agent-setting` record — already
  rendered by `store::preview` — is what says which agent it is. The one exception
  is `last_new_agent` above, which is picker state, not a board row.

  The draft card is NOT a counter-example: it is a PANE, and it is the reason a
  row was never an option. `apply_sessions` replaces `sessions` wholesale, so a
  synthetic row dies on the next autorefresh; its empty `content_index` would
  drop it under any active query; `Ctrl-X x` on it would persist a fabricated id
  into the one file snapback owns; and `resume.rs` forbids deriving the short id
  from a `sessionId` anyway. A pane needs none of that — it holds no id, survives
  no reload, and disappears when the draft does.

The launch's honesty seam (`send::status_for_bg_launch`) is deliberately stricter
than the send's, because `--bg` can fail SILENTLY: an unrecognized `--agent` exits
**0**, warns on stderr, and starts the session without that agent. A zero exit with
a non-empty stderr is therefore reported as *started, but claude warned…* rather
than as a clean start — see
[CLAUDE_CLI.md](CLAUDE_CLI.md#--bg-can-fail-silently-on-a-zero-exit).

## Quick reply — non-interactive send (`src/send.rs`)

`Ctrl-R` sends a one-shot message to the selected session WITHOUT the teardown
hand-off above. `claude -p -r <id> --output-format json "<msg>"` resumes the
session non-interactively (its stdio is a pipe, no TTY), replays the full
context, **appends the exchange in place** to the same `<id>.jsonl` — same
`sessionId`, no new file — prints a JSON result (`is_error`, `total_cost_usd`,
`result`, …), and exits. Tools run and are recorded in-file just as in an
interactive turn. Because it needs no terminal, it runs on a detached thread while
the board stays up, and the reply renders through the ordinary `SessionWatcher` →
`SessionsChanged` → reload → preview path.

**Optimistic in-flight echo.** Because `claude -p` writes the user turn only after a
network round trip, the reload path alone would leave the preview showing stale
content for the first seconds after Send. So while the send is in flight (`App::sending`,
which carries the message and the session's turn count AT SEND TIME), the preview
appends two synthetic turns via `store::preview::pending_reply_turns`: the sent
message under a `▶ you` turn plus a live `● claude` **cooking…** placeholder,
and it FOLLOWS the bottom so both stay in view. The `▶ you` echo is
dropped the instant the real turn lands on disk — detected by the reloaded
`Session::msg_count` growing past `Sending::baseline_msg_count` — so the real turn
(styled identically) takes its place with no doubling; the placeholder stays until
`AppEvent::SendFinished` clears `App::sending`. The pinned status banner is SUPPRESSED
while a send is in flight (`view::preview_banner` returns `None`, keeping render and
the click hit-test agreeing on the geometry), since the inline turns replace it.

That echo is also why `App::status` never carries a transient `"sending…"`: an
in-flight send is true over an INTERVAL, so it renders on the pane that owns it
rather than on the keypress-scoped help line. The rule lives in
[AGENTS.md](../../AGENTS.md) and its instances in
[PATTERNS.md §11](PATTERNS.md#11-status-line-ownership); neither is restated
here. What this send puts on that line is the honest OUTCOME below.

The load-bearing constraint: **claude will not resume a session it is holding as a
live agent.** `claude -p -r <id>` on such a session exits non-zero, verbatim:

> `Error: Session <id> is currently running as a background agent (bg). Use claude`
> `agents to find and attach to it, or add --fork-session to branch off a copy.`

This holds in EVERY held state — `done` included: a just-finished background job
stays a registered agent (in the bare `agents::live_agents` list) until claude
reaps it, and claude refuses it exactly like a working one.

**The unlock: `claude stop <job-id>`** deregisters the job — "Its conversation is
kept" — after which `-p -r` resumes and appends **in place** (verified on the wire:
`stop` → the session leaves the bare list → the reply grows the same `<id>.jsonl`).
Stopping is only safe when nothing is running to interrupt, so `send::reply_gate`
decides from the agent's STATE (one-shot bare probe via `App::live_agent_now` —
never the polled `--all` map — classified by the one `agents::classify`), and
`Ctrl-R` in `tui::update` routes on it:

| Probe result | Bucket | `Ctrl-R` (`send::reply_gate`) |
| --- | --- | --- |
| claude is not holding the session | — | reply in place, no stop (compose opens) |
| held, but the record carries no stoppable job id (an **interactive** session) | — | refuse (`SEND_LIVE_REFUSED`) |
| `done` | `Done` | stop the ended job, then reply — straight to compose |
| `stopped`, `failed` | `Ended` | same: stop, then reply |
| `blocked`, `waiting` | `NeedsInput` | **confirm** (`App::pending_stop`, a small modal — stopping abandons a waiting agent), then stop + reply |
| `working`, `busy` | `Working` | refuse (`SEND_LIVE_REFUSED`) — Attach or Fork instead |
| `state`=`working`/`busy` **AND** `status`=`idle` | `WorkingButIdle` (reads `interrupted`) | refuse |
| `idle` | `Idle` | refuse |
| anything else, or no qualifier at all | `Other` | refuse |

The **job-id check runs BEFORE the bucket** and wins in every state: an agent
`claude stop` cannot address is unstoppable whatever it is doing, so even a `done`
interactive session refuses.

**The two STEADY buckets part ways here, and that split is the point.** `Ended`
takes the stop-then-reply path while `WorkingButIdle` refuses with the live states,
even though both badge at rest. The difference is EVIDENCE: `Ended` is claude
REPORTING a terminal token, so "the run is over" is claude's own answer, whereas
`WorkingButIdle` is snapback INFERRING it from a `state`/`status` contradiction
whose documented false positive is a healthy agent caught mid-flip. Acting on that
inference would stop live work on a guess, so the bucket stays display-only exactly
as [its own rules](#activity-buckets-agentactivity) promise. A badge that merely
LOOKS at rest is not licence to stop a job.

The stop step (`build_stop_argv`, the SHORT agent-view job id from the probe's
`ReportedAgent.id`) runs in `run_send` BEFORE the send, **best-effort**: if the job
was already reaped between the gate and the send, the stop fails but the reply still
lands; if the session really is still held, the reply's own error is what surfaces.
No permission flags are passed: a send inherits the user's existing settings.

**Report the send HONESTLY.** Because claude prints its refusal to **stderr** and
exits non-zero with an EMPTY stdout, a driver that nulls stderr and ignores the exit
code would map the empty stdout to the neutral `"sent"` — a false success over a
failed send (the exact bug that shipped first). So `run_send` captures stderr AND
honors the exit code (`status_for_output`): a clean exit maps the JSON payload
(cost / `is_error` / neutral) via `status_for_send`, while a non-zero exit surfaces
claude's own reason via `status_for_failed_send` (`send failed: <reason>`),
sanitized (ANSI/control stripped, one line, length-capped) so no raw escape reaches
the status line.

## Interrupt — stopping a live agent (`Ctrl-K`, `src/send.rs`)

`Ctrl-K` runs the SAME `claude stop <job-id>` the reply path uses as its unlock,
but as the whole point rather than a preparatory step: it ends the selected
session's live background job from the board (the conversation is kept; the job
registration drops). It is a one-shot on a detached thread like a send —
`Outcome::Interrupt` → `send::spawn_interrupt` → one `AppEvent::InterruptFinished`
— so the board never tears down.

`send::interrupt_gate` mirrors `reply_gate`'s shape over the same one-shot probe,
with the **opposite intent**: a reply must never interrupt live work, whereas an
interrupt exists to end it, so a `working` agent is a valid target here rather than
a refusal.

| Probe result | Bucket | `Ctrl-K` (`send::interrupt_gate`) |
| --- | --- | --- |
| claude is not holding the session | — | refuse (`INTERRUPT_NOT_LIVE`) — a transcript on disk is not a running process |
| held, but no stoppable job id (an **interactive** session) | — | refuse (`INTERRUPT_NO_JOB_ID`) — point at the terminal that owns it |
| `done` | `Done` | stop NOW, no confirmation (nothing is running to abandon) |
| `stopped`, `failed` | `Ended` | same: stop now |
| every other bucket — `Working`, `WorkingButIdle`, `NeedsInput`, `Idle`, `Other` | | **confirm** first (`App::pending_interrupt`, `view::render_interrupt_confirm`), then stop |

`WorkingButIdle` confirms rather than stopping outright for the same evidence gap
the reply gate turns on: the inferred rest cannot prove the run ended, and skipping
the guard would kill live work on that false positive with no way back. The confirm
costs one keypress.

`claude stop` acts on the GLOBAL background-job registry, so the child runs in
`App::launch_dir` — deliberately NOT a re-read of the session's own `cwd`, since a
deleted worktree must never block stopping its still-live job. That is the one
hand-off in the crate that does not re-read the authoritative `cwd`, and the reason
is that the job id, not the session's directory, is what identifies the target.
`status_for_stop` maps the result: a clean exit is the neutral `stopped`, a
non-zero one surfaces claude's own sanitized reason (`stop failed: <reason>`), so a
failed stop never reads as a successful one.

**The interrupt carries a dispatch identity.** `AppEvent::InterruptFinished` rides
back with the `session_id` of the row that dispatched it, and `App::interrupting`
stores that same id while the stop is in flight. The completion clears the guard
only when the ids match, exactly as `App::sending_to` guards a quick reply and
`App::launching_draft` guards a background launch. `claude stop` is a fast registry
operation and the row badge clears on the next agents poll, so there is no
dedicated `"stopping…"` label; the guard exists solely so a stale completion cannot
land on a surface that has moved on.
