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
- **OFF-UI-THREAD blocking work.** RECURRING shell-outs / FS watch / input run on
  their own threads and deliver `AppEvent`s; the render loop never blocks. A
  one-shot at hand-off is a deliberate, documented exception (`PATTERNS.md` §6).
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
- **2026-07-15** — Restored the `N msgs` turn count on an expanded fork lineage's
  CHILD rows, which the fold below shipped without. The drop was the wrong call,
  and its three arguments are recorded here because each will look reasonable
  again: (1) *"`Session` carries no message count"* — true, and that is the thing
  to FIX, not a reason not to do it; a missing field is a task, not a constraint.
  (2) *"`content_index` is a 64 KB-capped dump that would silently lie at the
  cap"* — a CORRECT argument, but only against using `content_index` as a PROXY
  for the count. It says nothing about a real counter and went irrelevant the
  moment one existed; it is now pinned from the other side by a test that counts
  200 turns in a file whose index stops dead at the cap. (3) *"widening the model
  to satisfy a mockup is the speculative abstraction YAGNI forbids"* — a MISREAD.
  YAGNI forbids fields added because they MIGHT be needed; this one serves a
  shipped, approved UI requirement, which makes it REQUIRED rather than
  speculative. "A mockup" is the wrong frame for a ratified spec, and YAGNI never
  licenses declining to build what was asked for.
  What all three missed is the point of the row: lineage members are
  label-identical BY CONSTRUCTION — that identity IS the duplicate-row bug — so
  `6 msgs` beside `171 msgs` is the single most informative thing a child can
  carry. It says at a glance which member is a stalled stub and which holds the
  work. The timestamp and the 8-char id say WHICH member; neither says which one
  is worth going back to.
  **A turn is a `user` or `assistant` record — deliberately a NARROWER set than
  the four types (`user`/`assistant`/`attachment`/`system`) the root-uuid logic
  calls tree records, and the two are NOT to be unified.** Roughly a quarter of
  the tree is not conversation: `attachment` context is hook-injected and
  `system` records are notices — nobody typed them and claude did not answer
  them. Counting tree records would inflate a stub that holds no work into
  something that looks like it does, which is precisely the question the number
  exists to answer. Two overlapping notions, one of them a tree test and the
  other a conversation test; collapsing them into a single "tree record"
  predicate would break the count while leaving the lineage correct, which is the
  kind of bug that reads as a tidy-up. The counter rides the EXISTING streaming
  pass — no second read, no allocation, nothing to invalidate — and sits OUTSIDE
  the cap guard, which is the whole of argument (2)'s answer. FAIL-SOFT: no turns
  counts 0, and a missing or non-string `type` simply does not count.
  **The ` N msgs` segment is ALL-OR-NOTHING — drawn whole or dropped entirely,
  never clipped.** A clipped count is not a smaller count, it is a WRONG one:
  `171` cut to fit reads back as `17`, a plausible number off by an order of
  magnitude, and it would send the user to resume the wrong member. No answer
  leaves them where they were; a confident wrong answer does not. The 8-char id
  is not shortened to make room either — it is already at its documented minimum
  (`CHILD_ID_CHARS`), so it has nothing left to give, and trimming it would trade
  a field that cannot be wrong for one that can while risking two children on a
  shared prefix. The count is the field that yields last and, when the columns run
  out, entirely. That is the same marker-first discipline `fit_label` already
  applies, not an exception to it: there the label yields because it is redundant,
  here the count yields because it is the only thing left that can.
  Docs refreshed: `DOMAIN.md` gains the turn count as a derived concept (the
  turns-vs-tree-records split, and why a real counter is untouched by the cap
  objection that sank the proxy) and records what an expanded child row carries
  and why the count is the informative field; `ARCHITECTURE.md` notes `msg_count`
  falling out of the same streaming pass as `root_uuid`, counted outside the cap
  guard, plus the view's all-or-nothing drop; `PATTERNS.md` §3 registers
  `child_msgs`/`fit_child_msgs` and its fixture inventory records that the fork
  pair's members differ in turns on purpose; `README.md` tells the user the
  fanned-out copies say how much work each one holds.
