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
| Start a new session on a drafted prompt | `claude [--agent <name>] <prompt>` | `resume::build_new_argv` |
| Start a BACKGROUND agent on a drafted prompt | `claude [--agent <name>] --bg <prompt>` | `send::build_bg_launch_argv` (`src/send.rs`) |
| Attach to a live background job | `claude attach <job-id>` | `resume::build_attach_argv` |
| Quick-send a reply (non-interactive) | `claude -p -r <session-id> --output-format json <message>` | `send::build_send_argv` (`src/send.rs`) |
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
| `--model <model>` | Model for the session — alias (`fable`/`opus`/`sonnet`) or full id (`claude-fable-5`). |
| `--fallback-model <model>` | `[P]` Fallback model(s), comma-separated, tried in order when the primary is overloaded; the primary is re-tried at the start of each user turn. |
| `--agent <agent>` | Agent for the session; overrides the `agent` setting. |
| `--agents <json-or-file>` | JSON object defining custom agents inline, or with `--print` the path to a file that holds one. |
| `--effort <level>` | `low` \| `medium` \| `high` \| `xhigh` \| `max`. |
| `--autocompact <auto\|tokens>` | Auto-compact window size (`auto`, or 100k–1M tokens). |
| `--teleport [session]` | Resume a teleport session, optionally by session id. |
| `--cloud [description\|session_id\|url]` | Create a cloud session with a description, or attach to an existing one by session id or claude.ai/code URL. |
| `--environment <environment_id>` | Create a new cloud session on a given self-hosted environment (`ccpool_...`). |

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

Update the tables **and** the [version pin](#version-pin-self-healing) together
when the surface changes. When a flag/command that `snapback` invokes changes,
also fix the matching argv builder and its inline test in `src/resume.rs`,
`src/send.rs`, or `src/agents.rs` — the code and this doc are the two halves of
one contract.
