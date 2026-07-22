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

### The lint gate reports clean in two ways that are lies

Both have already shipped a warning past a review pass here. Neither is
hypothetical.

1. **`--all-targets` is not optional.** Bare `cargo clippy` lints only the
   default target: it never builds the test/bench/example targets, so anything
   that only fires there is invisible. The concrete case: a fn whose sole caller
   is `#[cfg(not(test))]`-gated is **dead under `lib test`** and warns — but only
   with `--all-targets`. Bare `cargo clippy` on that same tree is genuinely,
   silently clean.
2. **A re-run tells you nothing.** Cargo caches lint results. A second
   `cargo clippy --all-targets` on an unchanged tree prints `Finished` and no
   warnings *whether or not the first run warned*. So "I ran clippy, it was
   clean" is not evidence unless that run actually rebuilt — and a run that
   finishes in ~0.05s did not.

Force a real run before believing the gate:

```sh
touch src/lib.rs && cargo clippy --all-targets   # or: cargo clean -p snapback
```

The general rule this serves lives in [AGENTS.md](../../AGENTS.md)'s execution
checklist: **a check that could not have gone red is not evidence.** When
reporting a gate green, report what made it capable of being red.

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

Three GitHub Actions workflows, all installing the pinned toolchain from
`rust-toolchain.toml` via `actions-rust-lang/setup-rust-toolchain`:

- **`🚀 CI`** (`.github/workflows/ci.yml`) — on every PR, runs the same gates as
  the [validation checklist](#validation-checklist-before-finishing-a-change):
  `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo build`, `cargo test`.
- **`🔖 Release`** (`.github/workflows/release-plz.yml`) — on every push to
  `main`, runs [release-plz](https://release-plz.dev) (config: `release-plz.toml`).
- **`📦 Publish to npm`** (`.github/workflows/npm-release.yml`) — on every
  `v*` tag, cross-compiles the four supported platforms and publishes the
  prebuilt binaries to npm as `snapback-tui` (package source: `npm/`).

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
- That tag then fires **`📦 Publish to npm`**, which builds the four platform
  binaries from the tagged source and publishes them as `snapback-tui`.

This crate is **never published to crates.io**, and that guarantee rests on
`release-plz.toml` — `publish = false` there stops the registry step, while
`git_only = true` makes the git tag the sole source of truth for "what is
released". Note the scope of that flag: it governs **`cargo publish`**, not
"never publish anything". npm ships prebuilt *binaries*, not crate source, from a
separate workflow keyed off the tag — the two do not conflict, and the npm
publish must not be folded into `release-plz.toml`.

Users install from npm (`npx snapback-tui install`) or from the tagged git ref —
see the README [Install](../../README.md#install) section.

### The npm package

`npm/` is the package source; `npm/bin/` is build output and is **never
committed** (gitignored) — the binaries' only source of truth is the tag they
were built from.

Three things there are deliberate and will look wrong to a future reader:

- **`package.json` carries `version: "0.0.0"`.** It is a placeholder. The git tag
  is the sole source of truth for the version, so the workflow stamps it in at
  publish time and cross-checks it against `Cargo.toml`. A real version committed
  here would be a second source of truth that nothing keeps in sync.
- **Only `snapback` is shipped per platform; `sb` is a copy made at install
  time.** The two binaries are the same program (`src/main.rs` and `src/bin/sb.rs`
  are the same shim; the crate documents "no `argv[0]` dispatch"). Shipping both
  doubles the tarball (measured: 3.1MB → 6.2MB) to carry identical logic twice.
  The build job **verifies** that contract against the real binaries on every
  target rather than trusting the comment.
- **The package runs no lifecycle scripts on install.** `bun` blocks
  `postinstall` for untrusted packages by default, so the usual
  download-in-postinstall design would silently no-op under `bunx`. Everything
  happens in the `bin` entry, which `npx`/`bunx` invoke directly.

`npm/scripts/preflight.js` runs from `prepublishOnly` and is the last gate: it
refuses the placeholder version and any missing or truncated binary. It matters
because both failure modes are silent and land on the user, and an npm version
number can never be re-published once burned.

`Cargo.toml` intentionally sets **no `publish` key**, and must not gain one.
`publish = false` there marks the package non-publishable, and release-plz's
`release` command discards non-publishable packages *before* the git-only branch
that cuts the tag and the GitHub Release — the workflow then succeeds having
released nothing. Keep the guard in `release-plz.toml` only.

The release PR, commits, and tags are created with **`RELEASE_PLZ_TOKEN`**, a PAT
authored by a real account. That secret is **required**, with no fallback —
`release-plz.yml` reads it and nothing else, so a missing or revoked token fails
loudly at the point of use instead of 403ing later wearing a disguise.

The PAT is what makes the tag **trigger `📦 Publish to npm`**. A tag pushed with
the built-in `GITHUB_TOKEN` would trigger no workflow at all — GitHub suppresses
downstream triggers on `GITHUB_TOKEN`-authored events to break loops — so under
the built-in token the npm publish would silently never run. A PAT-authored tag
is an ordinary event and fires it.

No loop follows from that, and the reason is the trigger filters, not the token:
`🔖 Release` is `push: branches: [main]` with no `tags:` filter, and
`📦 Publish to npm` is `push: tags: ['v*']` and pushes nothing. Neither can
re-enter the other.

One repo-side prerequisite remains for the release PR: the "Allow GitHub Actions
to create and approve pull requests" setting. Publishing to npm needs an
**`NPM_TOKEN`** secret (an npm automation token).

## Environment

| Var | Default | Effect |
| --- | --- | --- |
| `CLAUDE_PROJECTS_DIR` | `~/.claude/projects` | Overrides the session store root (used by both the TUI and `--print-list`). |
| `SNAPBACK_CONFIG_DIR` | `~/.config/snapback` | Overrides snapback's OWN config dir (the single env-resolved root for snapback-owned paths; state lives in its `state/` subdir). Resolved only by the `config` module. |

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
