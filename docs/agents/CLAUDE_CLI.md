# Claude CLI reference

Reference for the **external `claude` binary** that `snapback` shells out to.
Everything here is captured from the live CLI (`claude --help` and each
`claude <cmd> --help`), not from this repo — it is the "what does Claude Code
actually expose" quick-review sheet the rest of the docs assume.

`snapback` never links `claude`; it spawns it as a child (resume/fork/attach/send,
the compose pick list's catalog fetch, and the `Ctrl-X w` move) and reads
`claude agents --json`. On
ONE route it also sends a SIGTERM to a `pid` that `claude agents --json` reported. That is not a `claude` invocation, and it is
listed [below](#how-snapback-drives-claude) so the boundary stays visible. For the
catalog fetch and the move alone it also READS, never writes, claude's own
workspace-trust record ([Workspace trust](#workspace-trust-what-an-untrusted-folder-can-run-and-the-two-argv-forms)). The
terminal-safety and authoritative-from-file rules around those spawns live in
[PATTERNS.md](PATTERNS.md) and
[ARCHITECTURE.md](ARCHITECTURE.md); the runtime "`claude` on `PATH`" prerequisite
lives in [OPERATIONS.md](OPERATIONS.md#runtime-prerequisites). This file owns one
scope only: the surface of the `claude` command itself.

## Version pin (self-healing)

> **Captured against `claude 2.1.282` (Claude Code).** Previous capture:
> `2.1.280`.

Before trusting a flag or command below, compare the installed version:

```sh
claude --version </dev/null   # e.g. "2.1.282 (Claude Code)"
```

This pin covers the COMMAND surface: flags, commands and their help text. The
`claude agents --json` WIRE shape that `snapback` parses (its keys, which records
carry `id` and `pid`, and what `kind: "interactive"` turned out to denote) is
measured separately in [DOMAIN.md](DOMAIN.md#reported-agents-srcagentsrs). It was
captured at 2.1.278 and spot-checked at 2.1.280 with the same nine-key union, and
again at 2.1.282 ([DOMAIN.md, Sample E](DOMAIN.md#observed-value-distribution)).
The `initialize` control handshake is pinned separately too, at 2.1.284, in
[its own section](#the-initialize-control-handshake-compose-pick-list), with the
workspace-trust rule the fetch's form depends on: both are probed or read out of
the bundle, not `--help` captures, and the refresh below does not re-verify them.
So is the [`set_cwd` move](#set_cwd-moving-a-session-without-leaving-the-board),
probed at 2.1.291 (2026-10-08) in a throwaway profile, and claude's own
[`/cd`](#cd-moving-a-session-to-another-folder), observed at 2.1.284 (2026-10-02):
neither is captured from `--help`, and the refresh below re-probes `set_cwd`
alone, because the move depends on it.

- **Installed == pinned** → this doc matches the live CLI. Trust it.
- **Installed < pinned** → the local install is **behind this doc**. Newer flags
  listed here may not exist yet; `claude update` (or `claude install latest`)
  brings the binary up to the documented surface. Do not assume a flag is gone
  just because an older local `claude` rejects it.
- **Installed > pinned** → **this doc is stale**, not the CLI. Re-capture and
  refresh it (see [Refreshing this doc](#refreshing-this-doc)) before relying on
  the tables; flags may have been added, renamed, or removed since 2.1.282.

Keep the pinned version above in sync with the tables — bumping one without the
other defeats the check.

## Website gap claims

The ONE home of the website copy rule: a row in `website/src/pages/index.astro`
says what snapback DOES, never what Claude Code lacks. Each row answers a
Claude Code behaviour recorded here, so a closed gap is found by re-reading its
source ([Refreshing this doc](#refreshing-this-doc)).

| Website row | Claude Code behaviour it answers | Source | Verified at |
| --- | --- | --- | --- |
| `--one-board` | `/resume` is a picker inside a session, and agent view lists background sessions only. | https://code.claude.com/docs/en/sessions; https://code.claude.com/docs/en/agent-view | docs read 2026-10-02; installed claude 2.1.284 |
| `--search` | `/resume` filters by name, title, branch or PR URL, and agent view by name, first prompt or result. Neither documents transcript-text search. A live `claude --resume <term>` probe matched a title word but not an early message token. | the sessions and agent-view docs; the live probe | docs read 2026-10-02; installed claude 2.1.284 |
| `--everything` | `/resume` leaves out `claude -p`, Agent SDK and `/loop`-first sessions. | the sessions doc | docs read 2026-10-02; installed claude 2.1.284 |
| `--tidy` | There is no per-session transcript delete; `claude rm` keeps the transcript, and `claude project purge` wipes a whole project. | the sessions and agent-view docs | docs read 2026-10-02; installed claude 2.1.284 |
| `--fold` | The picker groups entries that share a session ID, while background hand-off copies carry new IDs. The docs say nothing about folding by content. Confidence: medium. | the sessions doc | docs read 2026-10-02; installed claude 2.1.284 |
| `--move` | The only user command that moves a session is `/cd`, an interactive command with no headless twin: the one registry entry named `cd` is `type: "local-jsx"`, which claude's [print-mode gate](#the-initialize-control-handshake-compose-pick-list) never passes. The Agent SDK docs say "Neither SDK has a setter for `cwd`". The [`set_cwd`](#set_cwd-moving-a-session-without-leaving-the-board) request the move sends is undocumented. Confidence: medium. | the 2.1.291 bundle's `/cd` registry entry; https://code.claude.com/docs/en/commands; https://code.claude.com/docs/en/agent-sdk/configuration | bundle and docs read 2026-10-08; installed claude 2.1.291 |
| `--model` | The model is set per session (`--model`, `/model`, the dispatch default); the docs say nothing about a per-reply pick. Confidence: medium. | the sessions and agent-view docs | docs read 2026-10-02; installed claude 2.1.284 |

## How snapback drives `claude`

The only invocations `snapback` depends on, plus the one effect that is NOT an
invocation (the last row). Each invocation is a **pure argv builder** with an
inline test asserting the exact string, so drift here is caught by
`cargo test`. Cross-references are to the builder that owns the shape.

| Purpose | Argv | Builder |
| --- | --- | --- |
| Resume a session in place | `claude -r <session-id>` | `resume::build_argv` (`src/resume.rs`) |
| Fork a session (new id) | `claude -r <session-id> --fork-session` | `resume::build_argv` |
| Dispatch a DEFINED agent | `claude --agent <name>` | `resume::build_new_argv` (`src/resume.rs`) |
| Start a new session on a drafted prompt | `claude [--agent <name>] [--model <alias> [--effort <level>]] <prompt>` | `resume::build_new_argv` |
| Start a BACKGROUND agent on a drafted prompt | `claude [--agent <name>] [--model <alias> [--effort <level>]] --bg <prompt>` | `send::build_bg_launch_argv` (`src/send.rs`) |
| Attach to a live background job | `claude attach <job-id>` | `resume::build_attach_argv` |
| Quick-send a reply (non-interactive) | `claude -p -r <session-id> --output-format json [--model <alias> [--effort <level>]] <message>` | `send::build_send_argv` (`src/send.rs`) |
| Release a held job before a reply, or interrupt a selected agent (`Ctrl-K`) | `claude stop <job-id>` | `send::build_stop_argv` |
| Move a session to another folder (`Ctrl-X w`), the board staying up | In a folder claude TRUSTS: `claude -p --input-format stream-json --output-format stream-json --verbose --strict-mcp-config --settings {"disableAllHooks":true} -r <session-id>`, never with `--no-session-persistence`. In ANY OTHER folder: the same, followed by `--setting-sources user`, with `CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS=1` added to the CHILD's environment. Either runs in the session's CURRENT folder (never the target), with ONE stdin line holding `{"type":"control_request","request_id":"snapback-move","request":{"subtype":"set_cwd","path":"<abs target>"}}` and then EOF (see [`set_cwd`](#set_cwd-moving-a-session-without-leaving-the-board)) | `claude_move::build_set_cwd_argv(session_id, FolderTrust)`, the catalog's `build_catalog_env(FolderTrust)` for the child's environment, and `set_cwd_request_line` (`src/claude_move.rs`); the verdict from `claude_trust::folder_trust`, read on the move's worker thread |
| Detect live agents (gate probe) | `claude agents --json` | `agents::live_agents_argv` (`src/agents.rs`) |
| Detect live agents (incl. just-finished) | `claude agents --json --all` | `agents::agents_argv` |
| List a folder's slash commands and agents (compose pick list) | In a folder claude TRUSTS: `claude -p --input-format stream-json --output-format stream-json --verbose --no-session-persistence --strict-mcp-config --settings {"disableAllHooks":true}`. In ANY OTHER folder: the same, followed by `--setting-sources user`, and with `CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS=1` added to the CHILD's environment (never snapback's own). Either runs IN that folder, with ONE stdin line holding the object `{"type":"control_request","request_id":"snapback-catalog","request":{"subtype":"initialize"}}` and then EOF (see [the handshake](#the-initialize-control-handshake-compose-pick-list) and [the two forms](#workspace-trust-what-an-untrusted-folder-can-run-and-the-two-argv-forms)) | `claude_catalog::build_catalog_argv(FolderTrust)` and, for the child's environment, `build_catalog_env(FolderTrust)` (+ `initialize_request_line`) (`src/claude_catalog.rs`); the verdict from `claude_trust::folder_trust` (`src/claude_trust.rs`), read on the fetch's worker thread |
| `Ctrl-K` on a reported session with NO job id but a `pid` | **none: NOT a `claude` invocation.** A `kill(2)` sending `SIGTERM` (never `SIGKILL`) to the `pid` that `claude agents --json` (the bare probe above) reported, after a confirm and a re-probe at `Enter` | `send::signal_plan` (pure: re-verify the pid against the fresh record) → `send::signal_target` (pure: a strictly positive `pid_t`, or refuse) → `send::signal_term` (the syscall; no test calls it) |

The last row is the deliberate exception to "every row is an argv builder": it
spawns nothing and has no argv, so there is no string to pin. Its tests pin the
two pure checks in front of the syscall instead. It exists because `claude` offers
no verb for a record without a job id (see
[Background-session commands](#background-session-commands)), so the only handle
left is the pid claude itself reported. Do not confuse it with **`claude kill
<id>`**: at 2.1.282, as at 2.1.280, that is an alias of `claude stop`, takes a
background JOB id, and cannot address such a record either. What
`kind: "interactive"` records are, and why no user-facing string says who owns
the signalled process, is in
[DOMAIN.md](DOMAIN.md#what-kind-interactive-denotes).

Two of these, **`attach`** and **`stop`**, were hidden from `claude --help` in the
2.1.220 capture and have been listed there since 2.1.280 (see
[below](#background-session-commands)). `attach`/`stop` take the **short
agent-view job id** (e.g. `ca56b543`), NOT the full `sessionId`; passing a UUID
returns exit 1 ("No job matching"). The `-r` resume/fork/send paths take the
**full `sessionId`**.

`[--model <alias>]` is a compose box's `Ctrl-L` pick. Which launches may carry it
is the A MODEL IS PICKED PER COMPOSE, NEVER PER BOARD rule in
[AGENTS.md](../../AGENTS.md#critical-rules), and which builder emits it for each
launch is [DOMAIN.md](DOMAIN.md#compose-model-pick-ctrl-l)'s table. It is emitted
ONLY for an explicit pick — with none, every argv that can carry it is
byte-identical to its modelless form, so a reply normally keeps the session's last
model and a new session runs on whatever `claude` gives one (see
[Which model a launch runs on](#which-model-a-launch-runs-on-without---model)) —
and it is always placed BEFORE a trailing positional (the new-session prompt, the
reply message), since a flag trailing an operand is at the mercy of the parser.
The `claude` behaviour that rule rests on: a `-r` launch without `--model`
normally restores the session's own model (the exceptions are
[below](#which-model-a-launch-runs-on-without---model)), and `claude attach` joins
a process already running under a model. `--agent` and `--model` compose freely,
and snapback emits both when both are set; the accepted alias set is below.

`[--effort <level>]` is the same pick's optional effort (`←`/`→` inside that
compose's model picker). It lives INSIDE the model pick (`resume::ModelPick`), so
it is emitted only together with `--model`: immediately after it, before any
trailing positional, through the one `resume::push_model_flag` guard — a pick
whose model is blank emits neither flag. With no effort each of those argvs is
byte-identical to its `--model`-only form, and no row that cannot carry a model
can carry an effort. What claude does with the value is
[below](#effort-levels---effort).

### `/cd`: moving a session to another folder

claude's OWN interactive local command, and no longer an argv snapback builds:
the `Ctrl-X w` move is [`set_cwd`](#set_cwd-moving-a-session-without-leaving-the-board).
It still matters here twice: the `relocated` record it writes is what
`store::parse` reads, and a `set_cwd` refused with `needs_trust` points the user
at it, because only its dialog can grant the trust. Observed at `claude 2.1.284`
(2026-10-02) in a throwaway `CLAUDE_CONFIG_DIR` profile with a fabricated
transcript and a real git worktree:

- An interactive `claude -r <id> "/cd <abs path>"` dispatches `/cd` as a LOCAL
  command: no model turn, no assistant record. The transcript gains `system` /
  `local_command` records (`<command-name>/cd</command-name>`, then
  `<local-command-stdout>Moved to …`), a `{"type":"relocated","relocatedCwd":…,
  "sessionId":…}` record, and an `isMeta` user record carrying a system-reminder
  that the directory changed. The file (and its sibling `<id>/` dir) MOVES from the
  old `<encoded-cwd>` project dir to the target's.
- Claude derives a session's folder as `relocatedCwd ?? headCwdStrict` and merges
  the `relocated` record last-wins; [DOMAIN.md](DOMAIN.md#jsonl-record-model) has
  snapback's reading of it.
- `/cd` moves at once into a folder trusted through a trusted parent, and shows
  claude's OWN trust dialog ("Yes, move here / No, stay put") otherwise; snapback
  adds none.
- With an inherited `CLAUDE_CODE_CHILD_SESSION` marker claude prints "Transcript
  saving is off" and `/cd` still reports "Moved to" while NOTHING moves on disk.
- Run from the target, `/cd` has nothing to move ("Already in").

### `set_cwd`: moving a session without leaving the board

> **Captured against `claude 2.1.291`, 2026-10-08**, by probe in a throwaway
> `CLAUDE_CONFIG_DIR` profile (fabricated transcripts, a real git worktree), plus
> two details read from the bundle (one at 2.1.284, one at 2.1.291), each marked
> below. Pinned separately from
> the [command surface](#version-pin-self-healing) (still 2.1.282): this is an
> UNDOCUMENTED stream-json control request, not a `--help` capture, and the Agent
> SDK docs say "Neither SDK has a setter for `cwd`". The version-pin refresh is
> the only thing that would catch a change, so re-probe this section whenever the
> installed `claude` moves past 2.1.291.

`src/claude_move.rs` drives it, on a worker thread of its own; the argv is the
[table row above](#how-snapback-drives-claude). The trade, decided: the move
never touches the terminal and the user stays on the board, in exchange for
depending on an undocumented request and carrying an in-flight guard while the
child runs.

**Wire shape.** One stdin line, then EOF:

```json
{"type":"control_request","request_id":"snapback-move","request":{"subtype":"set_cwd","path":"<abs target>"}}
```

and one answer among claude's stream-json output lines:

```json
{"type":"control_response","response":{"subtype":"success","request_id":"snapback-move","response":{"status":"ok","cwd":"<abs target>","changed":true,"transcript_relocated":true}}}
```

- `status: "needs_trust"` carries `directory` and, optionally, `trust_root`.
- `status: "rejected"` carries `reason` (`unsafe_path`, `not_found`,
  `not_a_directory`, `blocked_by_rule` or `busy`) and `message`.
- `path` is a JSON string, so a folder with a space needs no quoting.

**Observed:**

| Run | Answer | Transcript moved? |
| --- | --- | --- |
| Target inside a trusted repo | `ok`, `changed: true` | Yes, with a `relocated` record |
| Same, plus `--no-session-persistence` | `ok`, `changed: true` (`transcript_relocated: true`) | No: the success answer was false |
| Target outside any trusted folder | `needs_trust` | No, but three metadata records were appended |
| Worktree path containing a space | `ok`, `changed: true` | Yes |

- **Two false success shapes.** `ok` alone is not a move. A target equal to the
  folder the child runs in answers `ok` with `changed: false` and relocates
  nothing (read from the 2.1.284 bundle). With `--no-session-persistence` claude
  answers `ok`, `changed: true` while nothing moves (the table's second row). So
  `claude_move::parse_set_cwd_response` counts a move only on `status == "ok"`
  AND `changed` being the JSON boolean `true`, and `build_set_cwd_argv` never
  passes `--no-session-persistence`, unlike the catalog fetch. Both are pinned by
  tests.
- **Spawn in the CURRENT folder, never the target.** That is the first shape's
  cause: the child must start where the session is, so it has something to move.
  The worker re-reads that folder from inside the transcript at move time
  (`resume::plan_at`), and refuses a target that resolves to it.
- **A refused move still writes.** A `needs_trust` refusal appended `atis-latch`,
  `mode` and `cost-state` metadata records to the transcript. Harmless: the file
  stays where it was, and the next reload re-parses that one file.
- **No model call.** The only `user` record claude adds is the same hidden
  "working directory changed" note an interactive `/cd` writes; no assistant
  record follows, and the cost is $0. So the argv carries no `--model` or
  `--effort`.
- **No hooks.** With `--settings '{"disableAllHooks":true}'` no hook record was
  appended.
- **Timing and the child.** The request is written and stdin closed at once;
  claude answers and exits in ~0.7 s total. The child runs through the catalog
  fetch's machinery (`claude_catalog::exchange_reaped`): stdin and stdout pipes,
  stderr discarded, a process group of its own, one deadline
  (`claude_move::MOVE_TIMEOUT`, 30 s, for a transcript `-r` takes long to load),
  then the group SIGKILL and the reap ([below](#the-initialize-control-handshake-compose-pick-list)).
- **Trust is claude's.** `-p` never shows claude's trust dialog; it answers
  `needs_trust` instead. The request accepts a host's answer to that dialog,
  `trust_accepted: true` with a `trusted_directory` echoing the `needs_trust`
  folder, which its schema says to send only after showing the user one (read
  from the 2.1.291 bundle, 2026-10-08). snapback never sends it and never claims
  trust on the user's behalf: the status line points at `Enter`, then
  `/cd <target>` once, so claude's own dialog asks
  ([`/cd`](#cd-moving-a-session-to-another-folder)).

**Unverified: a session whose CURRENT folder claude does not trust.** Every probe
above started in a trusted folder. A move out of a folder claude does not trust
takes the catalog's
[untrusted form](#workspace-trust-what-an-untrusted-folder-can-run-and-the-two-argv-forms),
`--setting-sources user` on the argv and `CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS=1`
in the child's environment, chosen by the same `claude_trust::folder_trust`
verdict on the move's worker. Whether claude still honours `set_cwd` in that form
was not probed. `claude_move`'s tests pin that the form is what the child
receives (the exact argv, the environment, and a stand-in child reporting both),
not what claude does with it.

**Also unverified for `set_cwd`:** an inherited `CLAUDE_CODE_CHILD_SESSION` turned
transcript saving off for an interactive `/cd` at 2.1.284 (above), which is the
`--no-session-persistence` shape again. Whether it does the same to this request
was not probed.

**Not probed either: a session another process is writing.** Whether the `-p -r`
child refuses such a session before it answers `set_cwd` is unknown, so snapback
asks first: the move's worker refuses a session the bare `claude agents --json`
lists, and, unlike every other gate, it also refuses when that probe cannot
answer (`MOVE_PROBE_FAILED_REFUSAL`; why the directions differ is
[DOMAIN.md](DOMAIN.md#why-the-gate-does-not-read-the---all-map)'s). A probe
showing that claude refuses a live session here itself is the evidence that
would reopen that direction.

**Re-verify** at a version-pin refresh, never against the real store: in a
throwaway profile under `/tmp`, outside any repository, with every
`CLAUDE*`/`ANTHROPIC*` variable removed from the environment, create a git repo
with one worktree, mark the repo trusted in `<profile>/.claude.json`
(`projects[<repo>].hasTrustDialogAccepted = true`, plus `hasCompletedOnboarding`),
and place a minimal two-record transcript under
`<profile>/projects/<encoded repo>/<id>.jsonl`. From the repo, run the table row's
trusted argv with `-r <id>` and pipe it the one request line for the worktree.
Expect `ok` with `changed: true`, the transcript under the worktree's
`<encoded-cwd>` with a `relocated` record and no assistant record. Then expect the
false `ok` with `--no-session-persistence` added, and `needs_trust` for a target
outside the trusted repo. Any other answer is a reason to re-probe this section
before trusting the move.

## Invocation form

```
claude [options] [command] [prompt]
```

Interactive session by default. With no `command`, a trailing `prompt` (or stdin)
is sent to a session. `-p/--print` switches to non-interactive one-shot output.

### The trailing positional AUTO-SUBMITS, and there is no pre-fill

The positional (`--help`: "Your prompt") becomes the session's **first turn**,
sent the moment claude starts. There is **no** way to put text in claude's input
box UNSUBMITTED for the user to edit first — the whole flag surface was checked
for one, and the two candidates are not it:

- `-n, --name <name>` sets a session **display name** (prompt box, `/resume`
  picker, terminal title). It labels the session; it puts nothing in the input.
- The `--input-format` / `--output-format` / streaming-input family is documented
  **`--print`-only** ("only works with --print"), i.e. non-interactive SDK mode —
  the opposite of an interactive session.

So the positional is the only mechanism, and its auto-submitting behaviour is a
property of the CLI. Any snapback surface that offers it must be worded as "run
interactively" and must NOT imply a review step — `resume::build_new_argv` owns
that reasoning in code, and the key-doc surfaces listed in
[AGENTS.md](../../AGENTS.md) carry the wording.

### `--bg` can fail SILENTLY on a zero exit

`claude --agent <unknown-name> --bg <prompt>` exits **0**: it warns on `stderr`
that the agent is unknown and starts the background session **without** it. Exit
status alone therefore cannot distinguish "started as asked" from "started as
something else", which is why `send::status_for_bg_launch` treats a zero exit with
a non-empty `stderr` as a distinct *started-but-warned* outcome rather than the
neutral success. Do not simplify that seam back to an exit-code check.

### `-p` can also fail SILENTLY on a zero exit

The one-shot send has the same hazard, so the same seam. When a `-p` turn leaves
background tasks running past claude's own wait ceiling, claude writes
`Background tasks still running after <n>s; terminating.` to **stderr**, kills
those tasks, and exits **0** with an ordinary success payload on stdout
(upstream: `anthropics/claude-code#95789`). A `Ctrl-R` reply that just had its
background agent terminated therefore prints a clean, priced `sent — $0.0136`
unless stderr is consulted — which is why `send::status_for_output` carries the
same *zero exit + non-empty stderr ⇒ warned* row as the launch seam.

Two properties of that seam are deliberate and must survive any simplification.
It does NOT match on the message's wording — snapback does not own that string,
and the next zero-exit downgrade will not be this one — and "non-empty stderr"
means what survives sanitizing, so a blank or all-escape stream degrades to the
ordinary success rather than to a fabricated failure.

snapback neither sets nor overrides claude's background-wait ceiling: the ceiling
is claude's to own, and snapback's job is only to REPORT what it did. Do not add
an env override to paper over it.

## The `initialize` control handshake (compose pick list)

> **Captured against `claude 2.1.284`, 2026-09-30.** Pinned separately from the
> [command surface](#version-pin-self-healing) (still 2.1.282): this is a probe of
> one undocumented wire exchange, not a `--help` capture.
> **Added 2026-10-02, same `claude 2.1.284`:** the built-ins claude hides from its
> own `/` menu, the two skill visibility flags against a transcript's
> `skill_listing`, and the two agent-mention forms, by probe, by reading the same
> bundle and from claude's docs.
> **Also read 2026-10-02, from the 2.1.284 bundle and the 2.1.280 / 2.1.282
> bundles:** claude's print-mode command gate, and the hidden commands whose name
> is a variable or a factory argument.

The compose pick list's PRIMARY source, asked for by `src/claude_catalog.rs`. What
the list does with the answer is
[DOMAIN.md](DOMAIN.md#compose-pick-list)'s; this section records only what
`claude` did. Every probe ran with cwd = a fresh `mktemp -d` directory, with
`~/.claude/projects` listed before and after, and every directory a probe created
was removed afterwards.

**Wire shape.** The argv is the [table row above](#how-snapback-drives-claude).
`--verbose` is REQUIRED: without it claude exits 1 with
`Error: When using --print, --output-format=stream-json requires --verbose`. Stdin
carries one line, `{"type":"control_request","request_id":"<id>","request":{"subtype":"initialize"}}`,
and the answer is one stdout line:

```json
{"type":"control_response","response":{"subtype":"success","request_id":"<id>","response":{"commands":[…],"agents":[…],…}}}
```

- `commands` is an array of `{name, description, argumentHint[, aliases][, builtin]}`
  and `agents` an array of `{name, description[, model]}`.
- The body carries 17 more keys snapback ignores (`models`, `output_style`, `pid`,
  `session_state`, …). One of them, `account`, carries the user's account
  identity: `claude_catalog` never reads it, and a captured reply must never be
  logged, pasted or committed with it.
- Every command and agent observed carried a non-empty `description`. User and
  project command descriptions end in a source tag (` (user)` / ` (project)`);
  bundled and built-in ones carry none.
- An unknown subtype answers
  `{"type":"control_response","response":{"subtype":"error","request_id":"<id>","error":"Unsupported control request subtype: …"}}`.
- Size: ~34 KB for 74 commands and 15 agents.

A trimmed sample (descriptions shortened, every other body key removed; the
parser's fixture, `tests/fixtures/claude_catalog/initialize_response.json`, is a
six-command, three-agent cut of the same reply with its descriptions whole):

```json
{"type":"control_response","response":{"subtype":"success","request_id":"sb-v","response":{
  "commands":[
    {"name":"sbz","description":"Probe command Z for snapback (project)","argumentHint":"word"},
    {"name":"cr-review","description":"Independently review the current branch's changes … (user)","argumentHint":""},
    {"name":"code-review","description":"Review the current diff, …","argumentHint":"[low|medium|high|xhigh|max] …","aliases":["review"],"builtin":true},
    {"name":"compact","description":"Free up context by summarizing the conversation so far","argumentHint":"<optional custom summarization instructions>","builtin":true}],
  "agents":[
    {"name":"Explore","description":"Fast read-only search agent for locating code. …","model":"inherit"},
    {"name":"sby-agent","description":"Probe agent Y for snapback","model":"haiku"}]}}}
```

**No model call.** No `result` message, no cost and no `assistant` message in any
run: no user message is ever sent.

**Latency** (wall clock, request written then stdin closed): default flags 0.69 s
cold, then 0.50–0.53 s and 0.39 s; `--strict-mcp-config` alone 0.29 s; snapback's
flags 0.21–0.22 s, the reply line at ~0.19 s.

**Hooks and MCP.** By default the user's `SessionStart` hooks FIRE (their
`{"type":"system","subtype":"hook_started","hook_name":"SessionStart:startup",…}`
lines come first) and MCP servers START. Neither delays the reply much, but a slow
hook delayed the EXIT ~2 s after EOF. MCP prompts are NOT in the reply: they
arrive later as `{"type":"system","subtype":"commands_changed","commands":[…]}`
lines (74 → 79 → 81 names, each `<server>:<prompt> (MCP)`). With
`--settings '{"disableAllHooks":true}'` no hook event is emitted and the command
and agent lists are identical (74/15), which is why the fetch passes it beside
`--strict-mcp-config`, in BOTH argv forms: the fetch is not a session the user
started. Hooks and MCP servers are not all a folder's settings can run, though:
its `env` block and helper commands such as `apiKeyHelper` apply too, and neither
flag stops them. Where claude does not trust the folder, `--setting-sources user`
is what keeps the repository's settings out
([Workspace trust](#workspace-trust-what-an-untrusted-folder-can-run-and-the-two-argv-forms)).
Nor is everything that runs a setting: claude's own git prefetch is not, so
neither `disableAllHooks`, `--strict-mcp-config` nor `--setting-sources user`
stops it (the same section). `--bare` (43/5) and `--setting-sources ''` (46/5)
DROP the user's skills and agents, so neither is usable; `--setting-sources user`
keeps them.

**EOF, SIGTERM and SIGKILL.** With stdin a real pipe (what `Stdio::piped()`
gives), closing it after the request makes claude answer and exit 0, 25–50 ms
after EOF with snapback's flags (~2 s with the defaults, the slow hook above). A
harness that fed stdin through a named FIFO saw the child ignore EOF for more
than 20 s — a macOS FIFO artifact, reproduced also when an unrelated child
inherited the FIFO's write end, and never seen with a pipe; it is why the fetch
carries a timeout and a kill all the same. SIGTERM exits 143 and claude removes
its registry files; SIGKILL exits 137 and leaves `~/.claude/sessions/<pid>.json`,
`<pid>.<hash>.key` and `/tmp/cc-socks/<pid>.sock` behind (the `.json` is pruned by
claude's next `claude agents --json --all`). The fetch's kill is that SIGKILL,
sent to the child's process group AND to the child itself: the child is spawned as
the leader of a group of its own, and a timed-out fetch ends that group with
`killpg(2)` (`claude_catalog::kill_group`) and the child with `Child::kill`
([PATTERNS.md §6](PATTERNS.md#6-off-ui-thread-for-anything-that-can-block) owns
the worker's contract and why it needs both). A timeout is the only path that
kills: stdout closed by 0.24 s in every form probed, so a normal fetch is never
signalled.

**No transcript.** Across 20+ handshake runs — every flag variant, immediate and
held-open stdin, SIGTERM and SIGKILL — no `<encoded-cwd>` folder and no `.jsonl`
appeared in `~/.claude/projects`, so the fetch cannot put a stub row on the board;
`--no-session-persistence` is passed anyway. What EVERY run leaves inside claude's
own state, as any `claude -p` snapback spawns does: an empty
`~/.claude/session-env/<session-id>/` directory, a transient
`~/.claude/sessions/<pid>.json` (above) and a
`~/.claude/backups/.claude.json.backup.<ts>`; no `~/.claude.json` project entry for
the folder. While alive (~0.2 s) the child is listed by `claude agents --json --all`
as `kind: "interactive"`, `status: "idle"`, with a `sessionId` that has no
transcript. The MODEL-CALL probes below did create
`~/.claude/projects/<encoded>/memory/` and `<id>/subagents/*.meta.json`, even under
`--no-session-persistence`; the handshake makes no model call and never did.

**`cwd` scopes the answer.** A folder holding `.claude/skills/sbx-skill/SKILL.md`,
`.claude/commands/sbz.md` and `.claude/agents/sby-agent.md` listed `sbx-skill`
(`… (project)`), `sbz` (`argumentHint: "word"`) and the agent `sby-agent`
(`model: "haiku"`); a second fresh folder listed none of them (77/16 against
74/15). That was the trusted form's argv in a folder claude never trusted. Print
mode skips the workspace-trust dialog, so that argv LOADS such a folder's project
settings, its `env` block and helper commands with them, not only its items (only
its hooks stay off, through `disableAllHooks`). A folder claude does not trust is
therefore fetched in the untrusted form, which lists none of its own items
([Workspace trust](#workspace-trust-what-an-untrusted-folder-can-run-and-the-two-argv-forms)).

**Built-ins, hidden built-ins and two visibility flags.** Built-in commands carry
`builtin: true`: `clear` (aliases `reset`, `new`), `compact`, `model`, `context`,
`init`, `usage`, `code-review` (alias `review`), …. `/exit` is absent. Found on
2026-10-02, by reading the 2.1.284 bundle (and, for the gate and the drift, the
2.1.280 and 2.1.282 bundles) and by probe:

- **claude hides some built-ins from its own `/` menu, and the reply cannot say
  which.** A command object may declare `isHidden`. claude's interactive `/`
  typeahead skips every hidden command, with one exception: a hidden command whose
  name EXACTLY equals the typed query is offered first. There is no `__` naming
  rule and no feature flag on that path. The `initialize` reply never carries the
  flag: its `commands` pass claude's print-mode gate (next bullet), then one
  filter (`REr`, below), then one mapper (`Znn` in the bundle) that emits exactly
  `{name, description, argumentHint, aliases?, builtin?}`. `reload_plugins` and
  `reload_skills` answer through the same mapper, and `get_skills_dialog` lists
  skills only. A menu-facing variant that
  does skip hidden commands exists, but only the remote-control bridge uses it,
  behind a server flag (`tengu_bridge_initialize_commands`, default off). So no
  control request snapback could send says which built-in is hidden, and a hidden
  one has the same shape as a visible one:

  ```json
  {"name":"__remote-workflow","description":"Run the workflow script …","argumentHint":"","builtin":true}
  {"name":"compact","description":"Free up context …","argumentHint":"<optional custom summarization instructions>","builtin":true}
  ```

- **Only a command claude's print-mode gate passes can reach the reply.** The
  gate is one predicate, `function lxe(e){return e.type==="prompt"&&!e.disableNonInteractive||e.type==="local"&&e.supportsNonInteractive}`:
  it keeps a `local` command whose `supportsNonInteractive` is truthy and a
  `prompt` command without `disableNonInteractive`, and a `local-jsx` command
  never passes. Its wrapper (`yae`) returns an empty list when slash commands are
  disabled (`disableSlashCommands()`). The stdin `initialize` handler answers
  `{commands: jK(e), …}` over `Yn()`, whose merge (`zyt`) joins the gated list
  (`Ts`, every assignment of which is the wrapper's result) with the MCP commands
  (prompts, never built-ins) and dedups against synced skills. Those minified
  names are 2.1.284's; 2.1.282 (`KTe`) and 2.1.280 (`_Te`) carry the same gate
  body under other names. So a hidden command the gate refuses can never be in the reply,
  and only a hidden built-in that passes it needs the pinned list.
- **Five built-ins in the reply are hidden in claude's own menu.** The reply held
  76 commands (46 `builtin: true`) and 17 agents. Five of the built-ins declare a
  LITERAL `isHidden: !0`: `__remote-workflow`, `workflow-launch-exec`, `heapdump`
  ("Dump the JS heap to the Desktop…"), `agents` ("(removed) Ask Claude to
  create/manage subagents…") and `extra-usage` ("Renamed to /usage-credits").
  Every other one has no `isHidden`, or an `isHidden` GETTER that is false
  whenever the command is enabled in print mode (`model`, `config` and `usage`
  also have a visible interactive twin of the same name). 21 gate-passing
  commands carry such a getter at 2.1.284 and 2.1.282 (20 at 2.1.280). Most read
  `return!Ce()`, where `Ce()` is `!isInteractive()`; `import`
  (`!uge()||!Ce()` against `isEnabled:()=>uge()&&Ce()`) and `advisor`
  (`!Ce()||!xw()` against `isEnabled:()=>Ce()&&xw()`) are true in print mode only
  while the command is disabled there. A getter that turns true only in an
  interactive session (`fast` without fast mode) is not mirrored: the reply
  carries neither its condition nor its result.
- **The bundle's literal-hidden commands, through the gate.** The 2.1.284 bundle
  carries 34 literal `isHidden: !0`. 18 are typeless
  `{isEnabled:()=>!1,isHidden:!0,name:"stub"}` placeholders and one is the
  `{...g,isHidden:!0}` copy `zyt` makes of an MCP duplicate, so neither kind
  declares a command. The other 15 objects declare 21 commands under 20
  names: two factory objects build eight of them, and `extra-usage` is declared
  twice.

  | Name | Name declared as | Type | Gate |
  | --- | --- | --- | --- |
  | `__remote-workflow`, `agents`, `design-consent`, `design-revoke`, `heapdump`, `workflow-launch-exec` | literal | `local`, `supportsNonInteractive: !0` | PASS |
  | `extra-usage` | literal, twice | a `local` twin with `supportsNonInteractive: !0`, and a `local-jsx` twin | PASS (the `local` twin) |
  | `update` | literal | `local`, `supportsNonInteractive: !1`, `isEnabled: () => !1` | FAIL |
  | `pro-trial-expired` | literal | `local-jsx` | FAIL |
  | `claim-credit`, `low-priority` | a variable (`KOt`, `ble`) | `local`, `supportsNonInteractive: !1` | FAIL |
  | `limit-reset` | a variable (`BK`) | `local-jsx` | FAIL |
  | `vim`, `output-style` | factory `S1t(e,n,r)`'s first argument | `local-jsx` | FAIL |
  | `ultraplan`, `ultrareview`, `teleport`, `remote-control`, `schedule`, `autofix-pr` | upsell factory `rB(e)`'s `e.name` | `local`, `supportsNonInteractive: !1` | FAIL |

  The seven that pass are `claude_catalog::CLAUDE_HIDDEN_BUILTINS`: the hidden
  built-ins that can reach the reply. `design-consent` and `design-revoke` pass
  the gate, but the print-mode reply probed on 2026-10-02 did not list them. The
  const drops a reply entry only when its `builtin` is the JSON boolean `true`
  and its name is listed, so a user or project skill of the same name is still
  offered. Both sets drift between releases, so the re-verify steps below
  re-derive them:

  | claude | Gate-passing hidden built-ins | Literal-hidden, gate-failing |
  | --- | --- | --- |
  | 2.1.280 | `__remote-workflow`, `design-consent`, `design-revoke`, `extra-usage`, `heapdump`, `workflow-launch-exec` (6) | `pro-trial-expired`, `rate-limit-options` (`local-jsx`), `update`, `claim-credit`, `low-priority`, `limit-reset`, `vim`, `output-style`, the six upsells |
  | 2.1.282 | adds `agents` (7) | unchanged |
  | 2.1.284 | unchanged (7) | drops `rate-limit-options` |

  A pinned list fails OPEN: a built-in a later claude hides is listed until the
  const catches up, and one it removes is never listed.
- **`/__remote-workflow` in `claude -p`**, which is what a quick reply runs, ran
  locally with no model call (`total_cost_usd` 0, `num_turns` 0,
  `is_error: false`) and answered `remote-workflow: error[not-remote-session]:
  this command only runs inside a remote (CCR) session (CLAUDE_CODE_REMOTE is not
  set). Use the Workflow tool locally.` It is not rejected, only useless outside a
  server-launched session, and a real reply would write it into the session's
  transcript.
- **The two skill flags.** A skill or legacy command with
  `user-invocable: false` is EXCLUDED from the reply, and a skill with
  `disable-model-invocation: true` is INCLUDED (first seen 2026-09-30; re-verified
  2026-10-02 with one of each beside a control skill). The reply's filter is
  `userInvocable !== false` (`REr` in the bundle), and a bundled, file or plugin
  skill's `isHidden` derives from that same flag, so every hidden SKILL is already
  out of the reply and only hidden BUILT-INS need the pinned list. claude's docs
  (code.claude.com/docs/en/skills, read 2026-10-02) agree: `user-invocable: false`
  means "Claude Code hides it from the `/` menu and doesn't run it when you type
  `/name`". Typing one in `claude -p` is refused locally, with no model call:
  both a `user-invocable: false` project skill and the bundled
  `/keybindings-help` (`userInvocable: !1` in the bundle) wrote the user record
  `This skill can only be invoked by Claude, not directly by users. Ask Claude to
  use the "<name>" skill for you.`
- **A transcript's `skill_listing` is the MODEL's list, not the menu's.** claude
  builds it from the bundled skills without `disable-model-invocation`, plus the
  user, project and plugin skills, so it is the exact opposite of the reply on
  both flags: it INCLUDES `user-invocable: false` skills and OMITS
  `disable-model-invocation: true` ones. Its record,
  `{"type":"skill_listing","names":[…],"content":"- <name>: <description>\n…","skillCount":…,"isInitial":…}`,
  carries no per-skill flag, so nothing reading it can tell which names claude
  refuses when typed. A read-only scan of one local store found the bundled
  `keybindings-help` in 179 of its 184 session files. This is why a reply's `/`
  lists claude's catalog alone and `store::skills` never reads these records
  ([DOMAIN.md](DOMAIN.md#compose-pick-list)).

**`@agent-<name>` works in `claude -p`, and is the form to insert.**
`claude -p --model haiku
--disallowed-tools "Read,Bash,Glob,Grep,Edit,Write,WebFetch,WebSearch,NotebookEdit"
-- '@agent-sby-agent Say the single word hello.'` (≈ $0.01–0.03 a run) made the
main thread call `Agent` with `subagent_type: "sby-agent"`, which answered, and
the persisted transcript carried
`{"type":"attachment","attachment":{"type":"agent_mention","agentType":"sby-agent"}}`
— a structured mention, not model inference. The 2.1.284 binary's mention
extractor accepts two forms, `@"<name> (agent)"` and `@agent-<name>`, with
`<name>` in `[\w:.@-]+` (`\w` is ASCII). claude's own typeahead labels an agent
`<name> (agent)` and inserts `@"<name> (agent)"` (read from the binary, not
observed in a UI). `@agent-<name>` is the verified form, the one claude's own
telemetry compares against, and a single whitespace-free token. The `--bg` path
could not be probed in isolation (`claude --bg` in an untrusted temp dir refuses:
"Workspace not trusted"); the store is the evidence instead: of 142 transcripts
born background (`sessionKind: "bg"` on the first user record), 33 open with an
expanded slash command and 3 carry an `agent_mention` attachment. An interactive
run's trailing prompt is believed to go through the same input processing (it
auto-submits as an ordinary first turn, [above](#the-trailing-positional-auto-submits-and-there-is-no-pre-fill));
that was not probed, since it needs a TTY.

claude's docs give `@agent-<name>` as the form to type by hand
([sub-agents](https://code.claude.com/docs/en/sub-agents), "Invoke subagents
explicitly", read 2026-10-02): "You can also type the mention manually without
using the picker: `@agent-<name>` for local subagents, or `@agent-` followed by
the scoped name for plugin subagents, for example
`@agent-my-plugin:code-reviewer`. While you type this form the typeahead shows
file matches rather than agents. The agent mention still resolves when you
submit." It is also the more robust form. Re-probed on 2026-10-02 in `claude -p`
(`--model haiku --tools ""` and a one-line system prompt, ≈ $0.001–0.002 a run),
a run's transcript carrying an `agent_mention` attachment only when the mention
resolved:

| Prompt | `agent_mention` |
| --- | --- |
| `@agent-sbq-agent …` | `sbq-agent` |
| `@"sbq-agent (agent)" …` | `sbq-agent` |
| `@agent-my-agent-x …` | `my-agent-x` |
| `@"my-agent-x (agent)" …` | none |

The quoted form loses every agent whose name CONTAINS `agent-`: claude resolves
an extracted name with `replace("agent-","")` (`g9o` in the bundle), which cuts
the first `agent-` out of a quoted bare name, while the `@agent-` form loses only
its own prefix and so resolves every name in the class. A plugin agent's `:` is
in the class, as the docs' example shows. A name with a space or a non-ASCII
character cannot be mentioned in either form, which is what
`complete::is_mention_safe` skips. One side path: the unquoted token also passes
claude's FILE-mention extractor (`@([^\s]+)\b`), so claude looks for a file named
`agent-<name>` in the folder; with none it is skipped silently, and every run
above answered normally. The quoted form stays off that path, its one advantage.
So `complete::AGENT_MENTION_PREFIX` keeps `@agent-`. An interactive session still
was not driven: for it the form rests on the docs ("still resolves when you
submit") and on the extractor it shares with `-p`.

To re-verify after a `claude` update, re-run the handshake — never the model-call
probe — in a throwaway folder, and print only the counts, so the `account` key
never reaches the terminal:

```sh
d="$(mktemp -d)" && cd "$d" \
  && printf '%s\n' '{"type":"control_request","request_id":"probe","request":{"subtype":"initialize"}}' \
   | claude -p --input-format stream-json --output-format stream-json --verbose \
       --no-session-persistence --strict-mcp-config --settings '{"disableAllHooks":true}' \
   | grep '"control_response"' \
   | jq '.response | {subtype, commands: (.response.commands | length), agents: (.response.agents | length)}'
cd - >/dev/null && rm -rf "$d"
```

**Re-derive the hidden built-ins.** Run it from the repository root. It reads the
installed binary and `src/claude_catalog.rs` and runs nothing; it needs `python3`
(macOS ships it with the Command Line Tools) and took about 25 s per bundle. It
finds the print-mode gate by its whole body, takes every command object declared
with a literal `isHidden: !0`, and resolves its name: a literal as is, a variable
to the nearest string literal assigned to it (printing how many candidates there
were), a factory parameter to every call site's literal. It applies the gate,
lists the typeless objects it skipped and the getter-hidden commands that pass
the gate, and compares the gate-passing names with `CLAUDE_HIDDEN_BUILTINS`:

```sh
B="$(readlink -f "$(command -v claude)")"
python3 - "$B" src/claude_catalog.rs <<'PY'
import re, sys

data = open(sys.argv[1], "rb").read()
const_file = sys.argv[2] if len(sys.argv) > 2 else "src/claude_catalog.rs"
WINDOW = 6000  # bytes searched around a match for its enclosing object

# The print-mode gate, found by CONTENT: its minified name changes every build.
GATE = re.compile(rb'function ([\w$]+)\(([\w$]+)\)\{return \2\.type==="prompt"&&'
                  rb'!\2\.disableNonInteractive\|\|\2\.type==="local"&&\2\.supportsNonInteractive\}')
gates = GATE.findall(data)
if len(gates) != 1:
    print("!! print-mode gate found %d times, want 1: re-read it" % len(gates))
    sys.exit(2)
print("gate: %s(...)" % gates[0][0].decode())


def enclosing(pos):
    depth, i = 0, pos - 1
    while i >= max(0, pos - WINDOW):
        if data[i] == 0x7D:
            depth += 1
        elif data[i] == 0x7B:
            if depth == 0:
                break
            depth -= 1
        i -= 1
    else:
        return None
    depth = 0
    for j in range(i, min(len(data), i + 2 * WINDOW)):
        depth += {0x7B: 1, 0x7D: -1}.get(data[j], 0)
        if depth == 0:
            return i, j + 1
    return None


def top_level(obj):  # the object's own keys: nested {...} collapsed to {}
    out, depth = bytearray(), 0
    for c in obj:
        depth += c == 0x7B
        if depth == 1 or (c in (0x7B, 0x7D) and depth == 2):
            out.append(c)
        depth -= c == 0x7D
    return bytes(out)


def key(top, k):
    m = re.search(rb"[{,]" + k + rb":([^,}]*)", top)
    return m.group(1) if m else None


def gate(top):
    kind, sni, dni = key(top, rb"type"), key(top, rb"supportsNonInteractive"), key(top, rb"disableNonInteractive")
    if re.search(rb"get (supportsNonInteractive|disableNonInteractive|type)\(\)", top):
        return "UNKNOWN(getter)"
    if kind == b'"local-jsx"':
        return "FAIL"
    if kind == b'"local"':
        return "PASS" if sni in (b"!0", b"true") else "FAIL" if sni in (None, b"!1", b"false") else "UNKNOWN"
    if kind == b'"prompt"':
        return "PASS" if dni in (None, b"!1", b"false") else "FAIL" if dni in (b"!0", b"true") else "UNKNOWN"
    return "UNKNOWN"


def names(start, top):
    raw = key(top, rb"name") or b"?"
    if raw.startswith(b'"'):
        return [(raw.strip(b'"').decode(), "literal")]
    m = re.fullmatch(rb"([\w$]+)(?:\.([\w$]+))?", raw)
    if not m:
        return [("?" + raw.decode("latin-1"), "unresolved")]
    ident, field = m.groups()
    fac = re.search(rb"function ([\w$]+)\(([^)]*)\)\{[^{}]{0,200}return$", data[max(0, start - 400):start])
    if fac and ident in fac.group(2).split(b","):  # a factory: resolve its call sites
        fn = re.escape(fac.group(1))
        arg = rb'\("([^"]+)"' if field is None else rb'\(\{(?:(?:[^{}]|\{[^{}]*\})*?,)?' + field + rb':"([^"]+)"'
        found = sorted(set(re.findall(rb"(?:^|[^\w$.])" + fn + arg, data)))
        how = "factory %s(%s)" % (fac.group(1).decode(), fac.group(2).decode())
        return [(n.decode(), how) for n in found] or [("?" + raw.decode(), how)]
    if field is not None:
        return [("?" + raw.decode(), "unresolved")]
    # A minified name is reused across scopes: take the literal assigned nearest.
    hits = sorted((abs(m.start() - start), m.group(1)) for m in
                  re.finditer(rb"(?:^|[^\w$.])" + re.escape(ident) + rb'="([^"]*)"', data))
    if not hits:
        return [("?" + raw.decode(), "unresolved")]
    return [(hits[0][1].decode(), "variable %s (nearest of %d)" % (ident.decode(), len(hits)))]


rows, skipped, seen = set(), {}, set()
for m in re.finditer(rb"isHidden:(?:!0|true)(?![\w$])", data):
    span = enclosing(m.start())
    if span is None or span in seen:
        continue
    seen.add(span)
    top = top_level(data[span[0]:span[1]])
    if not re.search(rb"isHidden:(?:!0|true)(?![\w$])", top):
        continue
    if key(top, rb"type") is None:  # a stub or a spread copy, not a declaration
        shape = data[span[0]:span[1]][:60].decode("latin-1")
        skipped[shape] = skipped.get(shape, 0) + 1
        continue
    for name, how in names(span[0], top):
        rows.add((name, key(top, rb"type").decode("latin-1"), gate(top), how))

print("\nliteral isHidden command objects:")
for row in sorted(rows):
    print("  %-22s %-12s %-16s %s" % row)
print("skipped typeless objects:")
for shape, n in sorted(skipped.items()):
    print("  x%-3d %s" % (n, shape))

print("\ngetter-hidden commands that pass the gate (not mirrored; read each getter):")
for m in re.finditer(rb"get isHidden\(\)\{([^{}]*)\}", data):
    span = enclosing(m.start())
    if span is None:
        continue
    top = top_level(data[span[0]:span[1]])
    if key(top, rb"type") is not None and gate(top) != "FAIL":
        for name, _ in names(span[0], top):
            print("  %-22s %-16s %s" % (name, gate(top), m.group(1).decode("latin-1")))

reach = {n for n, _, g, _ in rows if g != "FAIL"}
text = open(const_file, encoding="utf-8").read()
const = re.search(r"CLAUDE_HIDDEN_BUILTINS[^=]*=\s*\[(.*?)\];", text, re.S)
have = set(re.findall(r'"([^"]+)"', const.group(1))) if const else set()
print("\ngate-passing hidden built-ins: %s" % ", ".join(sorted(reach)))
print("CLAUDE_HIDDEN_BUILTINS:        %s" % ", ".join(sorted(have)))
problems = [
    ("UNRESOLVED NAME", sorted(n for n in reach if n.startswith("?"))),
    ("MISSING FROM CLAUDE_HIDDEN_BUILTINS", sorted(n for n in reach - have if not n.startswith("?"))),
    ("IN CLAUDE_HIDDEN_BUILTINS BUT CANNOT REACH -p", sorted(have - reach)),
]
for label, found in problems:
    if found:
        print("!! %s: %s" % (label, ", ".join(found)))
if not const:
    print("!! CLAUDE_HIDDEN_BUILTINS not found in %s" % const_file)
if not const or any(found for _, found in problems):
    sys.exit(1)
print("OK: CLAUDE_HIDDEN_BUILTINS equals the gate-passing hidden built-ins")
PY
```

- **Exit 0** prints `OK: CLAUDE_HIDDEN_BUILTINS equals the gate-passing hidden
  built-ins`.
- **Exit 1** follows a `!!` line naming a mismatch: `UNRESOLVED NAME` (a
  gate-passing command whose name it could not resolve),
  `MISSING FROM CLAUDE_HIDDEN_BUILTINS`,
  `IN CLAUDE_HIDDEN_BUILTINS BUT CANNOT REACH -p`, or the const not found.
- **Exit 2** follows `!! print-mode gate found N times, want 1`: the gate's body
  changed. Re-read the gate before trusting the predicate the recipe carries.

At 2.1.284 (2026-10-02) it printed `gate: lxe(...)`, the rows of the first table
above and `OK`, exit 0. At 2.1.282 it printed `gate: KTe(...)`, the same rows
plus `rate-limit-options`, and `OK`, exit 0. Against 2.1.280 it exits 1 with
`IN CLAUDE_HIDDEN_BUILTINS BUT CANNOT REACH -p: agents`, as expected: the const
is pinned to 2.1.284. A `!!` line is a reason to update the const, its
provenance and both tables above together. If a later reply ever carries
`isHidden`, or a field like it, for a command, filter on that field instead and
delete the const.

**What it cannot see.**

- `isHidden` GETTERS: the gate-passing ones are printed for reading, never
  evaluated.
- An `isHidden` that is not a literal on the object itself (a variable, a spread
  base, a later assignment): it is never matched.
- The `command.describe` extension point, which can override a command's
  `isHidden` at run time.
- Spread copies such as `zyt`'s `{...g,isHidden:!0}`: listed among the skipped
  typeless objects, never resolved.
- A factory whose name is not its first argument (or that argument's `.name`),
  or that is not a `function` declaration: its name resolves to the wrong
  literal or to `?`.
- Its heuristics: an object is found by brace counting within 6,000 bytes, so a
  brace inside a string literal can miscount and an object whose `isHidden` sits
  farther from its opening brace is skipped; a variable resolves to the NEAREST
  literal assigned to that identifier.

An unresolved name on a gate-PASSING command ends in a `!!` line, and so does a
wrong one unless it happens to be a name the const already holds; on a
gate-failing command either is only a printed row. The getters' values, the
`describe` override, a non-literal `isHidden` and an object the brace counting
skips are silent. On the three bundles read on 2026-10-02 the brace counting skipped none:
the skipped and declaring objects add up to every literal `isHidden: !0`.

Then run the trust, git-prefetch and `rootOnly` checks at the end of
[Workspace trust](#workspace-trust-what-an-untrusted-folder-can-run-and-the-two-argv-forms).

### Workspace trust: what an untrusted folder can run, and the two argv forms

> **Captured against `claude 2.1.284`, 2026-09-30**, by probe and by reading the
> 2.1.284 bundle. Pinned with the handshake above, not with the
> [command surface](#version-pin-self-healing) (still 2.1.282). The docs quoted
> are code.claude.com/docs/en/permissions ("Project allow rules and workspace
> trust", "What runs before you trust a folder") and /docs/en/env-vars.
> **Added 2026-10-01, same `claude 2.1.284`:** claude's own git prefetch and the
> switch that turns it off, and the rule's dead `rootOnly` branch, by probe and
> by reading the same bundle.

`src/claude_trust.rs` mirrors the rule below, and `src/claude_catalog.rs` picks
the form, argv and child environment, from its verdict. What the pick list then
shows is [DOMAIN.md](DOMAIN.md#compose-pick-list)'s.

**What a never-trusted folder can run under `-p`.** `-p` never shows the
workspace-trust dialog. claude's docs table "What runs before you trust a folder"
marks these repository-supplied items "Used" for a `claude -p` run in a folder
never trusted:

- hooks in its settings files;
- its `env` block;
- helper commands such as `apiKeyHelper`;
- a project skill's hooks and `allowed-tools`.

Its `.mcp.json` servers are "Connected without asking". Only `permissions.allow`
and `additionalDirectories`, a few frontmatter items and an MCP `headersHelper`
are held back. The table leaves one thing out, found by probe: claude's OWN git
prefetch, through which the repository's `.git/config` runs ("Observed: claude's
own git prefetch", below). The trusted form's flags turn off hooks
(`disableAllHooks`) and MCP (`--strict-mcp-config`) and nothing else, so in such
a folder that form would
run the repository's helper commands and apply its `env` block the moment a
compose box opened, with nothing typed. For running `claude -p` in a repository you did not write, the docs name
`--setting-sources user` ("reads neither the project's settings files nor its
`.mcp.json`") and `--bare`. `--bare` still applies the project's `env` block and
helpers such as `awsAuthRefresh`, and it drops the user's own skills and agents
([above](#the-initialize-control-handshake-compose-pick-list)), so the fetch
never uses it.

**Observed: the trusted form in a never-trusted folder.** The probe folder's
`.claude/settings.json` declared:

- an `apiKeyHelper` that writes a marker file and echoes `x`;
- an `awsAuthRefresh` and an `awsCredentialExport`;
- `env: {"SB_PROBE_ENV":"1"}`;
- one `permissions.allow` rule.

The folder also held a project skill, a command and an agent. The run gave:

- **The `apiKeyHelper` RAN** (the marker was written) and became claude's auth
  source. The reply's `account.tokenSource` and `account.apiKeySource` read
  `"apiKeyHelper"` (only key names were recorded), and the OAuth identity fields
  vanished. stderr printed `⚠ claude.ai connectors are disabled because
  ANTHROPIC_API_KEY or another auth source is set …`. The list held 73 commands,
  the 2 project ones included. That is 3 fewer of the others than a plain folder's
  74, presumably built-ins that need the claude.ai login. It held 16 agents.
- **The `env` block was APPLIED.** A second folder whose settings held only
  `env: {"ANTHROPIC_DEFAULT_SONNET_MODEL":"sb-probe-env-model"}` changed the
  reply's `models[value=sonnet].resolvedModel` to `sb-probe-env-model`.
- **`awsAuthRefresh` and `awsCredentialExport` did NOT run.** No Bedrock provider
  was configured, so this is no evidence that they never run.
- **stderr warned about the allow rule:** `Ignoring 1 permissions.allow entry from
  .claude/settings.json: this workspace has not been trusted. … set
  projects["/private/tmp/snapback-probe-…"].hasTrustDialogAccepted: true in
  ~/.claude.json.`
- **`.claude/settings.local.json` behaved the same.** The helper ran and became
  the auth source. There was no allow-rule warning, because an untracked local
  file counts under `-p`.
- **The network is NOT verified.** A `--debug-file` run logged no request line,
  but the probing machine had nonessential traffic disabled, so that absence is
  not general. Whether a helper-supplied key or a project `ANTHROPIC_BASE_URL` is
  used for a request during the handshake is unknown.
- **Latency was unchanged:** 0.21–0.24 s.

**Observed: the same folders with `--setting-sources user` added.**

- **No helper ran.** No helper wrote its marker, the `env` block was not applied
  (sonnet resolved to its default), and the OAuth `account` fields came back.
  These folders were no git repositories. In one that is, claude's own git
  prefetch still runs ("Observed: claude's own git prefetch", below).
- **The lists kept the user's items and lost the repository's.** No project
  skill, command or agent was listed. The name lists equalled a plain folder's:
  74 commands (46 `builtin`, 28 tagged ` (user)`) and 15 agents.
- **`--settings` is still honoured.** A flag-settings
  `env.ANTHROPIC_DEFAULT_SONNET_MODEL` changed sonnet's `resolvedModel`, so the
  flag's `disableAllHooks` still applies. No `hook_started` line was emitted, and
  with `--strict-mcp-config` no `commands_changed` line either.
- **Latency was unchanged:** 0.22–0.23 s over 5 runs. stdout reached EOF at
  0.21–0.24 s in plain, `user` and trusted folders alike.

**Observed: a trusted folder.** This probe was read-only, in an already-trusted
repository holding one project skill, with no settings files and no `.mcp.json`.
The trusted form listed 75 commands, the skill among them tagged ` (project)`.
With `--setting-sources user` it listed 74 and the skill was gone. Nothing was
written to the repository.

**Observed: claude's own git prefetch** (2026-10-01). A `-p` run starts claude's
git-status prefetch with NO trust check: the headless startup path logs
`prefetch_system_context_non_interactive` and runs it, where an interactive
session skips it until the folder is trusted
(`prefetch_system_context_skipped_no_trust`). It is not a setting, so no setting
source stops it. The probe folder was a git repository whose own `.git/config` set
`filter.sbprobe.clean = "touch <marker>; cat"`, with `.gitattributes`
`* filter=sbprobe` and one tracked file rewritten at the SAME size with another
mtime, which `git status` can only settle by re-hashing that file through the
filter. A clone never carries a `.git/config` of the sender's making, but an
unpacked archive or a copied folder can. The run used the untrusted form's argv,
`--setting-sources user` included, with the `initialize` handshake:

- **The filter RAN** (its marker was written). A logging `git` shim first on the
  CHILD's `PATH` recorded six calls: `status --short --ignore-submodules=dirty`,
  `log --oneline -n 5`, `config user.name`, `remote get-url origin`, `remote`, and
  `ls-files --error-unmatch` on `.claude/settings.local.json`. The first five
  carried claude's forced `-c` set (`core.fsmonitor=`, `core.hooksPath=/dev/null`,
  `core.askPass=`, `protocol.ext.allow=never`, `submodule.recurse=false`,
  `log.showSignature=false`, `gc.auto=0`, `maintenance.auto=false`), the
  `ls-files` call only its first two. Nothing in that set closes a
  `filter.<x>.clean` or `filter.<x>.process` driver. claude's diagnostics file
  logged `git_status_started`, `git_commands_completed` and
  `git_status_completed`.
- **The switch.** `CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS` gates the prefetch
  (bundle `lxt`). claude reads it as a tri-state boolean BEFORE the
  `includeGitInstructions` setting: `1`, `true`, `yes` or `on` turn the prefetch
  off; `0`, `false`, `no` or `off` turn it on; anything else falls back to the
  setting, whose default is `true`. The same gate also decides the system
  prompt's git instructions and one remote-session change, and none of its
  readers builds the command or agent list. With the variable `1` in the
  CHILD's environment, the marker did not appear, the `status`, `log` and
  `config user.name` calls and their two diagnostics events were gone, and the
  `commands` and `agents` objects were byte-identical to the run without it (74
  and 15, the same SHA-256). One warm run each took 0.24 s without it and
  0.23 s with it.
- **Three git calls remain:** the `ls-files` call (claude's check for a tracked
  local settings file), `remote get-url origin` and `remote`. They read the index
  and the config and never hash working-tree content, so no filter, textconv,
  hook, fsmonitor or signature program runs through them: no known vector.
- **Nothing else found.** The bundle's other `-p` startup work under
  `--setting-sources user` is a ripgrep file count (`rg --files --hidden`,
  aborted after 3 s, reading ignore files only) and the CLAUDE.md read. Both only
  read files.
- **A trusted folder** (read-only, an already-trusted repository whose `git
  status` was clean) ran the same prefetch, since `-p` has no trust check, and
  the switch removed it there too, leaving the lists byte-identical. snapback sets
  it in the untrusted form alone: claude's own interactive session runs that
  prefetch in a trusted folder anyway, the user has accepted that folder's far
  larger settings surface, and the trusted form stays the argv and environment it
  always was, byte for byte.
- **The residual: a settings `env` block outranks the child's environment.** With
  the child's variable `1` AND
  `--settings '{"disableAllHooks":true,"env":{"CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS":"0"}}'`,
  the prefetch ran and the filter fired. Under `--setting-sources user` the `env`
  sources still loaded are the user's own settings, managed policy and the flag
  settings, and the flag settings are snapback's own. So only the user's own user
  settings or a managed policy holding a false value can turn the prefetch back
  on in an untrusted folder; a repository cannot. snapback sets the switch through
  the child's environment alone, by decision: it does not also write it into the
  flag settings' `env`, which would change the untrusted argv.

**The two forms**: the argv from `claude_catalog::build_catalog_argv(FolderTrust)`,
and what the child's environment gains over snapback's from
`build_catalog_env(FolderTrust)`. snapback's own environment is never written.

| claude's verdict for the folder | argv | child environment | lists |
| --- | --- | --- | --- |
| trusted | the [table row's](#how-snapback-drives-claude) argv, byte for byte | snapback's, untouched | the user's own, bundled and built-in items, AND the repository's own `.claude/` skills, commands and agents |
| anything else, including every case snapback cannot settle | the same, followed by `--setting-sources user` | snapback's, plus `CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS=1` | the user's own, bundled and built-in items only |

`disableAllHooks` and `--strict-mcp-config` are in both forms. The verdict comes
from `claude_trust::folder_trust`, read on the fetch's worker thread when the
fetch runs, and ONE verdict picks both halves of a fetch's form.

**Where claude records trust.** The record is claude's GLOBAL config:
`$CLAUDE_CONFIG_DIR/.claude.json` when that variable is set and non-empty, else
`~/.claude.json`. The env-vars docs say "`.claude.json` (the global config)
lives directly in the specified directory"; the bundle's functions are `Lo`,
`M7n` and `tq`. Two exceptions pick another file:

- A legacy `<config home>/.config.json`, when it exists, is read instead. The
  config home is `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
- When `CLAUDE_CODE_CUSTOM_OAUTH_URL` is set, the file is
  `.claude-custom-oauth.json`.

Trust is `projects["<key>"].hasTrustDialogAccepted`, a boolean. A key is the
folder's REALPATH: absolute, NFC-normalised, with no trailing slash. On macOS a
temp folder is keyed `/private/tmp/…`, never `/tmp/…`. Trust in the home
directory is session-only and never written.

**The rule** (the `claude --bg` trust gate, `$e` → `VI` → `_9n` in the bundle).
Take F = realpath(folder). F is TRUSTED when either holds:

1. **The exact key.** `projects[K].hasTrustDialogAccepted === true`, where K is
   the CANONICAL root of F's git repository. For a linked worktree that is the
   MAIN checkout, resolved by reading files: the `.git` file's `gitdir:`, then
   that directory's `commondir`, with its `gitdir` back-pointer checked. Outside
   any repository, K is F itself.
2. **The walk.** Some directory from F upward carries a truthy flag. The walk
   stops AT F's git root, the nearest ancestor-or-self holding a `.git` directory
   or file. Outside any repository it stops at `/`.

**A dead branch: `rootOnly`.** claude's code holds a third path that these two
clauses leave out, because in 2.1.284 it never runs. `hS` first asks `y9n(F)`,
and `y9n` first asks a ROOT FINDER, `T8n(F, {uncached})`. When that finds a root
and a second check on it (`Fmt`) does not answer `true`, `y9n` returns
`{root, rootOnly: true}`, and `hS` then trusts F ONLY when
`projects[root].hasTrustDialogAccepted === true`: that root's exact key, with no
walk and no git-root bound. Otherwise `y9n` returns the git root with
`rootOnly: false`, which leads to the two clauses above. The `--bg` gate `_9n`
reads `rootOnly` too. It is dead because `T8n` loops over a per-directory helper,
`KD`, whose whole body is `return null` (its sibling `Lr` returns `false`), so the
finder always answers `null`. If a later claude makes that helper answer, claude
would trust a folder under such a root through the root's exact key alone, while
the mirror's walk (clause 2) could still trust it from a flagged ancestor: an
OVER-trust, the direction the mirror must never err in. The mirror then needs a
re-probe of this rule before anything else. The re-verify step at the end of this
section detects the branch going live.

Two further cases also count as trusted: `CLAUDE_CODE_SANDBOXED` set, and a
symlinked spelling of F whose own entry is flagged. In practice:

- A trusted folder outside any repository covers every subfolder, but not the
  inside of a nested repository.
- A trusted repository root covers its subfolders.
- A worktree inherits its main checkout's trust.
- A trusted folder ABOVE a repository does not reach into it.

The docs' "Project allow rules and workspace trust" says the same. The rule was
read from the bundle's `sF`, `hS`, `y9n`, `yS`, `VI`, `_9n`, `Mde`/`nk`,
`Oe`/`Qt`, `Zt`/`Te` and the `--bg` gate `$e` (its "bg: workspace trust check
threw"), and the worktree pointer checks from `Hi`.

**Not observed end to end.** No side-effect-free probe exposes the walk's
verdict. `permissions.allow`, `additionalDirectories` and an MCP `headersHelper`
are gated on the EXACT key only (the docs: "trusting a parent folder doesn't
count for these rules"), and a `claude --bg` would start a session. The mirror
rests on the bundle and the docs agreeing.

**Where snapback's mirror fails toward UNTRUSTED** (why that direction is
[PATTERNS.md §1](PATTERNS.md#1-fail-soft-over-external-input)'s). Every point
where `claude_trust` cannot match claude exactly answers untrusted:

- a missing, unreadable or malformed record, or one with a leading byte-order mark
  (not stripped);
- a folder that cannot be canonicalised, or that the record does not cover;
- any flag value but the JSON boolean `true` (claude's walk accepts any truthy
  value);
- a key compared as the raw UTF-8 of the canonical path, with no NFC
  normalisation, so a non-NFC or non-UTF-8 path matches nothing;
- the three cases that only ever ADD trust, not mirrored: `CLAUDE_CODE_SANDBOXED`,
  the home directory's session trust, and the symlinked-spelling fallback;
- any `.git` entry, of whatever kind, or one that cannot be stat'ed, bounds the
  walk, so it is never longer than claude's;
- the worktree → main checkout step accepts only regular, non-symlink pointer
  files within `GIT_POINTER_MAX_BYTES`, symlink-free targets, a matching
  back-pointer and a non-bare main repository, with the worktree, its git
  directory and the main repository all on one mount. It refuses every pointer
  claude's `Hi` refuses, and more: UNC, backslash and `??` spellings; the `/net`
  and `/Network` automounts, whatever the host (claude allows a `/net/<host>`
  pointer on the worktree's own host); macOS `/.vol`, `/.file`, `/.nofollow` and
  `/.resolve`; a crossing between `/home/<user>` automounts; and the zero-width and
  bidirectional format characters claude deletes before it compares a mount name.
  Any failure keys the folder on the worktree's own root, which claude's walk
  checks too;
- claude's internal `-local-oauth` and `-staging-oauth` record files are not
  mirrored, because public builds never select them.

**Re-verify the trust split and the git prefetch** after a `claude` update, in a
throwaway folder that claude has never trusted. Its `apiKeyHelper` is the folder's
own `touch`, and the first run lets it fire on purpose. The folder is also a git
repository whose own `.git/config` defines a clean filter that `touch`es a second
marker, and its one tracked file is rewritten at the same size with an older
mtime, so a `git status` must re-hash it through that filter. Only the counts
print, so the `account` key never reaches the terminal:

```sh
d="$(cd "$(mktemp -d)" && pwd -P)" && mkdir "$d/.claude" && cd "$d" \
  && printf '{"apiKeyHelper": "touch %s/ran; echo x"}\n' "$d" > .claude/settings.json \
  && git init -q && printf 'aaaa\n' > a.txt && git add a.txt \
  && git -c user.name=probe -c user.email=probe@example.invalid -c commit.gpgsign=false \
       commit -q --no-verify -m init \
  && git config filter.sbprobe.clean "touch $d/filtered; cat" \
  && printf '* filter=sbprobe\n' > .gitattributes \
  && printf 'bbbb\n' > a.txt && touch -t 202001010000 a.txt
probe() {
  rm -f ran filtered
  printf '%s\n' '{"type":"control_request","request_id":"probe","request":{"subtype":"initialize"}}' \
    | claude -p --input-format stream-json --output-format stream-json --verbose \
        --no-session-persistence --strict-mcp-config --settings '{"disableAllHooks":true}' "$@" \
    | grep '"control_response"' \
    | jq -c '.response | {subtype, commands: (.response.commands | length), agents: (.response.agents | length)}'
  sleep 1   # give a straggling git a moment before the markers are read
  if [ -e ran ]; then echo "helper RAN"; else echo "no helper"; fi
  if [ -e filtered ]; then echo "filter RAN"; else echo "no filter"; fi
}
probe                          # the trusted form, in a folder claude never trusted
probe --setting-sources user   # the untrusted form's argv, without its environment
( export CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS=1; probe --setting-sources user )   # the untrusted form
cd - >/dev/null && rm -rf "$d"
```

Expect `helper RAN` and `filter RAN`, then `no helper` and `filter RAN`, then
`no helper` and `no filter`, with the last two runs' counts equal to a plain
folder's. On 2026-10-01 (2.1.284) it printed 71/15, then 74/15 twice: the
trusted form lists 3 fewer commands once the helper is claude's auth source, as
observed above. Any result changing is a reason to re-probe this section, not to
drop a form or the switch. In particular, `filter RAN` on the third run means the
switch no longer stops claude's prefetch, so the untrusted form no longer keeps
the repository's git configuration from running.

**Re-verify that `rootOnly` is still dead.** This reads the installed binary and
runs nothing. It anchors on property names and code shapes that survive
minification, never on the minified names, which change every build: the one
function that returns `{root:r.root,rootOnly:!0}` names the ROOT FINDER, the
finder's loop names its PER-DIRECTORY helper, and the branch stays dead while
that helper's whole body is `return null`. A minified name may hold a `$`, which
the second and third steps escape:

```sh
BIN="$(readlink -f "$(command -v claude)")"
finder="$(grep -aoE 'r=[A-Za-z0-9_$]+\(n,\{uncached:!0\}\);if\(r!==null&&[A-Za-z0-9_$]+\(r\.root,\{uncached:!0\}\)!==!0\)return\{root:r\.root,rootOnly:!0\}' "$BIN" \
  | sed -E 's/^r=([^(]*)\(.*/\1/' | sort -u)"
echo "root finder: [$finder]"
f="$(printf '%s' "$finder" | sed 's/[$]/\\$/g')"
helper="$(grep -aoE 'function '"$f"'\(e,\{uncached:n=!1\}=\{\}\)\{let r=e;for\(;;\)\{let s=[A-Za-z0-9_$]+\(r,' "$BIN" \
  | sed -E 's/.*let s=([^(]*)\(r,$/\1/' | sort -u)"
echo "per-directory helper: [$helper]"
h="$(printf '%s' "$helper" | sed 's/[$]/\\$/g')"
grep -aoE 'function '"$h"'\(e,\{uncached:n=!1\}=\{\}\)\{[^}]*\}' "$BIN" | sort -u
```

At 2.1.284 it printed `root finder: [T8n]`, `per-directory helper: [KD]` and
`function KD(e,{uncached:n=!1}={}){return null}`. Expect exactly one finder, one
helper and that `return null` body. Anything else (no match, several names, or a
body that does work) is a reason to re-probe [the rule](#workspace-trust-what-an-untrusted-folder-can-run-and-the-two-argv-forms)
before trusting the mirror, and possibly to make `claude_trust` fail toward
untrusted wherever such a root exists.

## Top-level options

Grouped for scanning; the CLI lists them alphabetically. `[P]` = only meaningful
with `-p/--print` (SDK/non-interactive mode).

### Session, model, effort

| Flag | Effect |
| --- | --- |
| `-c, --continue` | Continue the most recent conversation in this directory. |
| `-r, --resume [value]` | Resume by session ID, or open the picker (optional search term). |
| `--fork-session` | On resume/continue, mint a NEW session id instead of reusing the original. |
| `--from-pr [value]` | Resume a session linked to a PR (number/URL), or open the picker. |
| `--session-id <uuid>` | Use a specific (valid UUID) session id. |
| `-n, --name <name>` | Display name (prompt box, `/resume` picker, terminal title). |
| `--model <model>` | Model for the session — alias or full id (`claude-fable-5`). **`--help` lists only `fable`/`opus`/`sonnet`; that list is INCOMPLETE — see [Model aliases](#model-aliases---model).** |
| `--fallback-model <model>` | `[P]` Fallback model(s), comma-separated, tried in order when the primary is overloaded; the primary is re-tried at the start of each user turn. |
| `--agent <agent>` | Agent for the session; overrides the `agent` setting. |
| `--agents <json-or-file>` | JSON object defining custom agents inline, or with `--print` the path to a file that holds one. |
| `--effort <level>` | `low` \| `medium` \| `high` \| `xhigh` \| `max`. Never fails a launch — see [Effort levels](#effort-levels---effort). |
| `--autocompact <auto\|tokens>` | Auto-compact window size (`auto`, or 100k–1M tokens). |
| `--teleport [session]` | Resume a teleport session, optionally by session id. |
| `--cloud [description\|session_id\|url]` | Create a cloud session with a description, or attach to an existing one by session id or claude.ai/code URL. |
| `--environment <environment_id>` | Create a new cloud session on a given self-hosted environment (`ccpool_...`). |

#### Model aliases (`--model`)

**`claude --help` is WRONG here, and it is the one place in this doc where the
help text cannot be the source.** Its `--model` blurb names `fable`, `opus` and
`sonnet` only — three of the nine the binary accepts — so a help-derived list is
missing six, including the alias with the most leverage. The set below was read
out of the shipped binary instead (capture command in
[Refreshing this doc](#refreshing-this-doc)), in the binary's own array order:

| Alias | Notes |
| --- | --- |
| `sonnet` | Listed by `--help`. |
| `opus` | Listed by `--help`. |
| `haiku` | **Absent from `--help`.** |
| `fable` | Listed by `--help`. |
| `best` | **Absent from `--help`.** Accepted by the array; the bundle carries no picker label or description for it. |
| `sonnet[1m]` | **Absent from `--help`.** The 1M-context variant; the bundle builds its picker label from the current `sonnet` model's display name (`` `${displayName} (1M context)` ``) rather than carrying a literal. Its embedded `]` is why a `[^]]`-style capture regex truncates the array right here — see [Refreshing this doc](#refreshing-this-doc). |
| `opus[1m]` | **Absent from `--help`.** The 1M-context variant (`label:"Opus (1M context)"`). |
| `fable[1m]` | **Absent from `--help`.** Same `[1m]` naming; the bundle carries no label for this one. |
| `opusplan` | **Absent from `--help`.** Runs Opus for **plan mode** and the resting model otherwise — "plan with Opus, implement with Sonnet" as one alias. Confirmed from binary strings, including an `opusplan-mode-reminder`. It is also the CONTENT ANCHOR the capture command selects on: no other array in the bundle carries it. |

**This table is a POINT-IN-TIME RECORD FOR HUMANS, not a list `snapback` reads.**
`snapback` reads the same array out of the installed binary itself, at runtime
(`src/model_aliases.rs`), so a newly shipped or withdrawn alias reaches the
compose model picker (`Ctrl-L`) with no snapback release and **no edit here**. What
`tui::app::MODEL_ALIASES` holds is a five-entry COLD-START SEED — what the picker
draws in the frames before the probe answers, and what it keeps if the probe finds
nothing — and it is deliberately NOT hand-refreshed: a stale seed is cosmetic and
self-corrects. Refresh this table when you want the doc to describe the version in
the [pin](#version-pin-self-healing) above, never because a picker depends on it.

Neither the table nor the seed is a validation whitelist. `--model` also accepts a
**full model id** (`claude-sonnet-5`), so this is an alias set, not the accepted
domain: nothing in `snapback` validates the value it sends (the picker offers the
probed aliases verbatim), and an invalid one is claude's to refuse (a hard,
non-zero failure — see below).

**An invalid `--model` is a HARD failure, not the `--agent` silent downgrade.**
It exits **1** with an **empty stderr** and prints `is_error:true` on stdout with
`result:"There's an issue with the selected model (…)"`, `modelUsage:{}` and cost
0. Contrast the `--bg` agent case below, which exits **0** and starts the session
without the agent. Because the failure is loud, `send::status_for_failed_send`
already renders it correctly and no warned-outcome seam exists for it.

The `-p --output-format json` payload's **`modelUsage`** map is keyed by the model
that actually ANSWERED, with per-model `costUSD` — the only synchronous way to see
that a `--fallback-model` substituted something else for what `--model` asked for.
`send::status_for_send` reads it.

#### Effort levels (`--effort`)

**Read out of `claude 2.1.282`, and not tested live.** `claude --help` lists the
accepted levels (`--effort <level>  Effort level for the current session (low,
medium, high, xhigh, max)`); the behaviour below is from the bundle's strings and
code. Unlike `--model`, the flag can never make a hand-off FAIL:

- **The levels** are `low`, `medium`, `high`, `xhigh`, `max`, lowest to highest.
  snapback keeps them in ONE const, `resume::EFFORT_LEVELS`, in that order —
  the order the picker's `←`/`→` cycle walks (after an unset stop, wrapping). It
  is a compile-time list rather than a runtime read like the aliases, and that is
  safe precisely because of the next two points.
- **An unknown value does not fail the launch.** claude prints a warning
  (`Unknown --effort value '…'`) and runs at the default instead.
- **A level the model cannot use is quietly lowered**: `max` or `xhigh` become
  `high` ("after any silent downgrade for the selected model"), and a model with
  no effort support runs with no effort at all. snapback does not model any of
  this per model — it offers every level on every row, and claude resolves it.
- **`CLAUDE_CODE_EFFORT_LEVEL` beats `--effort`** (`CLAUDE_CODE_EFFORT_LEVEL
  overrides effort for this session`). A maximum effort level, when the settings
  configure one, also clamps whatever is asked for (`--effort` included).
- **Without `--effort`** the user's settings keep a default level per model
  (`modelSettings`), so a model picked with no effort runs at that model's
  settings level, or claude's built-in default. That is what the picker's unset
  stop (`default effort`) means; the board does not read which level it is.

snapback's effort pick lives inside its model pick, so the A MODEL IS PICKED PER
COMPOSE, NEVER PER BOARD rule in [AGENTS.md](../../AGENTS.md#critical-rules)
governs it as well. Unlike the model, **claude does not restore an effort** on a
later `-r` (`restoreModelFromSession`, below, restores the model alone), so a
picked effort applies to that one reply or launch; a later resume runs at the
settings level for whatever model it restores. Each preview turn marker already
shows the effort that turn actually ran at (`record_effort`,
[DOMAIN.md](DOMAIN.md#effort-level-record_effort)), so a lowered or overridden
level is visible after the fact. Because an effort can never cause a non-zero exit,
`resume::MODEL_NONZERO_HINT` is chosen on whether `--model` was emitted, never on
the effort, and never names it.

To re-verify after a `claude` update: `claude --help | grep -A1 -- --effort` for
the level list, and search the bundle (`strings -a`) for `Unknown --effort value`,
`CLAUDE_CODE_EFFORT_LEVEL overrides effort` and `silent downgrade`.

#### Which model a launch runs on without `--model`

**Read out of the `claude 2.1.282` bundle** — `--help` says nothing about any of
this. Only what the bundle's code showed is written here; the minified names it
was found under change every build, so re-verify against the stable strings named
at the end rather than against identifiers.

A launch picks its model in this order: `--model` (`--model default` means the
built-in default); else a bound agent's `model:` frontmatter (unless `inherit`);
else `ANTHROPIC_MODEL`; else the merged settings `model`; else the built-in
default. A settings or `ANTHROPIC_MODEL` value of `default` (trimmed, any case)
resolves to the built-in default too. Two JavaScript details matter: the check is
`ANTHROPIC_MODEL || model`, so an EMPTY `ANTHROPIC_MODEL` falls through to the
settings model; and the settings merge ASSIGNS a higher source's `model` even when
it is blank or `default`, so such a value masks the sources below it.

**Settings merge order**, lowest to highest (a later source wins): user
(`$CLAUDE_CONFIG_DIR/settings.json`, else `~/.claude/settings.json`) → project
(`<cwd>/.claude/settings.json`) → local (`<cwd>/.claude/settings.local.json`) →
flag (`--settings`) → managed. Managed settings are `managed-settings.json` in
`/Library/Application Support/ClaudeCode` (macOS), `C:\Program Files\ClaudeCode`
(Windows) or `/etc/claude-code` (everything else), overridden by every
`managed-settings.d/*.json` drop-in (dotfiles skipped) in file-name order; MDM
(plist/registry) and server-delivered tiers compose into the same managed layer.
One local-file caveat: when `<cwd>` is not its repository's canonical root,
`claude` reads that root's `.claude/settings.local.json` (after an ownership check)
and layers `<cwd>`'s own file beneath it.

**`env` blocks beat the shell.** At startup `claude` copies the global config's
(`~/.claude.json`) `env`, then each settings source's `env` in the merge order
above, ONTO its own process environment. So a settings file's
`env.ANTHROPIC_MODEL` overrides one inherited from the shell, the highest source
that sets it wins, and — because `ANTHROPIC_MODEL` precedes the settings `model` —
it beats every file's `model`.
Project and local files may set it; it is not on their blocked-key list.

`claude_settings::model_defaults` mirrors the settings layers, the drop-ins and
both `ANTHROPIC_MODEL` sources for a NEW session in the launch dir — the value a
`Ctrl-N` draft's `model:` label names. It does not mirror the MDM and server tiers,
the global config's `env`, or the local-file git-root relocation, and snapback
reads no agent's `model:` frontmatter, which outranks all of them for an agent
draft; a miss there mislabels the draft and never changes what `claude` runs,
since with no pick snapback sends no `--model`.

**A `-r` launch normally restores the session's own model**
(`restoreModelFromSession`). Every startup resume path calls it — interactive
`-r`, `-r --fork-session`, and `-p -r` — so Resume, Fork and the quick reply all
keep the model the session last ran on, NOT the settings default, unless it is
skipped or declined as below. (The in-session `/resume` picker skips it for a
fork; snapback never drives that path.) It:

- walks the transcript BACKWARDS and takes the last `assistant` record that is not
  `isMeta` and whose `message.model` is a string other than `<synthetic>`;
- is SKIPPED when a main-loop model override is set (`--model`, or a restored
  agent's frontmatter model), when `ANTHROPIC_MODEL` or any
  `ANTHROPIC_DEFAULT_{FABLE,OPUS,SONNET,HAIKU}_MODEL` is set, when the provider
  does not use first-party model ids (only first-party, Anthropic-on-AWS,
  Anthropic-on-Google-Cloud and the gateway do — Bedrock, Vertex and Foundry do
  not), and in one further env-attribution case the bundle does not make legible;
- is SKIPPED when the resolved model setting is the mode-dependent `opusplan` or
  `haiku` alias and the transcript model is compatible with it (`opusplan` with an
  opus or sonnet model, `haiku` with a haiku or sonnet model) and not an `-eap`
  model;
- is DECLINED — warning `Session model <m> could not be restored (<reason>) —
  using <model> instead.` and falling back to the model the launch would otherwise
  use — when the model's family is unknown to this `claude`, it is not allowed by
  the account's model settings (after an entitlement re-probe), or it is retired;
- may append `[1m]` to the restored id when the startup model or the transcript's
  own context calls for the long-context variant (the exact condition was not
  traced).

The consequences snapback is built on. **A launch without `--model` leaves the
model to `claude`, which keeps the session's own on every `-r` path unless a skip
or decline above applies** — an environment override, a non-first-party provider
or a model it declines at resume time among them — and the A MODEL IS PICKED PER
COMPOSE, NEVER PER BOARD rule in [AGENTS.md](../../AGENTS.md#critical-rules) rests
on that. **`--model` always wins over the restore**, so a compose
pick is honored as asked — and, because that model then answers last, the NEXT
`-r` normally restores it too. **No pick means two different things**: the
session's last model
for a quick reply, the settings default for a new session — so the two compose
boxes label their defaults differently. A reply's `model: session (<label>)` names
the same record the restore takes (`store::preview::restorable_model`: the last
non-`isMeta` `assistant` record whose `message.model` is a real model), and
`claude_settings::resolve_restore_overridden` mirrors the env-var skip for all five
names above — `ANTHROPIC_MODEL` or any `ANTHROPIC_DEFAULT_*_MODEL`, non-empty in the
highest settings layer's `env` block, else in the process environment — to make
that label say `default` instead. A draft's settings value carries
`(new sessions only)`. What snapback does NOT mirror includes the restored agent's
frontmatter-model skip (snapback reads no frontmatter `model:`), the provider skip
(this doc records no variables that select a provider), the `opusplan` / `haiku`
compatibility skip, the env-attribution skip the bundle does not make legible, and
the decline cases (only `claude` knows, at resume time, that a model is retired or
not allowed); in each of those the reply still reads `session (<label>)`, and none
of them changes what runs. The label also omits the `[1m]` suffix `claude` may
append to a restored id; that is the same model's long-context variant, not a
different model.

To re-verify after a `claude` update, search the bundle (`strings -a` on the
canonicalized binary) for these stable strings rather than for minified names:
`"userSettings","projectSettings","localSettings","flagSettings","policySettings"`
(the merge order), `managed-settings.d` and `/etc/claude-code` (the managed paths),
`applyConfigEnvironmentVariables` (the `env` copy), `startupModelWinsOverSessionRestore`
and `tengu_resume_model_restore` (the restore and its skip conditions).

### Print / SDK mode

| Flag | Effect |
| --- | --- |
| `-p, --print` | Print the response and exit (pipes). Skips the trust dialog (as does any non-TTY stdout); only use in trusted dirs. Settings files that fail validation are silently ignored in this mode. |
| `--output-format <fmt>` | `[P]` `text` (default) \| `json` \| `stream-json`. |
| `--input-format <fmt>` | `[P]` `text` (default) \| `stream-json`. |
| `--include-partial-messages` | `[P]` Emit partial chunks as they arrive (stream-json only). |
| `--include-hook-events` | Include hook lifecycle events (stream-json only). |
| `--replay-user-messages` | Re-emit stdin user messages on stdout (stream-json in+out). |
| `--forward-subagent-text` | `[P]` Forward subagent text/thinking as messages (stream-json). |
| `--json-schema <schema>` | JSON Schema for structured-output validation. |
| `--max-budget-usd <amount>` | `[P]` Hard cap on API spend. |
| `--no-session-persistence` | `[P]` Do not save the session to disk (not resumable). |
| `--prompt-suggestions [value]` | Emit a predicted next-prompt message each turn. |
| `--permission-prompts <target>` | `[P]` Who answers permission prompts: `host` (default — the SDK host or `--permission-prompt-tool`) or `none` (anything that would prompt is denied). |

### Permissions & tools

| Flag | Effect |
| --- | --- |
| `--permission-mode <mode>` | `acceptEdits` \| `auto` \| `bypassPermissions` \| `manual` \| `dontAsk` \| `plan`. |
| `--dangerously-skip-permissions` | Bypass ALL permission checks (sandboxes only). |
| `--allow-dangerously-skip-permissions` | Make bypass available as an option without defaulting to it. |
| `--allowedTools, --allowed-tools <tools...>` | Allowlist, e.g. `"Bash(git *)" Edit`. |
| `--disallowedTools, --disallowed-tools <tools...>` | Denylist. |
| `--tools <tools...>` | Restrict the built-in tool set (`""` = none, `default` = all, or names). |
| `--add-dir <dirs...>` | Extra directories tools may access. |
| `--restricted` | Drop the command/code-running tools and WebFetch unless `--tools` names them, ignore user/project/local settings files, confine file tools to the working dirs, and refuse `bypassPermissions`. |

### Config, MCP, plugins

| Flag | Effect |
| --- | --- |
| `--settings <file-or-json>` | Load extra settings from a file path or JSON string. |
| `--setting-sources <sources>` | Comma-separated: `user`, `project`, `local`. |
| `--mcp-config <configs...>` | Load MCP servers from JSON files/strings. |
| `--strict-mcp-config` | Only use MCP servers from `--mcp-config`. |
| `--plugin-dir <path>` | Load a plugin dir/`.zip` for this session (repeatable). |
| `--plugin-url <url>` | Fetch a plugin `.zip` from a URL for this session (repeatable). |
| `--system-prompt <prompt>` | Replace the default system prompt. |
| `--append-system-prompt <prompt>` | Append to the default system prompt. |
| `--exclude-dynamic-system-prompt-sections` | Move per-machine sections to the first user message (better cache reuse). |
| `--system-prompt-snapshot <on\|off>` | `on` (default): record the system prompt once per conversation and reuse it verbatim on every request and resume until compaction. `off`: render it fresh every request. |
| `--betas <betas...>` | Beta headers (API-key users only). |

### Session lifecycle & environment

| Flag | Effect |
| --- | --- |
| `--bg, --background` | Start in the background and return immediately, printing the id that `claude attach`, `logs`, `stop` and `rm` take (`claude agents` lists them). With `--resume <session-id>` it continues that session in the background under the same id, or starts a copy and says so when the session is already running. |
| `-w, --worktree [name]` | Create a git worktree for this session. |
| `--tmux` | Create a tmux session for the worktree (requires `--worktree`; `--tmux=classic` for plain tmux). |
| `--remote-control [name]` | Interactive session with Remote Control enabled. |
| `--remote-control-session-name-prefix <prefix>` | Prefix for auto-named Remote Control sessions. |
| `--ide` | Auto-connect to an IDE on startup if exactly one is available. |
| `--chrome` / `--no-chrome` | Enable / disable the Claude-in-Chrome integration. |
| `--brief` | Enable the `SendUserMessage` agent-to-user tool. |
| `--file <specs...>` | Download file resources at startup (`file_id:relative_path`). |

### Startup mode & diagnostics

| Flag | Effect |
| --- | --- |
| `--bare` | Minimal mode: skip settings/plugin hooks, LSP, plugin sync, attribution, auto-memory, background prefetches, keychain reads and CLAUDE.md discovery. Sets `CLAUDE_CODE_SIMPLE=1`; Anthropic auth is strictly `ANTHROPIC_API_KEY`/apiKeyHelper (OAuth and keychain are never read). |
| `--safe-mode` | Disable all customizations for troubleshooting. Policy settings still apply; sets `CLAUDE_CODE_SAFE_MODE=1`. |
| `-d, --debug [filter]` | Debug mode with optional category filter (`"api,hooks"` or `"!1p,!file"`). |
| `--debug-file <path>` | Write debug logs to a path (implies debug). |
| `--verbose` | Override the verbose setting. |
| `--ax-screen-reader` | Screen-reader-friendly flat output. |
| `--disable-slash-commands` | Disable all skills. |
| `-v, --version` | Print the version. |
| `-h, --help` | Help. |

## Commands

Listed by `claude --help`. Run `claude <command> --help` for a command's own
flags (a few are expanded below).

| Command | Purpose |
| --- | --- |
| `agents` | Manage background agents. Also `--json[ --all]` for scripting. |
| `attach <id>` | Open a background session in this terminal (see [below](#background-session-commands)). |
| `auth` | Manage authentication (`login`/`logout`/`status`). |
| `auto-mode` | Inspect or reset the auto-mode classifier config. |
| `doctor` | Health-check the installation (read-only; no trust prompt). |
| `gateway` | Run the enterprise auth/telemetry gateway (`--config <path>`). |
| `import [source]` | Import config from another AI coding agent (`codex` / `gemini` / `cursor`; `--dry-run`, `--yes`). |
| `install [target]` | Install a native build (`stable`/`latest`/version; `--force`). |
| `logs <id>` | Print a background session's recent terminal output. |
| `mcp` | Configure and manage MCP servers. |
| `plugin` \| `plugins` | Manage plugins and marketplaces. |
| `project` | Manage project state (`purge` deletes all Claude state for a project). |
| `respawn [id]` | Restart a background session, or all of them with `--all`, on the current Claude Code version. |
| `rm <id>` | Delete a background session, and its worktree when that is safe; works on sessions that already exited. |
| `setup-token` | Set up a long-lived auth token (requires a subscription). |
| `stop` \| `kill <id>` | Stop a background session; its conversation is kept. |
| `ultrareview [target]` | Cloud multi-agent review of the branch / a PR number / base branch. |
| `update` \| `upgrade` | Check for updates and install if available. |

## Background-session commands

The commands that act on a background session by its **short job id** — the `id`
that `claude --bg` prints and `claude agents --json` reports on background records.
In the 2.1.220 capture `attach` and `stop` were hidden from `claude --help`. At
2.1.282, as at 2.1.280, `attach`, `stop`, `logs`, `respawn` and `rm` are all
listed there, and `daemon` is the one hidden command in this set (see
[Hidden commands](#hidden-commands)). Each usage below is the command's own
`--help` text at 2.1.282, identical at 2.1.280, except `daemon stop`'s, which
was read from the binary (see [Refreshing this doc](#refreshing-this-doc) for why
it is never run).

**None of them can end a record that has no job id**, and every
`kind: "interactive"` record measured so far has none (0/3 at 2.1.278, 0/2 at
2.1.280, see [DOMAIN.md](DOMAIN.md#what-kind-interactive-denotes); 0/3 in the
2.1.282 [spot-check](DOMAIN.md#observed-value-distribution)). Where their
processes were inspected (2.1.278 and 2.1.280), those records were a `claude -p`
print-mode child or an interactive TUI. The last column below records this per
command, and it is why
`Ctrl-K` has a SIGTERM route at all (see
[How snapback drives `claude`](#how-snapback-drives-claude)).

| Command | Usage (own help text) | Notes | Can it end a record with no job id? |
| --- | --- | --- | --- |
| `claude attach <id>` | Open the background session in this terminal. `←` returns to agent view, `Ctrl+Z` drops back to your shell. The session keeps running either way. | **`snapback` depends on it** (Attach). Takes the SHORT job id. | No: it needs a job id, and it attaches rather than ends. |
| `claude stop <id>` (alias `kill`) | Stop a background session. Its conversation is kept; resume it later with `claude attach <id>`. | **`snapback` depends on it** (the reply unlock and `Ctrl-K`'s job-id route). Only the live job registration drops, which is what lets `claude -p -r` reclaim the session. Takes the SHORT job id. The `kill` alias takes a job id too; it is not a way to signal a pid. | No: it takes a job id. |
| `claude rm <id> [--discard-unpushed <commit>@<worktree-id>] [--force-remove-worktree <worktree-id>]` | Delete a background session and its worktree. Unlike `stop`, works on already-exited sessions. | Not used by `snapback`. The two flags pass back a value a previous `claude rm <id>` reported, to discard unpushed work or force-remove a worktree git could not. | No: it takes a job id and deletes a background session. |
| `claude respawn <id>\|--all` | Restart a background session (or all of them) so it picks up the current Claude binary. | Not used by `snapback`. | No: background sessions only, and it restarts rather than ends. |
| `claude logs <id>` | Print the background session's recent terminal output. | Not used by `snapback`. Read-only. | No: it reads, it does not end anything. |
| `claude daemon stop [--any] [--keep-workers]` | Shut down the supervisor and terminate background sessions (`--any` also stops a transient, non-service daemon; `--keep-workers` leaves detached sessions running). | **Do NOT build on it.** It is GLOBAL, with no per-session meaning: one call terminates background sessions across every project. | No: per its usage text it terminates BACKGROUND sessions (never exercised here). |

**`claude rm`, noted for its own sake (future consideration only; NOT wired).**
Background job registrations accumulate. In the 2.1.278 capture `--all` listed
159 background records against 83 in the bare list (162 against 86 records in
all), and the 2.1.280 spot-check read 164 against 82. An earlier 2.1.278 sample,
on 2026-09-21, read 150 against 83. `Ctrl-X d` unlinks a session's transcript but
leaves its job registration behind, so `claude agents --json --all` goes on listing
a job whose transcript is gone. `claude rm <id>` is the verb that would drop it, and it
works on sessions that already exited. Wiring it into `Ctrl-X d` is explicitly out
of scope here. It would need its own design: it deletes a WORKTREE too, and its
unpushed-work handling (`--discard-unpushed`) is not something a board keypress
should decide.

### Hidden commands

Subcommands that are **absent from `claude --help`** but whose usage text ships
in the binary. At 2.1.282 those are `daemon` and `self-hosted-runner`. Both were
already in the 2.1.280 binary, but that capture recorded only `daemon`.

| Command | Usage | Notes |
| --- | --- | --- |
| `claude daemon [subcommand] [options]` | Service lifecycle for the background-session supervisor: `run [json-path]` (**the default when piped**), `status`, `logs`, `stop`, `install`, `start`, `restart`, `uninstall`; options `--json-path <p>` (default `~/.claude/daemon.json`), `--log-file <p>` (default `~/.claude/daemon.log`), `--help`/`-h`. | Not used by `snapback`. Read from the binary's embedded usage text, never by running it: a bare `claude daemon` with piped stdout RUNS the supervisor. `stop` is in the table above. |
| `claude self-hosted-runner [options]` | Runs a self-hosted runner that takes Claude Code CLOUD sessions on this machine. Its options cover the connection (API URL, environment secret, an egress proxy), capacity and checkout directory, lifecycle hooks, and per-session watchdogs. Subcommands: `orchestrator` (polls a spawn-hint queue and runs a `spawn-runner` hook per hint), `setup` and `doctor` (interactive wizards that start a Claude Code session), `decode-token [token]`. | Not used by `snapback`, and it takes no background job id. Read from the binary's embedded usage text, never by running it: without `--help` it starts a long-running runner, and `setup`/`doctor` start a session. Whether its `--help` returns before anything starts was not tested by running it. |

Because a hidden command is undocumented in `--help`, a version bump can add,
change or remove one without a visible help diff. The embedded-usage listing in
[Refreshing this doc](#refreshing-this-doc) is how a new one shows up. The same
caution applies to the two `snapback` depends on: if its attach/send/stop paths
regress after a `claude` update, re-verify `claude stop --help` /
`claude attach --help` first.

## Returning to snapback from inside a session

`snapback` regains control only when the spawned `claude` child hands the terminal
back (the dashboard loop in [ARCHITECTURE.md](ARCHITECTURE.md#the-persistent-dashboard-loop-librun)
blocks in `resume::launch` until then). From inside a resumed session there are
three ways to trigger that, and they are NOT equivalent — the user guide
([GUIDE.md](../GUIDE.md)) steers users to the first two:

| Action | What it does | Hand-back |
| --- | --- | --- |
| `/bg` (alias `/background`) | Slash command: detaches the session to keep running as a background agent and frees the terminal. Control returns to the board, and the session reappears on the list with a live `bg` badge (Attach / fork / `Ctrl-K` stop; a quick reply is refused while it is genuinely live). Works the same in a plain-resumed and an attached session. | Clean. |
| `/exit` | Slash command: ends the session and returns to the board. | Clean. |
| `Ctrl+Z` | Only a clean detach when ATTACHED to a background agent (Claude Code intercepts it — see the `attach` row above). In a REGULAR interactive session it is an OS `SIGTSTP` suspend and can hand the terminal back dirty. | Dirty in the regular case — `hard_reset` repaints from a known-good state on return (see the terminal-safety seams). |

`Ctrl+Z` is the path the return-leg terminal recovery exists to survive, not the
recommended way out. There is currently no Claude Code keybinding action that
backgrounds a session (`~/.claude/keybindings.json` exposes no `/bg` equivalent
and does not bind slash commands), so `/bg` must be typed.

## Selected subcommand flags

Only the commands `snapback` touches or that are useful for quick review. For the
rest, `claude <command> --help` is authoritative.

### `claude agents` (snapback's live-agent source)

| Flag | Effect |
| --- | --- |
| `--json` | Print active sessions (interactive + background) as a JSON array and exit — no TTY needed. This is the shape `snapback` parses fail-soft; its fields, and which records carry `id` / `pid`, are measured in [DOMAIN.md](DOMAIN.md#reported-agents-srcagentsrs). A run that cannot answer (no child, a non-zero exit, no readable array) counts as "not live" for every gate but the `Ctrl-X w` move, which refuses instead ([DOMAIN.md](DOMAIN.md#why-the-gate-does-not-read-the---all-map)). |
| `--all` | With `--json`, also include completed background sessions. |
| `--cwd <path>` | Only background sessions started under `<path>`. |
| `--agent` / `--model` / `--effort` / `--permission-mode` | Defaults for sessions dispatched from agent view. |
| `--restricted` | Start dispatched sessions in restricted mode. |
| `--add-dir` / `--mcp-config` / `--plugin-dir` / `--settings` / `--setting-sources` / `--strict-mcp-config` | Config applied to dispatched sessions (repeatable where noted). |
| `--dangerously-skip-permissions` | Alias for `--permission-mode bypassPermissions`. |
| `--allow-dangerously-skip-permissions` | Make bypass available to dispatched sessions without defaulting to it. |

### `claude auth`

| Subcommand | Flags / notes |
| --- | --- |
| `login` | `--claudeai` (default) \| `--console` (API billing) \| `--sso` \| `--email <email>`. |
| `logout` | — |
| `status` | `--json` (default) \| `--text`. |

### `claude mcp`

`add`, `add-json <name> <json>`, `add-from-claude-desktop`, `get <name>`,
`list`, `login <name>`, `logout <name>`, `remove <name>`,
`reset-project-choices`, `serve`. `add` takes `--transport http|sse|stdio`,
`--header`, `-e KEY=val`, and `-- <command> [args...]` for stdio servers.

### `claude plugin`

`details`, `disable`, `enable`, `eval [target]`, `init|new <name>`,
`install|i <plugin>`, `list`, `marketplace`, `prune|autoremove`, `tag`,
`uninstall|remove <plugin>`, `update <plugin>`, `validate <path>`.
`update`'s `--scope` defaults to auto-detect.
`marketplace` has `add <source>`, `list`, `remove|rm <name>`, `update [name]`.

### `claude project`

`purge [path]` — delete ALL Claude Code state for a project (transcripts, tasks,
file history, config entry). Destructive; relevant because it removes the JSONL
`snapback` reads. Flags: `--all` (every project; exclusive with `[path]`),
`--dry-run`, `-i`/`--interactive` (prompt per item), `-y`/`--yes` (skip the
confirmation).

### `claude auto-mode`

`config` (effective config as JSON), `defaults` (shipped default rules as JSON),
`critique` (AI feedback on custom rules), `reset` (remove the `autoMode` section
from user settings).

### `claude ultrareview`

`--json` (raw `bugs.json`) \| `--timeout <minutes>` (default 45) \| `--post`
(post the findings to the PR as you; PR targets only) \| `--no-post` (the
default). User-triggered and billed; a session cannot launch it for you.

## Refreshing this doc

`claude` is an external binary, so the repo's `project-agent-docs` self-healing
stage cannot regenerate these facts — they must be re-captured from the live CLI:

```sh
claude --version </dev/null
claude --help </dev/null
for c in agents auth mcp plugin project install update ultrareview \
         doctor setup-token gateway auto-mode import \
         attach stop rm respawn logs; do
  echo "== $c =="; claude "$c" --help </dev/null
done
# Second level. A subcommand a group lists that is missing here is NEW:
# check it is an ordinary subcommand before adding it and calling its --help.
for p in auth:login auth:logout auth:status \
         mcp:add mcp:add-from-claude-desktop mcp:add-json mcp:get mcp:list \
         mcp:login mcp:logout mcp:remove mcp:reset-project-choices mcp:serve \
         plugin:details plugin:disable plugin:enable plugin:eval plugin:init \
         plugin:install plugin:list plugin:marketplace plugin:prune plugin:tag \
         plugin:uninstall plugin:update plugin:validate project:purge \
         auto-mode:config auto-mode:critique auto-mode:defaults auto-mode:reset; do
  echo "== ${p%%:*} ${p#*:} =="; claude "${p%%:*}" "${p#*:}" --help </dev/null
done
for s in add list remove update; do
  echo "== plugin marketplace $s =="; claude plugin marketplace "$s" --help </dev/null
done
# Hidden commands: NEVER run, not even with --help (see below). List every usage
# line embedded in the binary (a name missing from `claude --help` is hidden),
# then read each hidden command's usage from the binary.
BIN="$(readlink -f "$(command -v claude)")"
strings "$BIN" | grep -oE 'Usage: claude [a-z][a-z-]*' | sort -u
strings "$BIN" | grep -A15 '^Usage: claude daemon'
strings "$BIN" | grep -A30 '^Usage: claude self-hosted-runner'

# --model aliases: NOT derivable from --help (it names 3 of the 9), so read the
# array out of the shipped binary. Extract every FLAT array of quoted strings,
# keep the ones carrying `opusplan`, print the longest. That is the same rule
# `src/model_aliases.rs` applies at runtime — content-anchor first, longest-wins
# only as a deterministic tie-break — and it exits 1 when it finds nothing, so a
# withdrawn anchor fails loudly instead of printing an empty line.
strings -a "$(command -v claude)" \
  | grep -oE '\["[^"]*"(,"[^"]*")*\]' \
  | grep -F '"opusplan"' \
  | awk '{ if (length > n) { n = length; a = $0 } }
         END { if (n) print a
               else { print "no --model alias array found" > "/dev/stderr"; exit 1 } }'
```

**Run nothing but `--version` and `--help` here.** Several commands in this list
start, stop, delete, restart or attach to something when run without `--help`
(`stop`, `rm`, `respawn`, `attach`, `install`, `update`), and so do `-p` and `-r`.
Stdin comes from `/dev/null` so nothing can wait on a prompt. The two hidden
commands are the exception to even `--help`. With no subcommand, `daemon` RUNS the
supervisor when its stdout is piped, and `daemon stop` terminates background
sessions across every project. `self-hosted-runner` starts a long-running runner,
and its `setup`/`doctor` start a Claude Code session. Their usage is therefore
read from the text embedded in the binary, never from running them, and a NEW
hidden command gets the same treatment. Pass multi-word subcommands as separate
words (`claude auth login --help`, not a single quoted `"auth login"`, which just
re-prints the top-level help).

**Diff against the previous build.** On a native install,
`readlink -f "$(command -v claude)"` resolves to a file named after its version,
and older builds may still sit beside it. When the previous one does, run the
block above against both and `diff` the output. That diff is the capture-time
check: it catches a changed default or wording that no table records. What it
finds is folded into the tables and the version pin, and the before/after is left
to the commit message, because git history is the
[refresh log](README.md#maintenance).

Three traps in the `--model` alias capture, each reproduced against the real
binaries. Do not "simplify" past any of them:

1. **Do not require `opusplan` to be LAST.** The earlier form of this command
   (`'\[("[^"]+",)+"opusplan"\]'`) could only match while `opusplan` was the final
   element. Fed an array with one alias appended after it, that form printed
   NOTHING and the pipeline still exited **0** — a drift check that cannot detect
   the drift it exists to catch, and that fails silently rather than loudly. The
   pattern above accepts a flat string array of any length and finds the anchor
   anywhere in it.
2. **Do not use a `[^]]*`-style terminator.** `sonnet[1m]` carries a `]` inside a
   quoted element, so a "run of non-`]`" stops dead there and yields
   `["sonnet","opus","haiku","fable","best","sonnet[1m]` — the array truncated,
   silently dropping the four aliases after it.
3. **Do not anchor on the identifier, and do not take the longest array.** The
   variable holding it is minified and regenerated every build — observed as
   `h9e` → `bze` → `SWe` → `qWe` → `UKe` across five consecutive releases up to
   2.1.235, and as `xU` in 2.1.282. And the array immediately BEFORE the alias
   array is a full-model-id list (`["claude-3-5-haiku",…,"claude-sonnet-5"]`) —
   17 elements in 2.1.235 and 20 in 2.1.282, against the alias array's nine — so
   longest-wins on its own returns the wrong one. The content anchor is what
   discriminates: in 2.1.282 exactly one array in the bundle carries `opusplan`.

That command is the refresh owner for
[Model aliases](#model-aliases---model) and for nothing else — update the table
and the [version pin](#version-pin-self-healing) in one pass. Read its output as
the ACCEPTED alias set (`--model` takes full model ids besides).

It is **not** how `tui::app::MODEL_ALIASES` is maintained, and that const's doc
comment no longer points here. It is a cold-start seed the picker outgrows within
the first frames of a board session, because `src/model_aliases.rs` applies the
selection rule above to the installed binary at runtime — so the picker never
waits on this pass, and hand-syncing the const would rebuild the very artifact
that module exists to delete.

Re-read each source in the [website gap table](#website-gap-claims) and Claude
Code's "What's new" entries since its "Verified at". If a gap has closed, update
or drop that website row in the same change and update the table. The
command-surface pin above is a separate refresh.

Update the tables **and** the [version pin](#version-pin-self-healing) together
when the surface changes. When a flag/command that `snapback` invokes changes,
also fix the matching argv builder and its inline test in `src/resume.rs`,
`src/send.rs`, `src/agents.rs` or `src/claude_move.rs` — the code and this doc are
the two halves of one contract. The `Ctrl-X w` move rides an UNDOCUMENTED request
no `--help` shows, so a refresh to a `claude` past 2.1.291 also runs the
[`set_cwd` re-verify](#set_cwd-moving-a-session-without-leaving-the-board) and
re-pins that section: nothing else would catch its drift. The `--effort` level list is the one hand-kept list in that
contract: when `claude --help` changes it, update `resume::EFFORT_LEVELS` and its
pinning test (`the_effort_levels_are_claudes_in_ascending_order`) with it.
