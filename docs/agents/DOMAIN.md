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
status flag (see the ADR note in the delete plan).

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

**The one write into this tree** is hard delete (`Ctrl-X d`, `delete::remove`):
it unlinks a single session's own `<id>.jsonl` AND removes its sibling `<id>/`
directory (subagents included) when present — never any other path, and only
behind a confirmation modal plus the `can_delete` live guard. Everything else in
this tree stays read-only.

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

`repo_of(cwd)` derives a repo label from the raw path string:

- `*-worktrees[/...]` or `*.worktrees[/...]` → the text before the marker,
  rendered `<parent>/<base>` (the base dir alone is often ambiguous, e.g. `fe`).
- otherwise → the cwd basename.

The branch level comes from the authoritative `gitBranch` (missing ⇒
`(detached)`). Sessions sort repo↑ / branch↑ / timestamp↓; the list renders one
group head per repo→branch group, git-log style.

### Content index (`store::parse`)

A capped (`CONTENT_INDEX_CAP` = 64 KB) in-memory string of readable transcript
text (user/assistant text blocks + summaries; tool params/thinking omitted),
truncated on a UTF-8 char boundary. Extracted **once at load**; the name+content
search mode searches it without re-reading disk.

Per keystroke, `search` answers **membership only**: does every query atom occur
as a byte substring? `memchr::memmem` (SIMD, no allocation, no UTF-8 → UTF-32
conversion) decides it directly over the prebuilt haystacks, and **nucleo is not
on this path** — it backs the highlight seam alone.

The reason is that `App::order_filtered` imposes the timestamp/group order and
**discards any rank**, over a key (`(Reverse(timestamp), session_id)`) that is a
**total order with zero ties** (measured: 66 entries → 66 distinct keys). A rank
therefore provably could not reach the screen, yet producing one dominated the
keystroke: nucleo's `Utf32Str` conversion allocates a full `Vec<char>` for any
non-ASCII haystack at ~8.6 ns/byte, and **86% of entries are non-ASCII** once
`serde_json` decodes `\uXXXX` escapes (the raw files are pure ASCII — the decoded
`content_index` is what nucleo saw). So scoring every session's 64 KB of content
rebuilt megabytes of UTF-32 on the UI thread to order a list about to be
re-ordered by timestamp. Measured against the real corpus (66 entries, 1.29 MB of
haystack, 2026-07-17), dropping the rank stage moved the worst keystroke `n` from
**14.3 ms → 3.8 µs** and `the` from **13.5 ms → 5.3 µs**, with the whole-corpus
worst case (`zzzqqq`, nothing found, full scan) at **28.7 µs** — below the old
best case. Cost is now linear in haystack bytes with no per-entry conversion, so
the win grows with the corpus.

Each haystack is kept in **two** copies (bounded by the 64 KB cap) and both are
load-bearing: smart case is decided **per atom**, so an atom carrying an
uppercase char searches the *cased* copy case-sensitively while a lowercase atom
searches the *lowercased* copy. That branch is not an optimization — answering an
uppercase query from the lowercased copy is an inclusion regression (measured:
`NPX` finds 6 entries there, nucleo matches 0).

The candidate set is scope-limited BEFORE any of this. The `App` pushes its
cached in-scope index set into the search pass (`SearchIndex::results_within`)
rather than filtering the whole corpus and intersecting afterward, so in the
default current-folder scope the per-keystroke pass scans only the in-scope
sessions. The index still holds every session (scope is a runtime toggle and
`Scope::All` needs the full set), so this narrows the work per keystroke without
changing what is indexed.