- **2026-07-15** — Folded background-fork lineages behind one expandable row,
  ending the visually identical "double sessions" the board drew during
  background work. The cause was never a parse bug: handing a prompt to a
  background job makes claude FORK the transcript — it copies the foreground
  file's records verbatim (identical record `uuid`s) into a NEW `sessionId` file,
  stamps it `sessionKind: "bg"`, and appends there while the foreground file
  stops growing. Both share `cwd` + `gitBranch` + first prompt, so
  `label::finalize_label` derives the SAME label and the rows are
  indistinguishable. The store itself is clean — zero duplicate `sessionId`s,
  every file separately resumable — so this is presentation, and the fold is
  presentation-ONLY. That is proven rather than asserted: a same-moment A/B of
  two binaries against ONE store state (265 vs 265, **id sets identical**, and
  the id-set comparison is what carries it, since two equal counts could hide an
  offsetting drop+add). The method was forced by the store itself — a count
  written down earlier is NOT a baseline, because the live store DRIFTS under the
  measurement: four sessions vanished from disk mid-implementation, so comparing
  against the recorded 269 would have reported a phantom 4-session regression.
  A TOGGLE, not a hide, because the twins are not redundant: the bg copy is what
  makes `claude -r` refuse, so the stalled ancestor is the ONLY plain-resumable
  copy of that conversation. Hiding it irrecoverably would delete a real
  capability. `←`/`→` were unbound and are non-printable, so they can never be
  swallowed by type-to-search the way a plain letter would.
  **The lineage identity is the `uuid` of the record whose `parentUuid` is JSON
  `null` — the transcript TREE's root — NOT the first user/assistant uuid.** That
  correction is the load-bearing one. The first-message key is anchored to FILE
  ORDER, so it breaks whenever a fork's leading prompt differs (edited-and-resent,
  or a slash-command turn ordered differently) even though the conversation is
  identical: measured, it MISSED three real lineages, one of which shares 133 of
  ~136 message uuids with its twin. The null-parent root is STRUCTURAL — copied
  verbatim into every fork, so nothing downstream can move it — and it costs one
  comparison inside the EXISTING streaming pass: no walk, no second read, no
  index (YAGNI — the field falls out for free). It cannot false-positive either,
  since uuids are minted per record, so only a COPIED prefix can collide and a
  copied prefix IS a fork. Two traps are pinned by tests because both look like
  bugs to a future reader: `type` is deliberately NOT filtered on (the root is an
  `attachment` far more often than a `user` — the conversation starts two records
  deep, behind hook-injected context), and `parentUuid: null` (the root) is a
  different answer from an ABSENT `parentUuid` (a record outside the tree).
  FAIL-SOFT throughout: `root_uuid: Option<String>`; `None` ⇒ no lineage ⇒ never
  folded, never dropped.
  Keyed on `(repo, branch, root)` rather than the root ALONE: some lineages span
  branches, and folding across them would gather members across branch group
  heads and break `build_rows`' one-head-per-group invariant. It is also the
  correct semantic — a fork onto another branch is different work, and keeps its
  own row under its own branch's head.
  Folding is **content-derived and never liveness-gated**, which is the quietest
  decision here and the one most likely to be "improved" later. Gating the fold on
  having a live twin would let the ~1s agents poll restructure the list once per
  second, with rows appearing and vanishing under the cursor — precisely what
  STABLE-ID STATE exists to prevent. `expanded` is likewise keyed by the
  content-derived `LineageKey` and never by index, so it survives the reload that
  reorders every index in it.
  **Gathering an expanded lineage's members under their head is deliberate and
  implemented, not emergent** — an earlier draft called it "inherent to folding",
  which was simply wrong. Filtering alone leaves a child at its OWN timestamp
  slot, and time scatters a lineage: measured, 18 of 27 head→child pairs have
  unrelated rows between them, so an un-gathered child lands as an indented,
  label-less row far from the head that explains it — leaving the original
  complaint ("I can't tell these apart") moved rather than solved. It lives in
  `fold`, not `build_rows`, which cannot reorder without breaking the rule that
  `filtered`'s order IS the navigation order.
  New pure, framework-free `store::lineage` (`lineage_key` / `head_of` / `fold`),
  with D1's rank written down ONCE as a private `member_rank` that both the head
  pick and the gather sort read, so the head can never drift from the top of its
  own run. `(+N)` is `Modifier::DIM` + named ANSI; the pure `fit_label` reserves
  the marker's columns BEFORE the label's, so a narrow pane clips the (identical,
  by-construction redundant) label rather than the one mark saying this row stands
  for others — clip that and the fold silently becomes the vanished-sessions bug
  it exists to prevent. No `▸`/`▾` chevron: an expanded head hides nothing and so
  carries no count, making it indistinguishable from a lone row at row level,
  while its gathered children already say it is open. A child row reports
  (timestamp, badge, 8-char id) and asserts nothing about resumability — that is
  the hand-off probe's call, never a render's.
  (**The field list SUPERSEDED 2026-07-15**: a child row also reports its TURN
  COUNT. Dropping it here was wrong — the members of a lineage are
  label-identical by construction, so the count is the ONE field on that row that
  says which of them is a stalled stub and which holds the work, and the reasons
  given for dropping it do not survive inspection. The "asserts nothing about
  resumability" half STANDS exactly as written: a turn count is a REPORT of what
  the file contains, never a prediction of what claude's gate will permit. See
  the entry above, which wins wherever the two disagree.)
  Docs refreshed: `DOMAIN.md` gains the fork-on-background mechanism, the
  `sessionKind` field (recorded as the thing that explains the duplicate on disk,
  and deliberately NOT read — the tree is what proves two files are one
  conversation), and the tree/root-uuid lineage model. All three were
  undocumented anywhere in the repo, which is exactly why the duplicate rows were
  surprising. It also now splits the TWO senses of "fork" — the hand-off snapback
  performs on request (`--fork-session`) versus the copy claude makes unasked —
  where it previously knew only the former; and it records the store shape as
  PROVENANCE, not a contract, following the sampled `state`/`status` precedent,
  with the observed drift written down. `ARCHITECTURE.md` gains the
  `store::lineage` row, the fold's place above the load pipeline, and the app/view
  ownership; `PATTERNS.md` §3 registers the new pure fns and its fixture inventory
  gains the fork pair + root-less session; `README.md` describes the fold for end
  users.
