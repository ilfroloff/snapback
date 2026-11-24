import * as vscode from 'vscode';

export interface SnapbackConfig {
    /** Where to open the snapback terminal: "editor" (fullscreen tab) or "panel" (maximized panel) */
    location: 'editor' | 'panel';
    /** The snapback binary command (default: "sb") */
    command: string;
    /** Arguments to pass to the snapback command (default: ["-p"]) */
    args: string[];
}

/**
 * Read the snapback configuration from VS Code settings.
 */
export function getConfig(): SnapbackConfig {
    const config = vscode.workspace.getConfiguration('snapback');

    return {
        location: config.get<'editor' | 'panel'>('location', 'editor'),
        command: config.get<string>('command', 'sb'),
        args: config.get<string[]>('args', ['-p']),
    };
}
