# snapback

Browse, search, and resume **Claude Code** sessions from a terminal UI.

This is the npm distribution of [`snapback`](https://github.com/ilfroloff/snapback),
a single self-contained Rust [ratatui](https://ratatui.rs) TUI. It ships prebuilt
binaries, so installing needs **no Rust toolchain**.

> Published as **`snapback-tui`** because the `snapback` name on npm is taken by
> an unrelated package. The installed commands are still `snapback` and `sb`.

## Install

```sh
npx snapback-tui install
# or
bunx snapback-tui install
```

That copies the native `snapback` and `sb` binaries to `~/.local/bin`, after
which you run them directly — no npx, and no Node in the process tree:

```sh
snapback       # browse the CURRENT folder's sessions
sb             # same thing (short alias)
snapback -a    # browse EVERY folder's sessions, grouped repo → branch
```

To install somewhere else, set `SNAPBACK_INSTALL_DIR`:

```sh
SNAPBACK_INSTALL_DIR=/usr/local/bin npx snapback-tui install
```

To uninstall, delete the two binaries — `install` prints the exact command.

### Run without installing

```sh
npx snapback-tui        # runs the TUI straight from the package
```

Handy for a look, but `install` is the better path: it takes Node out from
between your terminal and a program that owns raw mode and hands the terminal to
`claude` and back.

## Requirements

- **`claude` on your `PATH`** — snapback resumes into it.
- macOS (arm64/x64) or Linux (x64/arm64). On other platforms, build from source:
  `cargo install --git https://github.com/ilfroloff/snapback`

## Docs

Full feature list, key map, and configuration:
**[github.com/ilfroloff/snapback](https://github.com/ilfroloff/snapback)**

## License

Apache-2.0
