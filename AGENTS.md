# AGENTS.md — snapback

Guidance for AI coding agents. Read this before changing code. It is a strict
system prompt: keep it loaded, follow it exactly.

## Objective

`snapback` (alias `sb`) is a single self-contained Rust **ratatui TUI** that
browses, searches, and resumes **Claude Code** sessions stored as JSONL under
`~/.claude/projects/`. Ship changes that keep the data core correct against a
hostile, undocumented on-disk format and keep the terminal safe across the
resume round trip.

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
- **SUBAGENT EXCLUSION BY DEPTH.** Discover only `<root>/<cwd>/<id>.jsonl` at
  depth 2; NEVER descend into `<id>/subagents/`. Do not make discovery
  recursive. (`src/store/discover.rs`)
- **TERMINAL SAFETY.** Resume/fork/attach SPAWN `claude` as a child and RETURN
  to the board — never replace the process image. Restore the terminal (raw
  mode + alt screen + mouse capture) on EVERY exit: quit, error, hand-off, and
  panic. On EVERY return from a child, hard-reset the terminal — a deterministic
  full re-init onto a fresh screen — so a dirty hand-back (notably a Ctrl-Z that
  exits `claude` without restoring the terminal) repaints from a known-good state
  with no stale cells, native scrollback, leaked keyboard/input modes (notably a
  leftover kitty keyboard-protocol level), or leftover escape-parser corruption
  showing through, and without regressing the idempotent restore. The reset is
  ONE complete return-to-known-state, not one mode per bug, and stays WRITE-ONLY:
  never emit a cursor-position (DSR `CSI 6n`) query on the return path — it
  deadlocks on a dirty child's stdin. (`src/tui/mod.rs`, `src/resume.rs`)
- **NUCLEO ISOLATION.** Every `nucleo` call stays in `src/search.rs`. Matching
  is SUBSTRING (`AtomKind::Substring`), not fuzzy; the filter and highlight
  share one `Pattern`. (`src/search.rs`)
- **STABLE-ID STATE.** Track selection by `session_id`, never list index, so it
  survives autorefresh reloads. (`src/tui/app.rs`)
- **OFF-UI-THREAD blocking work.** Shell-outs / FS watch / input run on their
  own threads and deliver `AppEvent`s; the render loop never blocks.
  (`src/watch.rs`)
- **TERMINAL-SAFE STYLING.** Style with ratatui `Style`/`Modifier` + NAMED ANSI
  colors only. NEVER embed ANSI escapes or hardcode RGB. (`src/store/preview.rs`,
  `src/tui/view.rs`)
- **NARROW `#[allow(dead_code)]`.** Binary-crate lint quirk: attach it to the
  single item with a reason. NEVER a crate/module-wide blanket. (`src/search.rs`,
  `src/watch.rs`)
- **KEEP KEY DOCS IN SYNC.** A key/flag change must update the table in
  `update.rs`, `USAGE`/`KEYS` in `cli.rs`, the help line in `view.rs`, and the
  README key map together.

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

Do NOT name the model or harness behind a doc update anywhere in these files,
including `## Changelog` entries — describe the change, not who or what made it.

## Execution checklist

1. Read `AGENTS.md` + the relevant `docs/agents/*` file for the area you touch.
2. Make the smallest change that honors the critical rules above.
3. Add/extend inline unit tests for any new pure logic; add a fixture for a new
   format edge case.
4. `cargo build && cargo test && cargo clippy --all-targets && cargo fmt`.
5. If discovery/parsing/model changed, verify with `snapback --print-list`.
6. Run the self-healing stage.

Full command reference and the validation checklist:
[docs/agents/OPERATIONS.md](docs/agents/OPERATIONS.md).

## Progressive disclosure

| Need | Read |
| --- | --- |
| Module map, stack, runtime wiring | [docs/agents/ARCHITECTURE.md](docs/agents/ARCHITECTURE.md) |
| Session format, JSONL fields, domain concepts | [docs/agents/DOMAIN.md](docs/agents/DOMAIN.md) |
| Implementation + testing conventions | [docs/agents/PATTERNS.md](docs/agents/PATTERNS.md) |
| Commands, env, `--print-list`, CI + release automation, checklist | [docs/agents/OPERATIONS.md](docs/agents/OPERATIONS.md) |
| Commit message rules + examples | [GIT_COMMIT_INSTRUCTIONS.md](GIT_COMMIT_INSTRUCTIONS.md) |
| Reading order / doc ownership | [docs/agents/README.md](docs/agents/README.md) |
| End-user features + full key map | [README.md](README.md) |

