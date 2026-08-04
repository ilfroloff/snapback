# AGENTS.md — snapback

Guidance for AI coding agents. Read this before changing code. It is a strict
system prompt: keep it loaded, follow it exactly.

## Objective

`snapback` (alias `sb`) is a single self-contained Rust **ratatui TUI** that
browses, searches, and resumes **Claude Code** sessions stored as JSONL under
`~/.claude/projects/`, and — without leaving the board — quick-replies to
(`Ctrl-R`) or stops (`Ctrl-K`) the agents claude runs, and starts new ones in
the background (`Ctrl-N`, which drafts their first message). Ship changes that
keep the data core correct against a hostile, undocumented on-disk format and
keep the terminal safe across the resume round trip.

## Critical rules

Each rule names the file it governs; the mechanism and rationale are in
`docs/agents/*` (linked below). Do not restate these rules there — fix drift in
one place.

- **FAIL-SOFT parsing.** Parse JSONL as `serde_json::Value`, NEVER hard-typed
  deserialize structs. Skip bad lines/files; never panic on malformed input.
  Same for `claude agents --json` and for DEFINED-agent frontmatter (hand-parsed,
  no YAML crate). (`src/store/*`, `src/agents.rs`, `src/defined_agents.rs`)
- **AUTHORITATIVE-FROM-FILE.** Read `cwd`/`sessionId` from INSIDE the file,
  never decode the `<encoded-cwd>` folder name (the `/`→`-` encoding is lossy).
  Re-read them at hand-off time. (`src/store/parse.rs`, `src/resume.rs`)
- **SUBAGENT EXCLUSION BY DEPTH.** The only consumable session shape is
  `<root>/<cwd>/<id>.jsonl` at depth 2. The shared predicate
  `store::discover::is_session_path` decides this by name-shape alone and is
  used by BOTH discovery and the watcher filter; never duplicate the rule.
  NEVER descend into `<id>/subagents/`. Do not make discovery recursive.
  (`src/store/discover.rs`, `src/watch.rs`)
