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

> **Captured against `claude 2.1.235` (Claude Code).**

Before trusting a flag or command below, compare the installed version:

```sh
claude --version   # e.g. "2.1.235 (Claude Code)"
```

- **Installed == pinned** → this doc matches the live CLI. Trust it.
- **Installed < pinned** → the local install is **behind this doc**. Newer flags
  listed here may not exist yet; `claude update` (or `claude install latest`)
  brings the binary up to the documented surface. Do not assume a flag is gone
  just because an older local `claude` rejects it.
- **Installed > pinned** → **this doc is stale**, not the CLI. Re-capture and
  refresh it (see [Refreshing this doc](#refreshing-this-doc)) before relying on
  the tables; flags may have been added, renamed, or removed since 2.1.235.

Keep the pinned version above in sync with the tables — bumping one without the
other defeats the check.

## How snapback drives `claude`

The only invocations `snapback` depends on. Each is a **pure argv builder** with
an inline test asserting the exact string, so drift here is caught by
`cargo test`. Cross-references are to the builder that owns the shape.

| Purpose | Argv | Builder |
| --- | --- | --- |
| Resume a session in place | `claude -r <session-id> [--model <alias>]` | `resume::build_argv` (`src/resume.rs`) |
| Fork a session (new id) | `claude -r <session-id> --fork-session [--model <alias>]` | `resume::build_argv` |
| Dispatch a DEFINED agent | `claude --agent <name>` | `resume::build_new_argv` (`src/resume.rs`) |
| Start a new session on a drafted prompt | `claude [--agent <name>] [--model <alias>] <prompt>` | `resume::build_new_argv` |
| Start a BACKGROUND agent on a drafted prompt | `claude [--agent <name>] [--model <alias>] --bg <prompt>` | `send::build_bg_launch_argv` (`src/send.rs`) |
| Attach to a live background job | `claude attach <job-id>` | `resume::build_attach_argv` |
| Quick-send a reply (non-interactive) | `claude -p -r <session-id> --output-format json [--model <alias>] <message>` | `send::build_send_argv` (`src/send.rs`) |
| Release a held job before a reply, or interrupt a selected agent (`Ctrl-K`) | `claude stop <job-id>` | `send::build_stop_argv` |
| Detect live agents (gate probe) | `claude agents --json` | `agents::live_agents_argv` (`src/agents.rs`) |
| Detect live agents (incl. just-finished) | `claude agents --json --all` | `agents::agents_argv` |

Two of these — **`attach`** and **`stop`** — are hidden commands (see below).
`attach`/`stop` take the **short agent-view job id** (e.g. `ca56b543`), NOT the
full `sessionId`; passing a UUID returns exit 1 ("No job matching"). The `-r`
resume/fork/send paths take the **full `sessionId`**.

`[--model <alias>]` is the board's sticky override (`Ctrl-X m`). It is emitted
ONLY while one is armed — with none, every argv above is byte-identical to what it
was before the flag existed — and it is always placed BEFORE a trailing positional
(the new-session prompt, the reply message), since a flag trailing an operand is at
the mercy of the parser. **`attach` can never carry it**, and structurally rather
than by convention: `build_attach_argv` takes no model parameter at all. That is
deliberate — `claude attach` joins a process already running under a model, so a
`--model` there would be meaningless. `--agent` and `--model` compose freely, and
snapback emits both when both are set; the accepted alias set is below.

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
| `--fallback-model <model>` | `[P]` Fallback model(s), comma-separated, tried in order when the primary is overloaded. |
| `--agent <agent>` | Agent for the session; overrides the `agent` setting. |
| `--agents <json>` | JSON object defining custom agents inline. |
| `--effort <level>` | `low` \| `medium` \| `high` \| `xhigh` \| `max`. |
| `--autocompact <value>` | Auto-compact window size: `auto`, or a token budget in 100k–1M. |

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
| `sonnet[1m]` | **Absent from `--help`.** The 1M-context variant (`label:"Sonnet 5 (1M context)"` in the bundle). Its embedded `]` is why a `[^]]`-style capture regex truncates the array right here — see [Refreshing this doc](#refreshing-this-doc). |
| `opus[1m]` | **Absent from `--help`.** The 1M-context variant (`label:"Opus (1M context)"`). |
| `fable[1m]` | **Absent from `--help`.** Same `[1m]` naming; the bundle carries no label for this one. |
| `opusplan` | **Absent from `--help`.** Runs Opus for **plan mode** and the resting model otherwise — "plan with Opus, implement with Sonnet" as one alias. Confirmed from binary strings, including an `opusplan-mode-reminder`. It is also the CONTENT ANCHOR the capture command selects on: no other array in the bundle carries it. |

**This table is a POINT-IN-TIME RECORD FOR HUMANS, not a list `snapback` reads.**
`snapback` reads the same array out of the installed binary itself, at runtime
(`src/model_aliases.rs`), so a newly shipped or withdrawn alias reaches the
`Ctrl-X m` picker with no snapback release and **no edit here**. What
`tui::app::MODEL_ALIASES` holds is a five-entry COLD-START SEED — what the picker
draws in the frames before the probe answers, and what it keeps if the probe finds
nothing — and it is deliberately NOT hand-refreshed: a stale seed is cosmetic and
self-corrects. Refresh this table when you want the doc to describe the version in
the [pin](#version-pin-self-healing) above, never because a picker depends on it.

Neither the table nor the seed is a validation whitelist. `--model` also accepts a
**full model id** (`claude-sonnet-5`), so this is an alias set, not the accepted
domain: nothing in `snapback` rejects a `--model` value, and an invalid one is
claude's to refuse (a hard, non-zero failure — see below).

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
| `--cloud [value]` | Create a cloud session from a description, or attach to an existing one by session id or `claude.ai/code` URL. |
| `--environment <environment_id>` | Create a cloud session on a given self-hosted environment (`ccpool_…`). |
| `--teleport [session]` | Resume a teleport session, optionally by session ID. |
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
| `import [source]` | Import config from another AI coding agent (`codex`, `gemini`); `--dry-run`, `--yes[=<digest>]`. |
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

`--json` (raw `bugs.json`) \| `--timeout <minutes>` (default 30) \| `--post` /
`--no-post` (post the findings to the PR as you, PR targets only, one plain
comment — `--no-post` is the default and exists for parity with the
`/ultrareview` and `/code-review ultra` flags). User-triggered and billed; a
session cannot launch it for you.

## Refreshing this doc

`claude` is an external binary, so the repo's `project-agent-docs` self-healing
stage cannot regenerate these facts — they must be re-captured from the live CLI:

```sh
claude --version
claude --help
for c in agents auth mcp plugin project install update ultrareview \
         doctor setup-token gateway auto-mode import; do
  echo "== $c =="; claude "$c" --help
done
claude stop --help; claude attach --help   # hidden — re-verify explicitly

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

Three traps, each reproduced against the real binaries. Do not "simplify" past
any of them:

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
   `h9e` → `bze` → `SWe` → `qWe` → `UKe` across five consecutive releases. And the
   array immediately BEFORE the alias array is a 17-element full-model-id list
   (`["claude-3-5-haiku",…,"claude-sonnet-5"]`), nearly twice as long, so
   longest-wins on its own returns the wrong one. The content anchor is what
   discriminates: in 2.1.235 exactly one array in the bundle carries `opusplan`.

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
one contract.