- **2026-07-15** — Closed the loose thread the entry below left: the Attach job
  id was still read from `App::reported_agent(...)` — the `--all` polled map that
  the same entry declares too stale to trust for liveness. That is the identical
  bug one layer down (an AUTHORITATIVE decision made from a stale snapshot), and
  it is worse at Attach than at the gate: the overlay can sit open INDEFINITELY
  while the user decides, so the staleness window is unbounded rather than ~1.3s
  — even the probe that OPENED the overlay is stale by the time Attach is chosen.
  It only failed soft (`ATTACH_NO_JOB_ID`, leaving just Fork) because a stale id
  usually still matched. The rule is now uniform and is the durable finding:
  **every hand-off re-asks claude; nothing hands off on polled data. The poll
  draws badges, and that is the whole of its authority.**
  The probe was already parsing the job id and THROWING IT AWAY:
  `live_session_ids` ran the shared fail-soft parse and then did
  `.into_keys().collect()`. It is now `agents::live_agents`, returning the parsed
  records (`sessionId` → `ReportedAgent`) from the SAME one shell-out and one
  parse — no second call, no second parser — so liveness AND the job id come from
  ONE authoritative read and can never resolve against different snapshots.
  `App::live_agent_now` lifts the matched record out; `is_live_now` is now
  `live_agent_now(..).is_some()`, which is its EXACT prior semantics (membership
  in the fresh active list) re-expressed over one probe rather than two notions of
  live — a source refactor, no behaviour change, and the existing gate/TOCTOU
  tests pass untouched to prove it.
  `ReportedAgent::id` SURVIVES and is not stale-by-construction: both readings
  parse into the one struct, so the field exists on both, but it is now read ONLY
  from `live_agents`' records. `App::reported_agent` survives too — it is the
  badge/banner accessor (`view` draws from the poll precisely BECAUSE a render
  must never shell out), and its doc now says rendering-only rather than claiming
  the Attach id.
  "No longer live at Attach" is handled honestly rather than by spawning a dead
  id: `resume::ATTACH_NOT_LIVE`. The probe's fail-soft-toward-not-live direction
  COLLAPSES two premises — "the agent finished" and "we could not ask" are both an
  empty map — so the copy states what was OBSERVED (claude no longer reports it),
  never a cause the probe cannot distinguish, and it names the routes valid in
  both worlds (Enter re-probes and is backstopped by claude's own check; a fork of
  a finished session is an ordinary fork) instead of acting for the user. Fork
  deliberately does NOT probe — it has no liveness question to ask — so it stays
  valid exactly when Attach is refused, which is what the refusal points at.
  Tests seed the probe through the existing `App::set_live_probe` seam (no
  `claude` is ever spawned) and assert the observable argv the driver would spawn.
  The fixtures make the two ids DIFFER (probe `fresh-job` vs `--all` `stale-job`),
  because a fixture where they agree passes against either source and cannot
  distinguish them — the "one bucket" mistake `PATTERNS.md` records. The
  vanished-at-Attach case has claude answering with a DIFFERENT agent still live,
  so the refusal is pinned on our session's absence rather than on an empty
  answer, and an "attach to whatever is live" bug fails it. Probing at Attach is
  the same documented one-shot-at-hand-off exception (`PATTERNS.md` §6), costed
  per branch at the call site: invisible on a confirmed attach (nothing renders
  before teardown), a ~0.26s hitch on the two refusals.
