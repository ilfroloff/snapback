import * as vscode from 'vscode';
import { openSnapback } from './terminal';

export function activate(context: vscode.ExtensionContext): void {
    const disposable = vscode.commands.registerCommand('snapback.open', () => {
        openSnapback();
    });

    context.subscriptions.push(disposable);
}

export function deactivate(): void {
    // Nothing to clean up
}
