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
   session/subagent/sidecar distinction, the JSONL fields relied on, the derived
   concepts (label, grouping, content index, fork lineage, turn count, live
   agents, the preview's peer and injected-context nodes, the answering model
   and the one a `-r` launch restores, scopes, the per-compose model pick and
   its defaults, the compose pick list and where it reads from), and the
   per-state routing tables — the hand-offs, the
   `Ctrl-R` / `Ctrl-K` gates, and the terminal-paste owner table.
4. [PATTERNS.md](PATTERNS.md) — **how to build new things**: the repeated
   implementation rules and the testing conventions to match.
5. [OPERATIONS.md](OPERATIONS.md) — build/test/lint/run commands, the
   environment it reads (the `CLAUDE_PROJECTS_DIR` / `SNAPBACK_CONFIG_DIR`
   overrides, the session facts the clipboard copy routes by, `claude`'s own
   model variables the compose boxes' `model:` labels read, and the two that
   locate claude's workspace-trust record), the runtime
   prerequisites, the hidden `--print-list` mode, the CI + release-plz
   automation, and the pre-finish validation checklist.
6. [CLAUDE_CLI.md](CLAUDE_CLI.md) — **the external `claude` binary**: version
   pin, the argv `snapback` spawns (including where a compose's `Ctrl-L` pick
   places `--model` / `--effort`), the one effect that is not a `claude`
   invocation (a `kill(2)` on a pid `claude agents --json` reported), top-level
   flags, commands, the `--model` alias set (which `claude --help` reports
   incompletely, so it is captured from the binary instead — a point-in-time
   record for humans, since `snapback` reads the same array off the installed
   binary at runtime), what `--effort` accepts and why it can never fail a launch,
   which model a launch runs on without `--model` (the settings precedence and the
   `-r` model restore, read out of the bundle), the `initialize` control
   handshake the compose pick list asks (pinned at its own version: which
   built-ins and skills claude hides from its own `/` menu, and the agent-mention
   forms it resolves) with the workspace-trust rule that picks its two forms
   (argv and child environment) and what a never-trusted folder can run under `-p` (its settings, and claude's
   own git prefetch), and the
   background-session commands — `stop`/`attach`, which it depends on, and the
   ones it deliberately does not use.

## Section ownership (avoid duplication)

| Topic | Lives in |
| --- | --- |
| Module responsibilities, stack, runtime wiring | ARCHITECTURE |
| Store layout, JSONL fields, label/grouping/fork-lineage/turn-count/live-agent/answering-model semantics, the per-compose model pick and its `ComposeDefault` cases, the compose pick list's sources, precedence and fetch/retry behaviour, and the routing tables (hand-off, `Ctrl-R`, `Ctrl-K`, `Event::Paste`) | DOMAIN |
| How the critical rules are carried out in code (fail-soft direction, authoritative re-read, matcher isolation, styling, off-thread shapes, status ownership), the tunables table, testing conventions | PATTERNS |
| Commands, env vars, CI + release automation, validation checklist | OPERATIONS |
| External `claude` CLI surface (flags, commands, version pin, spawned argv, the captured `--model` alias set + its refresh command, the `--effort` levels and how claude treats them, the settings precedence and `-r` model restore a launch without `--model` follows, the `initialize` handshake's probed wire shape and side effects, the built-ins and skills claude hides from its `/` menu, the agent-mention forms, claude's workspace-trust record and rule, what an untrusted folder can run (claude's own git prefetch included), and the fetch's two forms, argv and child environment) | CLAUDE_CLI |
| The runtime readers of that alias set (`model_aliases`), of the settings default and restore override (`claude_settings`) and of claude's workspace-trust verdict (`claude_trust`), and how their answers reach the compose boxes | ARCHITECTURE |
| The critical rules themselves + engineering principles (the authoritative wording: the files above own each rule's mechanism, and where one still repeats a rule, AGENTS.md's statement wins) | AGENTS.md |

## Maintenance

These docs are generated and refreshed by the `project-agent-docs` skill from
the real repository. When the code structure changes, re-run that skill rather
than hand-patching, so stale references are removed in the same pass. Git history
is the refresh log — do not keep a changelog inside these docs.

Exception: [CLAUDE_CLI.md](CLAUDE_CLI.md) documents the external `claude` binary,
not this repo, so the skill cannot regenerate it. Refresh it by re-capturing from
the live CLI per its own
[Refreshing this doc](CLAUDE_CLI.md#refreshing-this-doc) section; its separately
pinned `initialize` handshake section carries its own re-verify commands, the
hidden-built-ins, workspace-trust, git-prefetch and `rootOnly` checks among them.
