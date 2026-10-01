# Claude CLI reference

Reference for the **external `claude` binary** that `snapback` shells out to.
Everything here is captured from the live CLI (`claude --help` and each
`claude <cmd> --help`), not from this repo — it is the "what does Claude Code
actually expose" quick-review sheet the rest of the docs assume.

`snapback` never links `claude`; it spawns it as a child (resume/fork/attach/send)
and reads `claude agents --json`. On ONE route it also sends a SIGTERM to a `pid`
that `claude agents --json` reported. That is not a `claude` invocation, and it is
listed [below](#how-snapback-drives-claude) so the boundary stays visible. The
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
| Detect live agents (gate probe) | `claude agents --json` | `agents::live_agents_argv` (`src/agents.rs`) |
| Detect live agents (incl. just-finished) | `claude agents --json --all` | `agents::agents_argv` |
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

**Every** invocation in the table above is spawned through
`claude_cmd::claude_command(argv, profile_override)` (`src/claude_cmd.rs`), the
ONE seam that turns a pure argv into an actual `Command`. It is handed the
override `config::claude_profile_override()` read, and stamps
`CLAUDE_CONFIG_DIR` onto the child via `Command::env` ONLY when the user set
one and it is absolute. With no override the child's environment is left
untouched, so `claude` resolves its own default `~/.claude`, which is also the
store's default. Spelling that default out would NOT be the same: an
explicitly set `CLAUDE_CONFIG_DIR` changes which login and which global config
file `claude` uses, even when it names `~/.claude`
([evidence](#an-explicitly-stamped-default-is-not-unset)). The one exception is
`resume::open_url`, which opens a link in the platform's default-app launcher
(`open`/`xdg-open`/`cmd /C start`) — that is not `claude`, so it builds its own
plain `Command` (`opener_command`) and never carries the profile. See
[`CLAUDE_CONFIG_DIR`](#claude_config_dir--the-profile-variable-claude_command-stamps)
below for the variable itself.

### `CLAUDE_CONFIG_DIR` — the profile variable `claude_command` stamps

`snapback` did not invent this name — the external `claude` binary already
honors it. Confirmed empirically against `claude 2.1.220`, **not from docs**:
the published settings reference at <https://code.claude.com/docs/en/settings>
(checked 2026-07-30) still describes `~/.claude` as fixed and does not list it.
The feature has outrun its docs, which is why this fact lives here rather than
being assumed permanent.

```sh
strings -a ~/.local/share/claude/versions/2.1.220 | grep -c CLAUDE_CONFIG_DIR
# → dozens of hits (measured 28 on 2026-07-30; the exact count drifts by
# release — only "non-zero" is load-bearing)
```

It relocates the transcript store to `$CLAUDE_CONFIG_DIR/projects`, with the
IDENTICAL `<encoded-cwd>/<id>.jsonl` layout, and the agent-job registry too —
an A/B/C probe against the real binary:

- **(A)** `claude -r <real-id> < /dev/null` under the normal config → the
  session was FOUND (it then errored later on an unrelated deferred-tool
  marker).
- **(B)** the SAME id with `CLAUDE_CONFIG_DIR=/tmp/probe` → `No conversation
  found with session ID: …`.
- **(C)** after copying that one `.jsonl` into
  `/tmp/probe/projects/<encoded-cwd>/` → FOUND again, failing with the
  identical error as (A) — proving the on-disk layout under a relocated
  profile is byte-for-byte what `store::discover` already walks.
- `CLAUDE_CONFIG_DIR=/tmp/probe claude agents --json` → `[]`, even when the
  un-overridden command returns a real, non-empty list — the agent-job
  registry is profile-scoped too, which is what makes `agents::live_agents`
  (the hard-delete WRITER guard's sole authority) profile-scoped.

Re-checked on 2026-09-30 against the installed `claude 2.1.284`
(`~/.local/share/claude/versions/2.1.284`). This is evidence for this variable
only; it does not move the command-surface
[version pin](#version-pin-self-healing):

- `strings -a <binary> | grep -c CLAUDE_CONFIG_DIR` → 49 hits;
  `strings -a <binary> | grep -c CLAUDE_PROJECTS_DIR` → 0.
- `CLAUDE_CONFIG_DIR=<fresh mktemp -d> claude agents --json </dev/null` →
  `[]`, exit 0. The job registry is still profile-scoped.
- `CLAUDE_CONFIG_DIR=<tmp> claude auth status --json` →
  `"configDirectory": "<tmp>"` and `"projectsDirectory": "<tmp>/projects"`. The
  binary itself reports the relocated profile and the `<dir>/projects` store
  under it, the shape `store::discover` derives.

#### An explicitly stamped default is not "unset"

Measured on 2026-09-30 against the installed `claude 2.1.284`, invoked by its
full path (`~/.local/share/claude/versions/2.1.284`), and re-run on 2026-10-01
with the same result. This is why `claude_cmd` stamps only an override the user
set and never the default spelled out. A stamped default once launched every
spawned child logged out.

- `env -u CLAUDE_CONFIG_DIR <binary> auth status --json </dev/null` →
  `"loggedIn": true`, `"authMethod": "claude.ai"`, `"configDirectory"` =
  `~/.claude` (printed as an absolute path).
- `CLAUDE_CONFIG_DIR="$HOME/.claude" <binary> auth status --json </dev/null` →
  `"loggedIn": false`, `"authMethod": "none"`, and the SAME
  `"configDirectory"`. Same directory, different login.
- The two read different global-config files. `~/.claude.json` (about 94 KB)
  is read when the variable is unset; `~/.claude/.claude.json` (772 bytes) is
  read when it is set. The sizes drift, and only the fact that these are two
  different files is load-bearing.
- The bundle shows why (minified names are per build). The keychain service
  name (`wN`) has no suffix only while `!process.env.CLAUDE_CONFIG_DIR`. Once
  the variable is set, to any value, it gains a `-<first 8 hex of
  sha256(dir)>` suffix. The global config path (`M7n`) is
  `$CLAUDE_CONFIG_DIR/.claude.json` when the variable is set, else
  `~/.claude.json`. Read from the bundle but NOT exercised: `nr()` returns
  false, skipping a background-daemon path, whenever the variable is set.
- The profile directory itself (`be`) is
  `process.env.CLAUDE_CONFIG_DIR ?? ~/.claude`. The `??` keeps an EMPTY value
  as `""`, so `claude` does not treat empty as unset, while `config` does.
  `claude_cmd` is handed no override for it and stamps nothing, so an empty
  value reaches the child as is, as it did before `snapback` honored the
  variable.

`auth status --json` also prints account identifiers. Record only the keys
above.

Because the feature has outrun its docs, `config::claude_config_dir()` always
falls back to `~/.claude` when the variable is unset or empty — if a future
`claude` release drops support for it, `snapback` degrades to today's behavior
rather than breaking. `CLAUDE_PROJECTS_DIR` (`snapback`'s own invention, zero
hits in the same `strings` scan) is a separate, fixtures/demo-only override of
the store view; see [OPERATIONS.md](OPERATIONS.md#environment) for how the two
differ.

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
three ways to trigger that, and they are NOT equivalent — the README steers users
to the first two:

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
| `--json` | Print active sessions (interactive + background) as a JSON array and exit — no TTY needed. This is the shape `snapback` parses fail-soft; its fields, and which records carry `id` / `pid`, are measured in [DOMAIN.md](DOMAIN.md#reported-agents-srcagentsrs). |
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

# CLAUDE_CONFIG_DIR is undocumented upstream, so THIS doc is its only record —
# re-run the strings count against the real installed binary ($BIN above, not
# a PATH shim), the A/B/C + `agents --json` probe and the unset-vs-stamped-
# default `auth status` pair above against the live binary rather than
# trusting a prior capture:
strings -a "$BIN" | grep -c CLAUDE_CONFIG_DIR
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

Update the tables **and** the [version pin](#version-pin-self-healing) together
when the surface changes. When a flag/command that `snapback` invokes changes,
also fix the matching argv builder and its inline test in `src/resume.rs`,
`src/send.rs`, or `src/agents.rs` — the code and this doc are the two halves of
one contract. The `--effort` level list is the one hand-kept list in that
contract: when `claude --help` changes it, update `resume::EFFORT_LEVELS` and its
pinning test (`the_effort_levels_are_claudes_in_ascending_order`) with it.
