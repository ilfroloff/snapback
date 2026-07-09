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
| `sessionId` | first non-null (else file stem) | stable id, resume target, live-agent join key |
| `gitBranch` | last non-null (`None` ⇒ `(detached)`) | branch grouping level |
| `timestamp` | last non-null, RFC 3339 | sort + display (per-message too, in preview) |
| `type` | `"summary"` / `"user"` / `"assistant"` | label, preview, content index |
| `summary` | on `type:"summary"` | preferred label + searchable text |
| `message.content` | string **or** typed-block array | user prompt, preview body, content index |
| `isSidechain` | bool | skip sub-agent turns when picking a label/preview |

"First non-null" vs "last non-null" is deliberate: identity fields take the
earliest value, activity fields (branch, timestamp) take the most recent.

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
search mode scores against it per keystroke without re-reading disk.

There is deliberately **no on-disk index** (YAGNI at ~170 sessions): the whole
content corpus is a few single-digit MB held in memory and nucleo matches it per
keystroke instantly, so a SQLite/FTS index would be pure overhead. If the store
ever grows into the **thousands** of sessions and the initial load or the
content haystack starts to feel heavy, the first step is a lazily-populated,
**mtime-keyed on-disk cache** (e.g. `~/.cache/snapback/`) of each session's
`content_index`, so only changed sessions are re-extracted; an **FTS5** table
over transcript text is the step past that. Until then it is not worth it.

### Live agents (`src/agents.rs`)

`claude -r <id>` refuses to plain-resume a session that is **currently running**
as an agent. The authoritative "is this live now" signal is `claude agents
--json` (a TTY-free JSON array of active agents), joined to sessions by the
**full** `sessionId`. Fields used: `kind` (`background`→`bg`,
`interactive`→`live`), `id` (the **short agent-view job id**, e.g. `ca56b543` —
distinct from the full `sessionId`; present only on **background** agents and
the authoritative target for Attach), `state`/`status` (dim qualifier), `name`.
Parsing is fail-soft: any failure ⇒ empty live set ⇒ the board degrades to plain
behavior.

## User-facing modes (`tui::app`)

| Concept | Values | Meaning |
| --- | --- | --- |
| **Scope** | `CurrentFolder` (default) / `All` | current-folder = sessions whose **canonical** `cwd` exactly equals the canonical launch dir; all = every session, grouped by folder. Toggled by `Ctrl-A` / `--all`. |
| **Search mode** | `NameOnly` (default) / `NameAndContent` | which haystack the substring matcher scores; toggled by `Tab`. |
| **Live choice** | `Attach` / `Fork` / `Cancel` | the overlay shown when `Enter` lands on a running session. |

The current-folder scope is an **exact** canonical `cwd` match by design: a
repo's *other* worktree folders do not appear until you switch to all-folders or
`cd` into them. Selection is tracked by stable `session_id` so it survives an
autorefresh reload.

## Hand-off invocations (`src/resume.rs`)

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
