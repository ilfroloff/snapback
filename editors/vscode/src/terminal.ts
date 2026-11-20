import * as vscode from 'vscode';
import { resolveBinaryPath } from './binary';
import { getConfig } from './config';

// Track the snapback terminal for reuse within the session
let snapbackTerminal: vscode.Terminal | undefined;

/**
 * Open snapback in a fullscreen terminal pane.
 *
 * - Reads settings: location, command, args
 * - Resolves the binary path
 * - Finds or creates a terminal named "snapback"
 * - Executes the command (prefer Shell Integration if available)
 */
export async function openSnapback(): Promise<void> {
    const config = getConfig();

    // Resolve the binary path
    const binaryPath = await resolveBinaryPath(config.command);
    if (!binaryPath) {
        const install = await vscode.window.showErrorMessage(
            'snapback binary not found. Install via: npx snapback-tui install',
            'Copy Install Command'
        );
        if (install === 'Copy Install Command') {
            await vscode.env.clipboard.writeText('npx snapback-tui install');
        }
        return;
    }

    // Check for an existing terminal named "snapback"
    const existingTerminal = findSnapbackTerminal();
    if (existingTerminal) {
        // Focus the existing terminal
        existingTerminal.show();
        return;
    }

    // Create a new terminal
    const location = config.location === 'panel'
        ? vscode.TerminalLocation.Panel
        : vscode.TerminalLocation.Editor;

    const terminal = vscode.window.createTerminal({
        name: 'snapback',
        location,
    });

    // Track the terminal for reuse
    snapbackTerminal = terminal;

    // Show the terminal
    terminal.show();

    // If location is panel, maximize it
    if (config.location === 'panel') {
        // VS Code 1.89+ supports maximize via show(true)
        // But the API is terminal.show(preserveFocus?: boolean), not maximize
        // For panel, we just show it; users can maximize manually or use defaultLocation setting
    }

    // Execute the command
    // Prefer Shell Integration if available (VS Code 1.89+)
    const args = config.args;
    const commandStr = `${binaryPath} ${args.join(' ')}`;

    if (terminal.shellIntegration) {
        // Shell Integration is available — use executeCommand for better lifecycle tracking
        terminal.shellIntegration.executeCommand(commandStr);
    } else {
        // Fall back to sendText
        terminal.sendText(commandStr);
    }
}

/**
 * Find an existing terminal named "snapback".
 *
 * Checks both the in-memory reference and the window's terminal list.
 */
function findSnapbackTerminal(): vscode.Terminal | undefined {
    // Check the in-memory reference first
    if (snapbackTerminal) {
        // Verify it's still in the window's terminal list (not closed)
        const stillExists = vscode.window.terminals.includes(snapbackTerminal);
        if (stillExists) {
            return snapbackTerminal;
        }
        // Terminal was closed, clear the reference
        snapbackTerminal = undefined;
    }

    // Fall back to name-based lookup
    const found = vscode.window.terminals.find(t => t.name === 'snapback');
    if (found) {
        snapbackTerminal = found;
        return found;
    }

    return undefined;
}

// Listen for terminal close events to clear the reference
vscode.window.onDidCloseTerminal(terminal => {
    if (terminal === snapbackTerminal) {
        snapbackTerminal = undefined;
    }
});
