# Operations

Commands, scripts, CI, release automation, and the validation checklist. There
is no `rustfmt.toml`/`clippy.toml` (toolchain defaults apply) and no build system
beyond Cargo — this is a single binary crate. Two GitHub Actions workflows
automate the quality gates and the releases (see
[Continuous integration & releases](#continuous-integration--releases)).

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

## Continuous integration & releases

Two GitHub Actions workflows, both installing the pinned toolchain from
`rust-toolchain.toml` via `actions-rust-lang/setup-rust-toolchain`:

- **`🚀 CI`** (`.github/workflows/ci.yml`) — on every push and PR to `main`,
  runs the same gates as the [validation checklist](#validation-checklist-before-finishing-a-change):
  `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo build`, `cargo test`.
- **`🔖 Release`** (`.github/workflows/release-plz.yml`) — on every push to
  `main`, runs [release-plz](https://release-plz.dev) (config: `release-plz.toml`).

### How a release happens

Versioning is driven by the **Conventional Commit** type of each merged commit
(the types in [GIT_COMMIT_INSTRUCTIONS.md](../../GIT_COMMIT_INSTRUCTIONS.md)):

- `feat:` → **minor** bump (e.g. 0.1.0 → 0.2.0); `fix:`/`perf:` → **patch** — the
  same mapping on 0.x as on 1.x, because `features_always_increment_minor = true`.
- On each merge to `main`, release-plz opens/refreshes a single
  **"Release vX.Y.Z"** PR that accumulates the pending bump + changelog. Ordinary
  merges only update that PR — nothing is tagged yet.
- Merging the release PR lands the new version in `Cargo.toml`/`Cargo.lock` and
  makes release-plz cut the `vX.Y.Z` **git tag** + **GitHub Release**.

This crate is **never published to crates.io** (`publish = false`); the git tag
is the sole source of truth for "what is released" (`git_only = true`), so there
is no registry step. Users install a tagged version straight from git — see the
README [Install](../../README.md#install--run) section.

The release PR, commits, and tags are created with the built-in `GITHUB_TOKEN`,
which by design does **not** trigger other workflow runs, so `🚀 CI` is never
re-triggered by `🔖 Release` and there is no CI loop. Two repo-side prerequisites
enable this automation: the "Allow GitHub Actions to create and approve pull
requests" setting, and (optionally) a `RELEASE_PLZ_TOKEN` secret if `🚀 CI` should
also run on the release PR.

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