## Ready checklist

- [ ] Change honors every critical rule that touches its area.
- [ ] `cargo build`, `cargo test`, `cargo clippy`, `cargo fmt` all clean.
- [ ] New pure logic has an inline unit test; new format edge case has a fixture.
- [ ] Key/flag docs kept in sync across the four locations.
- [ ] Agent docs refreshed via the self-healing stage.

## Changelog

- **2026-07-16** — Fixed releases cutting no git tag and no GitHub Release.
  Removed `publish = false` from `Cargo.toml`: it made cargo-metadata report the
  package's `publish` as `[]`, and release-plz's `release` command filters
  non-publishable packages out before reaching the git-only path that creates the
  tag and the Release — so the "🔖 Release" run went green having released
  nothing (v0.1.0 merged untagged). The non-published guarantee now rests solely
  on `release-plz.toml` (`git_only = true` + `publish = false`), which is the
  arrangement release-plz's own git-only setup expects. Comments in `Cargo.toml`
  and `release-plz.toml` plus the release section of `OPERATIONS.md` were
  reconciled to that mechanism. No code or config-value changes.
- **2026-07-13** — Restructured `README.md` into a pain-first, user-facing doc:
  expanded the "why" into a concrete cross-folder scenario, tightened the payoff,
  de-jargoned the (unchanged, still-complete) key map, and reduced the feature
  sections to benefit blurbs. Implementation detail was removed by DELETION, not
  moved — it already lives in its owning `docs/agents/*` (crate pins →
  ARCHITECTURE Stack; build/binary shims → OPERATIONS; canonicalization,
  substring search, live badges/Attach job-id, defined-agent frontmatter →
  DOMAIN/PATTERNS; dev-vs-release version label → ARCHITECTURE). Fixed the now
  stale `OPERATIONS.md` install cross-link anchor (`#install--run` → `#install`).
  No code/keybinding/flag changes.
- **2026-07-10** — Added Conventional-Commits-driven versioning + releases via
  release-plz. New `.github/workflows/release-plz.yml` (runs `release-pr` +
  `release` on every push to `main`) and `release-plz.toml` (git-tag-based
  detection with `git_only`, `publish = false`, `features_always_increment_minor`,
  and `git_tag_name = "v{{ version }}"`). Baseline `Cargo.toml` version 0.0.0 ->
  0.1.0 so the first release tags `v0.1.0`. Docs refreshed: `OPERATIONS.md` gains a
  "Continuous integration & releases" section (and drops the stale "no CI config"
  claim), `README.md` gains a `cargo install --git ... --tag vX.Y.Z` path, and the
  Git-commits rule now notes that commit type drives the bump.
- **2026-07-09** — Split the header version indicator by build profile. Release
  builds still show `v<crate-version>`; local debug builds now show
  `dev+<git-short-hash>` with a trailing `-dirty` when the working tree had
  uncommitted changes at build time. New fail-soft `build.rs` captures the hash
  and dirty flag into `SNAPBACK_GIT_HASH`/`SNAPBACK_GIT_DIRTY` (degrades to
  `unknown`/`0` outside a repo); `tui::view::version_label` branches on
  `cfg!(debug_assertions)` via the pure, unit-tested `format_version_label`.
  Docs refreshed (`ARCHITECTURE.md` module map + view row, `README.md` header).
- **2026-07-09** — Added DEFINED-agent selection for a new session: `Ctrl-N` opens
  an agent picker (`claude --agent <name>`) when `~/.claude/agents/*.md` /
  `<launch_dir>/.claude/agents/*.md` yield any, remembering the last pick
  in-memory; new module `src/defined_agents.rs` (distinct from the live-agent
  `src/agents.rs`). Threaded through `resume::build_new_argv`/`check_new` with a
  new-session-specific non-zero hint, a `pending_agent` overlay in
  `tui::{app,update,view}`, and key/agent docs refreshed.
- **2026-07-08** — Documented the `Ctrl-N` new-session hand-off (bare `claude`
  in the launch dir via `check_new`/`build_new_argv`) across `docs/agents/*`
  (`DOMAIN.md` hand-off table, `ARCHITECTURE.md` resume row, `PATTERNS.md` pure
  list + refusal gate).
- **2026-07-06** — Initial `AGENTS.md` + `docs/agents/*` generated from the
  repository.

---

Future agents: when project structure changes, use the `project-agent-docs`
skill to update this documentation rather than hand-editing it.
