# Claude CLI reference

Reference for the **external `claude` binary** that `snapback` shells out to.
Everything here is captured from the live CLI (`claude --help` and each
`claude <cmd> --help`), not from this repo — it is the "what does Claude Code
actually expose" quick-review sheet the rest of the docs assume.

`snapback` never links `claude`; it spawns it as a child (resume/fork/attach/send)
and reads `claude agents --json`. The terminal-safety and authoritative-from-file
rules around those spawns live in [PATTERNS.md](PATTERNS.md) and
[ARCHITECTURE.md](ARCHITECTURE.md); the runtime "`claude` on `PATH`" prerequisite
lives in [OPERATIONS.md](OPERATIONS.md#runtime-prerequisites). This file owns one
scope only: the surface of the `claude` command itself.

## Version pin (self-healing)

> **Captured against `claude 2.1.218` (Claude Code).**

Before trusting a flag or command below, compare the installed version:

```sh
claude --version   # e.g. "2.1.218 (Claude Code)"
```

- **Installed == pinned** → this doc matches the live CLI. Trust it.
- **Installed < pinned** → the local install is **behind this doc**. Newer flags
  listed here may not exist yet; `claude update` (or `claude install latest`)
  brings the binary up to the documented surface. Do not assume a flag is gone
  just because an older local `claude` rejects it.
- **Installed > pinned** → **this doc is stale**, not the CLI. Re-capture and
  refresh it (see [Refreshing this doc](#refreshing-this-doc)) before relying on
  the tables; flags may have been added, renamed, or removed since 2.1.218.

Keep the pinned version above in sync with the tables — bumping one without the
other defeats the check.

## How snapback drives `claude`

The only invocations `snapback` depends on. Each is a **pure argv builder** with
an inline test asserting the exact string, so drift here is caught by
`cargo test`. Cross-references are to the builder that owns the shape.

| Purpose | Argv | Builder |
| --- | --- | --- |
| Resume a session in place | `claude -r <session-id>` | `resume::build_argv` (`src/resume.rs`) |
| Fork a session (new id) | `claude -r <session-id> --fork-session` | `resume::build_argv` |
| Dispatch a DEFINED agent | `claude --agent <name>` | `src/resume.rs` |
| Attach to a live background job | `claude attach <job-id>` | `resume::build_attach_argv` |
| Quick-send a reply (non-interactive) | `claude -p -r <session-id> --output-format json <message>` | `send::build_send_argv` (`src/send.rs`) |
| Release a held job before a reply | `claude stop <job-id>` | `send::build_stop_argv` |
| Detect live agents (gate probe) | `claude agents --json` | `agents::agents_argv` (`src/agents.rs`) |
| Detect live agents (incl. just-finished) | `claude agents --json --all` | `agents::agents_argv` |

Two of these — **`attach`** and **`stop`** — are hidden commands (see below).
`attach`/`stop` take the **short agent-view job id** (e.g. `ca56b543`), NOT the
full `sessionId`; passing a UUID returns exit 1 ("No job matching"). The `-r`
resume/fork/send paths take the **full `sessionId`**.

## Invocation form

```
claude [options] [command] [prompt]
```

Interactive session by default. With no `command`, a trailing `prompt` (or stdin)
is sent to a session. `-p/--print` switches to non-interactive one-shot output.

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
| `--fallback-model <model>` | `[P]` Fallback model(s), comma-separated, tried in order when the primary is overloaded. |
| `--agent <agent>` | Agent for the session; overrides the `agent` setting. |
| `--agents <json>` | JSON object defining custom agents inline. |
| `--effort <level>` | `low` \| `medium` \| `high` \| `xhigh` \| `max`. |

### Print / SDK mode

| Flag | Effect |
| --- | --- |
| `-p, --print` | Print the response and exit (pipes). Skips the trust dialog; only use in trusted dirs. |
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
| `--betas <betas...>` | Beta headers (API-key users only). |

### Session lifecycle & environment

| Flag | Effect |
| --- | --- |
| `--bg, --background` | Start as a background agent and return immediately (manage with `claude agents`). |
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
| `--bare` | Minimal mode: skip hooks/LSP/plugins/attribution/auto-memory/CLAUDE.md discovery. Sets `CLAUDE_CODE_SIMPLE=1`; auth is strictly `ANTHROPIC_API_KEY`/apiKeyHelper. |
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
| `auth` | Manage authentication (`login`/`logout`/`status`). |
| `auto-mode` | Inspect or reset the auto-mode classifier config. |
| `doctor` | Health-check the installation (read-only; no trust prompt). |
| `gateway` | Run the enterprise auth/telemetry gateway (`--config <path>`). |
| `install [target]` | Install a native build (`stable`/`latest`/version; `--force`). |
| `mcp` | Configure and manage MCP servers. |
| `plugin` \| `plugins` | Manage plugins and marketplaces. |
| `project` | Manage project state (`purge` deletes all Claude state for a project). |
| `setup-token` | Set up a long-lived auth token (requires a subscription). |
| `ultrareview [target]` | Cloud multi-agent review of the branch / a PR number / base branch. |
| `update` \| `upgrade` | Check for updates and install if available. |

## Hidden commands

Real, working subcommands that are **absent from `claude --help`**. Verified by
their own dedicated usage output (a non-command argument instead just re-prints
the top-level help). `snapback` **depends on both** — treat them as load-bearing,
not incidental.

| Command | Usage | Notes |
| --- | --- | --- |
| `claude attach <id>` | Open a background session in this terminal. | `←` returns to agent view; `Ctrl+Z` drops to the shell; the session keeps running either way. Takes the SHORT job id. |
| `claude stop <id>` | Stop a background session. | Conversation is KEPT (resume later with `attach`); only the live job registration drops — which is what lets `claude -p -r` reclaim the session. Takes the SHORT job id. |

Because they are undocumented in `--help`, a version bump can change or remove
them without a visible help diff. If `snapback`'s attach/send paths regress after
a `claude` update, re-verify these two first with `claude stop --help` /
`claude attach --help`.

## Selected subcommand flags

Only the commands `snapback` touches or that are useful for quick review. For the
rest, `claude <command> --help` is authoritative.

### `claude agents` (snapback's live-agent source)

| Flag | Effect |
| --- | --- |
| `--json` | Print active sessions (interactive + background) as a JSON array and exit — no TTY needed. This is the shape `snapback` parses fail-soft. |
| `--all` | With `--json`, also include just-completed background sessions. |
| `--cwd <path>` | Only sessions started under `<path>`. |
| `--agent` / `--model` / `--effort` / `--permission-mode` | Defaults for sessions dispatched from agent view. |
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
`marketplace` has `add <source>`, `list`, `remove|rm <name>`, `update [name]`.

### `claude project`

`purge [path]` — delete ALL Claude Code state for a project (transcripts, tasks,
file history, config entry). Destructive; relevant because it removes the JSONL
`snapback` reads.

### `claude auto-mode`

`config` (effective config as JSON), `defaults` (shipped default rules as JSON),
`critique` (AI feedback on custom rules), `reset` (remove the `autoMode` section
from user settings).

### `claude ultrareview`

`--json` (raw `bugs.json`) \| `--timeout <minutes>` (default 30). User-triggered
and billed; a session cannot launch it for you.

## Refreshing this doc

`claude` is an external binary, so the repo's `project-agent-docs` self-healing
stage cannot regenerate these facts — they must be re-captured from the live CLI:

```sh
claude --version
claude --help
for c in agents auth mcp plugin project install update ultrareview \
         doctor setup-token gateway auto-mode; do
  echo "== $c =="; claude "$c" --help
done
claude stop --help; claude attach --help   # hidden — re-verify explicitly
```

Update the tables **and** the [version pin](#version-pin-self-healing) together
when the surface changes. When a flag/command that `snapback` invokes changes,
also fix the matching argv builder and its inline test in `src/resume.rs`,
`src/send.rs`, or `src/agents.rs` — the code and this doc are the two halves of
one contract.