- **2026-07-15** — Fixed the smart-Enter gate resuming into `claude -r`'s own
  refusal ("Session … is currently running as a background agent (bg)"), reported
  on a `● bg done` row. It was a TOCTOU race: the gate decided liveness from the
  `--all` poll — up to ~1.3s stale (~0.26s shell-out, then a 1s sleep) — and then
  `claude` RE-EVALUATED liveness at spawn time and disagreed. The durable finding,
  now recorded in `DOMAIN.md`: **`--all`'s `state: "done"` means "the agent
  reported completion", NOT "claude will permit `-r`"** — the two can disagree
  transiently, claude is the only authority, and the gate therefore probes
  claude's active list AT HAND-OFF rather than inferring from a polled snapshot.
  The compounding error was the previous entry's own: `--all` put 123 non-live
  records into a map whose membership USED to mean "live" structurally (the map
  WAS claude's active list), and that structural fact was replaced with an
  INFERENCE (`state != "done"` ⇒ live). It agrees in steady state (active=37,
  `--all`-not-done=37, exact) but it was a guess about claude's gate, and a guess
  is what lost the race.
  So `claude agents --json` is now read TWO ways through the ONE existing
  fail-soft parser: `reported_agents` (`--all`, polled, unchanged — still one call
  per cycle, no new tick/thread/event source) stays the DISPLAY signal driving
  badges/banner/pulse via `classify`, and a new `live_session_ids` (bare, NO
  `--all`, one-shot at hand-off) is the GATE signal, where MEMBERSHIP is liveness
  with nothing inferred. Both argvs are pinned by their own tests off a shared
  prefix (`AGENTS_ARGV` + `AGENTS_ALL_FLAG`), because `--all` reaching the probe
  would silently report all 123 finished sessions as live and break plain resume
  for the majority of rows. `AgentActivity::Done` stays — it is a badge, not a
  gate; `agents::is_live` / `App::is_live` are DELETED rather than left as a second
  notion of liveness, and `App::reported_agents` remains the authoritative source
  of the Attach job id (a different question).
  (**Attach-job-id claim SUPERSEDED, and `live_session_ids` RENAMED, later the
  same day**: calling the Attach id "a different question" was the loose thread
  this entry left. It is a different QUESTION but not a different KIND of
  question — it is still an authoritative decision, so it must not be read from
  the polled map either. The probe now returns the RECORDS
  (`agents::live_agents`), and Attach re-asks at its own hand-off. See the entry
  above, which wins wherever the two disagree.)
  The probe fails soft toward NOT live (empty set ⇒ plain resume ⇒ claude's own
  check backstops it) — deliberately the REVERSE of the deleted classifier's
  fail-toward-live, since membership has no bucket to be uncertain about and the
  only remaining error is "we could not ask". Degrading toward *let claude decide*
  is correct.
  On the OFF-UI-THREAD rule: that rule exists so the 1s POLL never blocks
  rendering, and the poll is untouched. The gate's probe is a ONE-SHOT at hand-off,
  analogous to `resume`'s authoritative `cwd`/`sessionId` re-read at the same
  moment; the tradeoff is stated honestly at the call site rather than framed as
  free — on plain resume nothing renders between probe and teardown, but on the
  OVERLAY branch the overlay draws ~0.26s after Enter, a small deliberate hitch.
  A lost race is also recovered AFTER the fact (`lib::run`'s `after_nonzero_resume`,
  off the board, free on the happy path): a non-zero plain resume re-probes and,
  only if claude confirms the session is live, says so and opens Attach/Fork
  instead of guessing with `RESUME_NONZERO_HINT`; if not live the neutral hint
  stands unchanged, since we must never claim a session is running without the
  probe's word. Recovery is scoped by ONE new `Ready::race_probe_id: Option<String>`
  carrying the AUTHORITATIVE id on the plain-resume path only, so `None` means
  "does not apply" STRUCTURALLY (a fork of a live session is expected to work;
  attach is already the live path; a new session has no session) — never by
  sniffing `argv` for `--fork-session`, and never by parsing claude's error text.
  `resume::launch` still spawns with INHERITED stdio and `.status()`: piping stderr
  to read it would hide claude's output from the user and risk a deadlock when the
  pipe fills.
  Tests inject the probe (`App::live_probe`, a boxed closure defaulting to the real
  shell-out) rather than spawning `claude`, following `build_argv`'s testable-seam
  spirit; under `cfg(test)` the default PANICS instead, so a test can neither spawn
  `claude` nor pass vacuously on an unstated "nothing is live". The pinning test is
  a `done`-BADGED session that claude still reports LIVE — the exact TOCTOU case —
  which the old gate cannot satisfy.
- **2026-07-14** — Made a live session's state legible without decoding a terse
  qualifier: the preview now leads with a friendly status banner
  (`view::preview_banner`, e.g. `bg needs input`) PINNED as its own layout row
  above the transcript, and the list badge is colored by state (dot + `bg`/`live`
  label share one color) with the dot pulsing while the agent works. The banner is
  a layout row, NOT a line prepended into the scrolled `Text`: the preview is
  bottom-anchored by default, so a prepended line was scrolled off-screen for any
  transcript taller than the pane. `view::preview_split` is the one place that
  geometry is derived — `render_preview` draws against its rects and
  `update::link_under_pointer` hit-tests against the same transcript rect, which
  is what keeps a click on a preview link resolving to the row it was drawn on. A
  pane with no banner splits off nothing, so its geometry is unchanged. The
  undocumented `state`/`status` value set is now interpreted in ONE place — a pure
  `agents::classify -> AgentActivity` that `friendly_status`, `is_active`,
  `is_live`, and `view::badge_color` all derive from — replacing the badge's
  hardcoded green. That classifier stays FRAMEWORK-FREE: `agents` names the
  bucket, and `tui::view` owns the named-ANSI color it maps to, so the fail-soft
  JSONL parser layer never pulls in ratatui.
  The signal itself changed to feed it: the shell-out now passes `--all`
  (`agents::agents_argv`, pinned by an argv unit test rather than by spawning
  `claude`), because the bare command reports only currently-active agents and the
  new `Done` bucket is otherwise unobservable — a just-finished session rendered
  as though claude had never heard of it. The cost is that map MEMBERSHIP stopped
  implying liveness, which was load-bearing for the smart-Enter gate: hence
  `LiveAgent` -> `ReportedAgent`, `App::live_agents` -> `reported_agents`, and a
  new `agents::is_live` that gates on the CLASSIFIED bucket (only `Done` is not
  live) rather than on presence. Without that split, every session that ever
  finished would have been diverted into the Attach/Fork/Cancel overlay instead of
  plain-resuming. `Done` colors a badge green and steady like `Idle`, since
  neither wants anything from the user.
  (**`is_live` SUPERSEDED 2026-07-15**: replacing membership with an inference —
  `state != "done"` ⇒ live — was itself the next bug. It is a guess about claude's
  gate rather than a given, and it lost a TOCTOU race against claude's spawn-time
  check. `agents::is_live` / `App::is_live` are DELETED; the gate now probes
  claude's active list at hand-off. `Done` remains exactly as described here — a
  badge. See the 2026-07-15 entry, which wins wherever the two disagree.)
  Read-only and additive: no new I/O and no new key. The pulse is driven from the
  redraw cadence that already exists — `App` gains a `tick: u64` (counted on
  `AppEvent::Tick`, `wrapping_add`) that the pure `view::blink_visible` phases
  into 500ms on / 500ms off; no new tick, thread, or event source. Nothing carries
  the ANSI blink attribute any more: it was tried first and is ignored by most
  modern terminals, so it never pulsed for the user. `render_search`'s cursor had
  the same dead `SLOW_BLINK` and had therefore never blinked either; it is now
  rephased onto the SAME `blink_visible`/`BLINK_TICKS` as the dot, leaving the
  board one phase source with the two in phase (see `PATTERNS.md` §7). The
  badge's `BOLD` is now asserted off the rendered cell rather than captured and
  discarded. The dot pulses by COLOR (`view::pulse_color`, `Gray` <-> `DarkGray`)
  and its `●` is drawn in every phase: it originally pulsed by swapping the glyph
  for a blank, which MUTATES the row's text, and since we emit plain-text URLs
  (no OSC 8) the terminal re-detects that line's link on every mutation — a
  session label containing a URL visibly flickered at 1Hz. A style-only change
  leaves the text byte-identical, so the same-width blank, the hold-the-column
  rule and their tests are gone rather than rebuilt around color. The search
  cursor still show/hides, which is correct for a cursor and safe on a line with
  nothing auto-detected on it.
  Docs refreshed: `DOMAIN.md` gains the `AgentActivity` bucket table (color, pulse
  AND liveness per bucket, dropping the stale "`state`/`status` (dim qualifier)"
  claim), the `--all` rationale, and — as provenance, NOT a contract — the sampled
  `state`/`status` distribution the buckets were built against, plus the accepted
  last-one-wins duplicate-`sessionId` risk; `PATTERNS.md` §1 names the shell-out
  with its flags, §3 registers the new pure fns and §5 the pinned-row rule, §7
  gains the badge styling rules (named-ANSI rationale; color unifies but pulse does
  not; pulse the style, never the symbol, with the URL-re-detection reason it
  exists; animate from the tick, never from the terminal, off the ONE
  `blink_visible` both share — plus the assert-drawn-cells and
  break-check-the-phase rules that fall out of it) and §8 the
  `BLINK_TICKS`/`watch::TICK` coupling, `ARCHITECTURE.md`'s `agents` row notes the
  classifier and that liveness is a bucket rather than map membership, its
  `tui::view` row the split + palette, and its Tick row the board clock, and
  `README.md` describes the banner + badge for end users — green covering idle AND
  finished, so the badge is not sold as "running right now".
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
