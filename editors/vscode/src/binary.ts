import * as path from 'path';
import * as os from 'os';
import { execFile } from 'child_process';
import { promisify } from 'util';
import * as fs from 'fs';

const execFileAsync = promisify(execFile);

/**
 * Resolve the snapback binary path.
 *
 * Resolution order:
 * 1. If `command` is an absolute path and exists, return it.
 * 2. Check if `command` is in PATH (via `which` / `command -v`).
 * 3. Check `~/.local/bin/<command>`.
 * 4. Check `/usr/local/bin/<command>`.
 * 5. Check `$SNAPBACK_INSTALL_DIR/<command>` if env var is set.
 *
 * Returns the resolved path or `undefined` if not found.
 */
export async function resolveBinaryPath(command: string): Promise<string | undefined> {
    // 1. Absolute path
    if (path.isAbsolute(command)) {
        if (fs.existsSync(command)) {
            return command;
        }
        return undefined;
    }

    // 2. Check PATH via `which` (Linux/macOS)
    try {
        const { stdout } = await execFileAsync('which', [command]);
        const resolved = stdout.trim();
        if (resolved && fs.existsSync(resolved)) {
            return resolved;
        }
    } catch {
        // `which` failed — command not in PATH, continue
    }

    // 3. Check ~/.local/bin/<command>
    const localBin = path.join(os.homedir(), '.local', 'bin', command);
    if (fs.existsSync(localBin)) {
        return localBin;
    }

    // 4. Check /usr/local/bin/<command>
    const usrLocalBin = path.join('/usr/local/bin', command);
    if (fs.existsSync(usrLocalBin)) {
        return usrLocalBin;
    }

    // 5. Check $SNAPBACK_INSTALL_DIR/<command>
    const installDir = process.env.SNAPBACK_INSTALL_DIR;
    if (installDir) {
        const customPath = path.join(installDir, command);
        if (fs.existsSync(customPath)) {
            return customPath;
        }
    }

    return undefined;
}
