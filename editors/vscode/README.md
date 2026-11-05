# Snapback VS Code Extension

Open [snapback](https://github.com/ilfroloff/snapback) (`sb`) in a fullscreen terminal pane with one command.

## Features

- **One-command launcher**: Open the snapback board in a fullscreen editor-tab terminal or maximized panel
- **Binary resolution**: Automatically finds `sb` from PATH, `~/.local/bin`, `/usr/local/bin`, or `$SNAPBACK_INSTALL_DIR`
- **Terminal reuse**: Focuses the existing snapback terminal if already open, instead of stacking duplicates
- **Configurable**: Choose location (editor/panel), binary command, and arguments via settings
- **Remote-SSH compatible**: Works unchanged over Remote-SSH and Dev Containers

## Installation

Download the `.vsix` file from the [GitHub releases](https://github.com/ilfroloff/snapback/releases) page, then:

1. Open VS Code
2. Open the Command Palette (`Ctrl+Shift+P` / `Cmd+Shift+P`)
3. Run **"Extensions: Install from VSIX..."**
4. Select the downloaded `.vsix` file

## Usage

1. Open the Command Palette (`Ctrl+Shift+P` / `Cmd+Shift+P`)
2. Run **"Snapback: Open Snapback"**
3. The snapback board opens in a fullscreen editor tab (or maximized panel, per settings)

From there, use snapback as usual: `Enter` to resume a session, `Ctrl-R` to quick reply, `Ctrl-N` to start a background agent, etc.

## Settings

| Setting | Type | Default | Description |
|---------|------|---------|-------------|
| `snapback.location` | `"editor"` \| `"panel"` | `"editor"` | Where to open the snapback terminal. `"editor"` opens it as a fullscreen editor tab; `"panel"` opens it in the integrated terminal panel (maximized). |
| `snapback.command` | `string` | `"sb"` | The snapback binary to run. Resolved from PATH, `~/.local/bin`, `/usr/local/bin`, or `$SNAPBACK_INSTALL_DIR`. |
| `snapback.args` | `string[]` | `["-p"]` | Arguments to pass to the snapback command. Default is `["-p"]` for project scope (repo + worktrees). |

### Example: Global Board

To open the global board (all repos) instead of the project board:

```json
{
  "snapback.args": ["-a"]
}
```

## Suggested Keybinding

No default keybinding is provided to avoid collisions across editor forks (VS Code, Cursor, VSCodium). Add your own in `keybindings.json`:

**Linux/Windows:**
```json
{
  "key": "ctrl+alt+s",
  "command": "snapback.open"
}
```

**macOS:**
```json
{
  "key": "cmd+alt+s",
  "command": "snapback.open"
}
```

## Troubleshooting

### Binary Not Found

If you see **"snapback binary not found"**, the extension couldn't locate `sb` in your PATH or standard install locations.

**Solution:** Install snapback via:
```bash
npx snapback-tui install
```

Or set `snapback.command` to the full path to your `sb` binary:
```json
{
  "snapback.command": "/path/to/sb"
}
```

### Ctrl-Chord Interception

Some of snapback's keybindings (`Ctrl-R`, `Ctrl-N`, `Ctrl-K`, etc.) may be intercepted by VS Code's workbench commands.

**Solution:** Add snapback's chords to `terminal.integrated.commandsToSkipShell` in your settings:

```json
{
  "terminal.integrated.commandsToSkipShell": [
    "-workbench.action.terminal.toggleTerminal",
    "-workbench.action.terminal.new",
    "-workbench.action.terminal.newInActiveWorkspace"
  ]
}
```

Note: The exact commands to exclude depend on which chords are intercepted. Start with the above and add more as needed.

### macOS: Dock-Launched VS Code

If VS Code is launched from the Dock (not the terminal), it may not inherit your login shell's PATH, so `~/.local/bin` is not in PATH.

**Solution:** The extension already checks `~/.local/bin` explicitly, so this should work. If not, set `snapback.command` to the full path:
```json
{
  "snapback.command": "/Users/yourname/.local/bin/sb"
}
```

## Requirements

- [snapback](https://github.com/ilfroloff/snapback) installed (`sb` binary on PATH or in `~/.local/bin`)
- [Claude Code](https://claude.ai) installed (snapback requires `claude` to be available)
- VS Code 1.89.0 or later

## License

Apache-2.0
