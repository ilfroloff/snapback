# Operations

Commands, scripts, and the validation checklist. There is no CI config, no
`rustfmt.toml`/`clippy.toml`, and no build system beyond Cargo — this is a
single binary crate.

## Build

```sh
cargo build            # debug → target/debug/snapback
cargo build --release  # optimized → target/release/snapback
```

## Test

All tests are inline unit tests (see [PATTERNS.md](PATTERNS.md#testing-patterns)).

```sh
cargo test             # run everything
cargo test <name>      # filter by test name, e.g. cargo test discover
cargo test -- --nocapture
```

Watcher tests touch the real filesystem in isolated temp dirs and sleep across
debounce windows, so the suite is not instantaneous; they never read
`~/.claude/projects`.

## Lint & format

No config is committed, so the toolchain defaults apply. Keep the tree clean:

```sh
cargo fmt
cargo clippy --all-targets
```

The `dead_code` lint is load-bearing here — do not silence it broadly (see
[PATTERNS.md](PATTERNS.md) rule 9).

## Run

```sh
./target/release/snapback   # browse the CURRENT folder's sessions (default)
snapback -a                 # or --all: every folder, grouped repo → branch
snapback -h                 # help
```

`snapback` and `sb` are two thin binary shims over the library crate. Both are
produced by one install and run the same program (no `argv[0]` dispatch):

```sh
cargo install --path .   # installs both snapback and sb
```

There is no separate search mode: you start in browse and typing filters live.
`Tab` widens name-only → name+content; `Ctrl-A` toggles scope. See the README
for the full key map.

## Environment

| Var | Default | Effect |
| --- | --- | --- |
| `CLAUDE_PROJECTS_DIR` | `~/.claude/projects` | Overrides the session store root (used by both the TUI and `--print-list`). |

## Hidden debug mode

`snapback --print-list` loads the store non-interactively and prints one line
per resumable session (`session_id\trepo\tbranch\tcwd`), a repo→branch group
breakdown, and a total — **without** starting the TUI or touching `claude`.
Meta lines are prefixed `#`, so `snapback --print-list | grep -vc '^#'` counts
resumable sessions. Use it to inspect what discovery actually finds (e.g. to
confirm subagents/sidecars were excluded). It is intentionally omitted from
`--help`.

## Runtime prerequisites

- A real **TTY** — the interactive UI refuses to run when stdout is not a
  terminal (it prints a count and exits instead of panicking).
- `claude` on `PATH` — the binary that resume/fork/attach spawn, and the source
  of live-agent badges. If it is missing or fails to launch, the hand-off fails
  soft to a board status message and live detection degrades to "nothing is
  live", so the live-agent badges disappear.

## Validation checklist before finishing a change

1. `cargo build` — compiles clean.
2. `cargo test` — all inline tests pass; add tests for new pure logic.
3. `cargo clippy --all-targets` — no new warnings (especially not from a broad
   `dead_code` allow).
4. `cargo fmt` — formatted.
5. If discovery/parsing/model changed: `snapback --print-list` against a real or
   fixture store still shows the expected resumable set (no subagents/sidecars).
6. If keys/flags changed: the key table in `update.rs`, the `USAGE`/`KEYS` in
   `cli.rs`, the help line in `view.rs`, and the README key map all agree.
7. Refresh the agent docs per the self-healing stage in
   [../../AGENTS.md](../../AGENTS.md).
