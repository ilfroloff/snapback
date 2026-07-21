# Agent reference docs

Deeper reference for AI coding agents working on `snapback`. Start with the
top-level [`AGENTS.md`](../../AGENTS.md) (the mandatory entry point and rule
set); come here for detail. Each file owns one scope with no overlap — if you
find the same rule in two places, that is a bug to fix.

## Reading order

1. [`AGENTS.md`](../../AGENTS.md) — objective, critical rules, engineering
   principles, execution checklist. Read before any change.
2. [ARCHITECTURE.md](ARCHITECTURE.md) — **what things are**: identity, stack,
   module map, runtime architecture (the dashboard loop, the event loop, the
   load pipeline, terminal-safety seams).
3. [DOMAIN.md](DOMAIN.md) — **the session format**: store layout, the
   session/subagent/sidecar distinction, the JSONL fields relied on, and the
   derived concepts (label, grouping, content index, fork lineage, turn count,
   live agents, scopes).
4. [PATTERNS.md](PATTERNS.md) — **how to build new things**: the repeated
   implementation rules and the testing conventions to match.
5. [OPERATIONS.md](OPERATIONS.md) — build/test/lint/run commands, the
   `CLAUDE_PROJECTS_DIR` override, the hidden `--print-list` mode, the CI +
   release-plz automation, and the pre-finish validation checklist.

## Section ownership (avoid duplication)

| Topic | Lives in |
| --- | --- |
| Module responsibilities, stack, runtime wiring | ARCHITECTURE |
| Store layout, JSONL fields, label/grouping/fork-lineage/turn-count/live-agent semantics | DOMAIN |
| Fail-soft / authoritative-from-file / isolation / styling rules, testing conventions | PATTERNS |
| Commands, env vars, CI + release automation, validation checklist | OPERATIONS |
| Critical rules + engineering principles | AGENTS.md |

## Maintenance

These docs are generated and refreshed by the `project-agent-docs` skill from
the real repository. When the code structure changes, re-run that skill rather
than hand-patching, so stale references are removed in the same pass. Git history
is the refresh log — do not keep a changelog inside these docs.