- **THE PARSE CACHE NEVER DECIDES WHICH FILES EXIST — OR WHETHER A FILE IS A
  SESSION IT COULD NOT READ.** `SessionStore` reuses the parse of a file whose
  `(mtime, len)` stamp did not move, so it may leave a row STALE; it must never
  be able to leave one MISSING, and only that asymmetry makes it acceptable at
  all. Two things hold that line and both are load-bearing: DISCOVERY is never
  cached, and only a VERDICT ABOUT CONTENT is — a read that finished, taken at an
  instant the stamp can vouch for. A failed read is not a verdict and is never
  stored; a stored one would outlive every retry, because a finished transcript's
  stamp never moves again. Keep the cache IN MEMORY (derived state, not the owned
  state below). Mechanism and the stamp rules:
  [DOMAIN.md](docs/agents/DOMAIN.md#incremental-reload-storesessionstore).
  (`src/store/mod.rs`)
- **SNAPBACK-OWNED STATE.** The ONLY persistent state `snapback` writes is the
  hidden-session id set. It lives under `$SNAPBACK_CONFIG_DIR` (default
  `~/.config/snapback`), specifically the `state/` subdir — resolved by the
  `config` module, the SINGLE place that reads the environment for any
  snapback-owned path — NEVER inside the read-only `~/.claude/projects` store.
  Read + write are FAIL-SOFT (a missing or garbage file ⇒ an empty set, never a
  panic) and the write is ATOMIC (temp file + rename). Hiding is a VISIBILITY
  preference, not a status flag. (`src/config.rs`; `src/hidden.rs`; the persist
  path in `src/tui/app.rs`)
- **STORE WRITES ARE GATED, AND ALL BUT ONE ARE DELEGATED.** The only mutation
  `snapback` itself performs on `~/.claude/projects` is hard delete (`Ctrl-X d`),
  behind BOTH a confirmation modal AND the pure `can_delete_target` WRITER guard.
  Every OTHER change to a transcript is made by a `claude` CHILD and must stay
  that way — a quick reply appends in place because `claude -p -r` writes it,
  NEVER because snapback edits a session file. Do not add a direct writer.
  That guard asks "is anything WRITING this file?", NEVER "does claude know this
  session?" — membership refused ~97% of reported rows for no safety gain. Refuse
  an OPEN INTERACTIVE session and a still-RUNNING background agent; ALLOW the
  parked ones, INCLUDING the reported-finished and terminal buckets (`Done`,
  `Ended`) whose run is over — `can_delete`'s doc comment owns why. An unreadable
  qualifier fails toward REFUSING. Express it over `AgentActivity` DIRECTLY —
  never reuse `agents::is_active`, a badge-pulse decision that must never widen an
  irreversible gate, and never widen it to a bucket the send gates treat as live
  without re-deriving the writer question. THREE writers, not two: claude's probe
  structurally CANNOT see snapback's OWN in-flight quick reply, because a send
  `claude stop`s the job on its way in, so the target is ABSENT from that active
  list for exactly the span a snapback-spawned child is appending to it. So the
  gate the confirm calls is `can_delete_target` — `can_delete` COMPOSED with
  `App::sending_to`, refusing that third case in its own words
  (`DELETE_SENDING_REFUSAL`) because the writer to name there is snapback, not
  claude; keep it a composition of two facts, never a wider `can_delete`.
  A confirm may target the selected id ALONE or its whole fork lineage
  (`lineage_member_ids`, the SAME grouping hide uses — never a second rule):
  guard each member individually, let one refusal
  skip only itself, and spend exactly ONE liveness probe for the whole set. That
  lineage sweeps the FULL store, so it takes soft-HIDDEN members too — keep that,
  and keep the confirm DISCLOSING how many of them are hidden. It removes ONLY
  each target id's own `<id>.jsonl` + sibling `<id>/` dir; everything else stays
  read-only. (`src/delete.rs`; `confirm_delete` in `src/tui/update.rs`;
  `src/send.rs`)
- **TERMINAL SAFETY.** Resume/fork/attach SPAWN `claude` as a child and RETURN
  to the board — never replace the process image. Restore the terminal (raw
  mode + alt screen + every mode snapback ENABLES — mouse capture and bracketed
  paste) on EVERY exit: quit, error, hand-off, and panic. A mode snapback turns on
  is owned end to end: enabled in `init_terminal`, disabled in `restore_terminal`
  AND the panic hook, and re-armed in `reassert_board_screen` after a child return.
  On EVERY return from a child, hard-reset the terminal — a deterministic
  full re-init onto a fresh screen — so a dirty hand-back (notably a Ctrl-Z that
  exits `claude` without restoring the terminal) repaints from a known-good state
  with no stale cells, native scrollback, leaked keyboard/input modes (notably a
  leftover kitty keyboard-protocol level), or leftover escape-parser corruption
  showing through, and without regressing the idempotent restore. The reset is
  ONE complete return-to-known-state, not one mode per bug, and stays WRITE-ONLY:
  never emit a cursor-position (DSR `CSI 6n`) query on the return path — it
  deadlocks on a dirty child's stdin. (`src/tui/mod.rs`, `src/resume.rs`)
- **NUCLEO ISOLATION.** Every `nucleo` AND `memchr` call stays in `src/search.rs`.
  Matching is SUBSTRING (`AtomKind::Substring`), not fuzzy. The FILTER answers
  MEMBERSHIP with `memchr::memmem` and never calls nucleo; smart case is decided
  **PER ATOM**, never per query, and each atom searches the cased or lowercased
  haystack accordingly — BOTH are load-bearing. nucleo is confined to the
  HIGHLIGHT seam (`match_indices`). Never rank the filter's results: display
  order is `App::order_filtered`'s alone. (`src/search.rs`)
- **STABLE-ID STATE.** Track selection by `session_id`, never list index, so it
  survives autorefresh reloads. (`src/tui/app.rs`)
- **OFF-UI-THREAD blocking work.** RECURRING shell-outs / FS watch / input run on
  their own threads and deliver `AppEvent`s; the render loop never blocks. The
  watcher filters each debounce batch through `is_session_path` before emitting
  `SessionsChanged`; the agents poller runs on `AGENTS_REFRESH` (5 s) and skips
  the shell-out once the board has been idle past `AGENTS_IDLE_AFTER` (60 s).
  TWO bounded one-shots are deliberate, documented
  exceptions (`PATTERNS.md` §6): the liveness probe at hand-off, and the
  worktree resolve at construction/reload. Both are argued at the call site,
  and NEITHER may move onto a keystroke or the render path — a scope toggle
  reads a cached set, it never resolves. (`src/watch.rs`, `src/worktrees.rs`)
- **PURE, GIT-FREE STORE CORE.** `src/store/*` decides everything from the bytes
  it was given: `repo_of`'s worktree collapse is a pure string heuristic, and NO
  module under `src/store/` may shell out (to `git` or anything else) or read
  ambient state to answer a per-session question — it does not scale past a
  handful of sessions and it is not fail-soft. The ONE `git` shell-out lives in
  `src/worktrees.rs`, outside the store, where it runs ONCE per launch/reload for
  the launch dir alone; that placement is the whole reason the module exists.
  Keep the dependency one-way: `tui` -> `worktrees` -> `store::group`.
  (`src/store/*`, `src/worktrees.rs`)
- **TERMINAL-SAFE STYLING.** Style with ratatui `Style`/`Modifier` + NAMED ANSI
  colors only. NEVER embed ANSI escapes or hardcode RGB. (`src/store/preview.rs`,
  `src/tui/view.rs`)
- **NARROW `#[allow(dead_code)]`.** Binary-crate lint quirk: attach it to the
  single item with a reason. NEVER a crate/module-wide blanket. (`src/search.rs`,
  `src/watch.rs`, `src/worktrees.rs`)
- **KEEP KEY DOCS IN SYNC.** A key/flag change must update the table in
  `update.rs`, `USAGE`/`KEYS` in `cli.rs`, the help line in `view.rs`, and the
  README key map together. This is the ONE list of those surfaces; the other docs
  point here. It binds ROUTING too, not just bindings: when a key's gate gains a
  case (a new `AgentActivity` bucket, a new refusal), every place that ENUMERATES
  that routing is stale until updated — the four above plus the gate tables in
  [DOMAIN.md](docs/agents/DOMAIN.md). A partial enumeration is a wrong one.
- **STATUS-LINE OWNERSHIP.** `App::status` is a keypress-scoped surface: it carries
  only **outcomes and refusals** (a send result, a launch warning, a paste nudge,
  a resume refusal). A fact that is true over an interval lives in typed state and
  renders on the surface that owns it. Failures and refusals stay sticky until the
  next actionable keypress; confirmations and nudges expire after
  `STATUS_DWELL_TICKS`. See [PATTERNS.md](docs/agents/PATTERNS.md#11-status-line-ownership).

## Engineering principles (mandatory)

- **SOLID / KISS / DRY** — small single-purpose modules; pure core, thin impure
  drivers; one source of truth (parsing lives in `parse_file`; nucleo in
  `search`).
- **YAGNI** — no on-disk index, no speculative abstraction (see the "index
  later" note under Content index in [docs/agents/DOMAIN.md](docs/agents/DOMAIN.md)).
  Match the existing restraint.
- **NO MAGIC VALUES** — every tunable is a named `const` with a rationale.
- **PURE + TESTED** — new decision logic is a pure function with an inline unit
  test; keep side effects in thin wrappers.

## Git commits

- **ALWAYS** read [`GIT_COMMIT_INSTRUCTIONS.md`](GIT_COMMIT_INSTRUCTIONS.md)
  before composing a commit message — follow every rule and example it contains
  (Conventional Commits, `src/`-derived scopes, WHY-focused body, plain text).
- **NEVER** write a commit message without consulting
  `GIT_COMMIT_INSTRUCTIONS.md` first.
- **NEVER** commit gitignored files, and NEVER use `git add -f` or similar force
  commands to bypass `.gitignore` (notably `/target`).
- **Commit TYPE now drives the released version.** release-plz maps Conventional
  Commit types to the next `vX.Y.Z` bump on every merge to `main`, so choosing the
  accurate type per `GIT_COMMIT_INSTRUCTIONS.md` is exactly what the next release
  is computed from. See the release flow in
  [docs/agents/OPERATIONS.md](docs/agents/OPERATIONS.md#how-a-release-happens).

## Self-healing stage (do before finishing)

When your change adds/renames/removes files, modules, commands, keys, flags, or
format handling, RE-RUN the `project-agent-docs` skill to refresh `README.md`,
`AGENTS.md`, and `docs/agents/*` against the new reality. Remove or rewrite stale
references — do not leave them. Deduplicate: if a rule appears in both `AGENTS.md`
and a `docs/agents/*` file, keep it in one place.

Do NOT name the model or harness behind a doc update anywhere in these files —
describe the change, not who or what made it. Do NOT reintroduce a `## Changelog`
section here; git history is the refresh log.

## Execution checklist

1. Read `AGENTS.md` + the relevant `docs/agents/*` file for the area you touch.
2. Make the smallest change that honors the critical rules above.
3. Add/extend inline unit tests for any new pure logic; add a fixture for a new
   format edge case.
4. `cargo build && cargo test && cargo clippy --all-targets && cargo fmt`.
5. **Watch it fail before you believe it.** A check that was never capable of
   going red is ceremony, not evidence. Break what each new/changed test pins,
   see it fail, restore. Re-running a passing gate proves nothing — clippy's
   default target set and its cache both report clean while hiding real warnings.
   Mechanism: [OPERATIONS.md](docs/agents/OPERATIONS.md) (lint gate) and
   [PATTERNS.md](docs/agents/PATTERNS.md) (tests).
6. If discovery/parsing/model changed, verify with `snapback --print-list`.
7. Run the self-healing stage.

Full command reference and the validation checklist:
[docs/agents/OPERATIONS.md](docs/agents/OPERATIONS.md).

## Progressive disclosure

| Need | Read |
| --- | --- |
| Module map, stack, runtime wiring | [docs/agents/ARCHITECTURE.md](docs/agents/ARCHITECTURE.md) |
| Session format, JSONL fields, domain concepts | [docs/agents/DOMAIN.md](docs/agents/DOMAIN.md) |
| Implementation + testing conventions | [docs/agents/PATTERNS.md](docs/agents/PATTERNS.md) |
| Commands, env, `--print-list`, CI + release automation, checklist | [docs/agents/OPERATIONS.md](docs/agents/OPERATIONS.md) |
| External `claude` CLI flags/commands + version pin + spawned argv | [docs/agents/CLAUDE_CLI.md](docs/agents/CLAUDE_CLI.md) |
| Commit message rules + examples | [GIT_COMMIT_INSTRUCTIONS.md](GIT_COMMIT_INSTRUCTIONS.md) |
| Reading order / doc ownership | [docs/agents/README.md](docs/agents/README.md) |
| End-user features + full key map | [README.md](README.md) |

## Ready checklist

- [ ] Change honors every critical rule that touches its area.
- [ ] `cargo build`, `cargo test`, `cargo clippy --all-targets`, `cargo fmt` all
      clean **on a run that actually rebuilt** — a cached clippy prints nothing
      whether or not it would have warned (see OPERATIONS.md).
- [ ] Every new/changed test was OBSERVED FAILING against the un-fixed code.
      Un-failed tests are unverified claims, not coverage.
- [ ] New pure logic has an inline unit test; new format edge case has a fixture.
- [ ] Key/flag docs kept in sync across the four locations.
- [ ] Agent docs refreshed via the self-healing stage.

---

Future agents: when project structure changes, use the `project-agent-docs`
skill to update this documentation rather than hand-editing it.