There is deliberately **no on-disk index** (YAGNI at the observed few-hundred
sessions — ~270 when last measured, 2026-07-15): the whole content corpus is a
few single-digit MB held in memory and the gate keeps per-keystroke matching
instant, so a SQLite/FTS index would be pure overhead. If the store ever grows
into the **thousands** of sessions and the initial load or the content haystack
starts to feel heavy, the first step is a lazily-populated, **mtime-keyed
on-disk cache** (e.g. `~/.cache/snapback/`) of each session's `content_index`,
so only changed sessions are re-extracted; an **FTS5** table over transcript
text is the step past that. Until then it is not worth it.

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
easy to "improve" wrongly: the agents poll fires about once a second, so folding
on whether a twin is live would restructure the list once per second, with rows
appearing and vanishing under the cursor.

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
it would silently stop at ~64 KB and understate exactly the long sessions most
worth telling apart — a 200-turn file would report 64. The counter sees every
record, so the cap cannot reach it (pinned by
`msg_count_keeps_counting_past_the_content_index_cap`).

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
| **Board signal** (`reported_agents`) | `--json --all` | polled ~1s off-thread | "what should each row's badge say?" |
| **Hand-off signal** (`live_agents`) | `--json` (**no `--all`**) | one-shot at EVERY hand-off | "will `claude -r` refuse *right now*?" **and** "what job id does `claude attach` take?" |

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
to **~1.3s stale** (a ~0.26s poll then a 1s sleep). Claude re-evaluates liveness
at *spawn* time, so when the two disagreed the user pressed Enter on a
`● bg done` row and got claude's refusal instead of a resume — a TOCTOU race.

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
than ~1.3s. It now comes from the `live_agents` probe taken at the hand-off.

#### Activity buckets (`AgentActivity`)

The `state`/`status` value set is **undocumented**, so it is interpreted in
exactly ONE place: `classify` buckets the resolved qualifier (`state`, else
`status` — `ReportedAgent::qualifier`'s precedence) into an `AgentActivity`.
Every qualifier-shaped output derives from that enum, so they cannot drift apart:

| Bucket | Qualifier(s) | Badge color | Badge glyph | Dot pulses | Banner / row reads |
| --- | --- | --- | --- | --- | --- |
| `NeedsInput` | `blocked`, `waiting` | `Yellow` (label/phrase) | `!` (`Red`) | no | `needs input` (the ONE translated bucket — both tokens) |
| `Idle` | `idle` | `Green` | `●` | no | `idle` (verbatim) |
| `Working` | `working`, `busy` | `Gray` | `●` | **yes** (-> `DarkGray`) | verbatim (`working` / `busy`) |
| `Done` | `done` | `Green` | `●` | no | `done` (verbatim) |
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

**Every column is a DISPLAY decision — no bucket answers "live?".** The table
once carried that column and it was the bug: liveness is not a property of a
qualifier, it is `live_agents`' membership answer straight from claude (see
above).

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
urgency — yellow needs you, green is ready (idle or finished), gray is quietly
working — and the PULSE, not the color, is what marks activity.

#### Observed value distribution

A **sampled observation dated 2026-07-14 on one machine — NOT a contract.** The
value set is undocumented and may drift at any claude release; this records
*provenance*, so the next author knows what the buckets were built against and
that the sample is a snapshot, not a guarantee.

| Command | Entries | `state` | `status` |
| --- | --- | --- | --- |
| `claude agents --json` | 37 | `blocked`×34, `working`×1, absent×2 | `idle`×19, `busy`×1, `waiting`×1, absent×16 |
| `claude agents --json --all` | 160 | `done`×123, `blocked`×34, `working`×1, absent×2 | — |

Notes from that sample: `done` occurred **only** under `--all` (0 occurrences
without it), which is the direct evidence for the flag. The token `running` does
**not** exist in the observed domain — an earlier revision guessed it, and the
resulting dead match arm is why this table is recorded here rather than inferred.

The 37 vs. 123 split is also why the two readings must stay apart: `--all`
carries **123** records the gate must never treat as live.

**Known, accepted risk:** `parse_agents_json` is **last-one-wins per
`sessionId`**. The sample showed **zero** duplicate `sessionId`s across all 160
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
| **Scope** | `CurrentFolder` (default) / `All` | current-folder = sessions whose **canonical** `cwd` exactly equals the canonical launch dir; all = every session, grouped by folder. Toggled by `Ctrl-A` / `--all`. |
| **Search mode** | `NameOnly` (default) / `NameAndContent` | which haystack the substring matcher scores; toggled by `Tab`. |
| **Show hidden** | off (default) / on | whether soft-hidden sessions appear (dimmed, marked `[hidden]`, live badge intact). Toggled by `Ctrl-X h`; a row is hidden/un-hidden by `Ctrl-X x`. The set persists — see [snapback-owned state](#snapback-owned-state-srchiddenrs). |
| **Modal** | `Row` \| `List` layout in one `Option<Modal>` | the SINGLE overlay type. `Enter` on a running session builds the `Attach` / `Fork` / `Cancel` choice (a `Row`); `Ctrl-N` with defined agents builds the agent picker (a `List`); `Ctrl-X d` builds the hard-delete confirm (a `Row`). Each choice carries a `ModalAction` tag the one confirm handler (`confirm_modal`) routes on. |

The current-folder scope is an **exact** canonical `cwd` match by design: a
repo's *other* worktree folders do not appear until you switch to all-folders or
`cd` into them. Selection is tracked by stable `session_id` so it survives an
autorefresh reload.

## Hand-off invocations (`src/resume.rs`)

These are the forks **snapback performs**, on request. They are not the
[background fork](#fork-lineage-storelineage) Claude Code performs on its own —
different mechanism, same verb.

| Action | argv |
| --- | --- |
| Resume | `claude -r <id>` (`<id>` = full `sessionId`) |
| Fork | `claude -r <id> --fork-session` (`<id>` = full `sessionId`) |
| Attach | `claude attach <job-id>` (one-shot reattach; `<job-id>` = the **short agent-view id** from `claude agents --json`, **not** the `sessionId`) |
| New session | `claude [--agent <name>]` (bare interactive launch, no `-r` — mints its own id; started in `App::launch_dir` via `Ctrl-N`, optionally bound to a picked agent) |

`claude attach` matches the agent-view **job id** (the short id), not the full
`sessionId` — a full UUID exits 1 ("No job matching"). Only **background** agents
carry that id, so Attach applies to them; an **interactive** live session has no
job id and cannot be attached (the Attach choice refuses with a clear hint,
pointing at Fork or opening it in its own terminal). The short id comes straight
from claude's authoritative `id`; it is never derived by splitting the UUID.

Before any hand-off, `cwd` and `sessionId` are **re-read from inside the file**
(authoritative at hand-off time) and the `cwd` must still exist on disk;
otherwise the board surfaces a refusal (deleted worktrees are common) and stays
up. Attach still `chdir`s into that authoritative `cwd`, but its argv is keyed on
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
discovered (otherwise it launches bare `claude` directly), pre-highlighting the
last-picked agent, which `App` remembers **in-memory only** (never persisted).

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
message under a `▶ you` turn plus a live `● claude` **sending… / cooking…**
placeholder, and it FOLLOWS the bottom so both stay in view. The `▶ you` echo is
dropped the instant the real turn lands on disk — detected by the reloaded
`Session::msg_count` growing past `Sending::baseline_msg_count` — so the real turn
(styled identically) takes its place with no doubling; the placeholder stays until
`AppEvent::SendFinished` clears `App::sending`. The pinned status banner is SUPPRESSED
while a send is in flight (`view::preview_banner` returns `None`, keeping render and
the click hit-test agreeing on the geometry), since the inline turns replace it.

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
decides from the agent's STATE (one-shot bare probe, classified by
`agents::classify`), and `Ctrl-R` in `tui::update` routes on it:

| Live state | `Ctrl-R` |
| --- | --- |
| not held | reply in place (no stop) |
| `done` | stop the finished job, then reply — straight to compose |
| `needs input` | **confirm** (`App::pending_stop`, a small modal — stopping abandons a waiting agent), then stop + reply |
| `working` / `idle` / no job id | refuse (`SEND_LIVE_REFUSED`) — Attach or Fork instead |

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
