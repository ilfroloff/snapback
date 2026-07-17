#!/usr/bin/env node
'use strict';

// npm entry point for snapback, published as `snapback-tui`.
//
// WHY THIS PACKAGE EXISTS: installing snapback otherwise needs a Rust toolchain
// (`cargo install --git ...`). This hands out the PREBUILT binaries instead, so
// `npx snapback-tui install` / `bunx snapback-tui install` works on a machine
// that has never seen rustup.
//
// WHY IT CARRIES EVERY PLATFORM AT ONCE rather than the esbuild-style
// optionalDependencies split: the binaries are ~1.8M each, so all four together
// are ~4MB — the split would save ~2MB per install and cost five packages to
// keep version-synced on every release, plus npm's known optionalDeps/lockfile
// bugs. YAGNI: esbuild needs that pattern because its binaries are ~10M x 20
// platforms. We are two orders of magnitude away from that problem.
//
// WHY IT RUNS NO LIFECYCLE SCRIPTS, deliberately: bun blocks `postinstall` for
// untrusted packages by default (only the top-500 npm packages are auto-trusted),
// so the usual "download the binary in postinstall" design would silently no-op
// under `bunx`. Everything here happens in this `bin` entry, which bunx invokes
// directly and therefore never blocks.
//
// WHY `install` IS THE BLESSED PATH rather than bare `npx snapback-tui`: it puts
// the native binaries on PATH, so the TUI thereafter runs with NO node in the
// process tree. That matters more here than for a typical CLI — snapback takes
// raw mode and an alt screen, and spawns `claude` as a child that hands the
// terminal back. Every process between the terminal and the TUI is one more
// thing that can leak a mode or swallow a signal. Bare `npx` is supported for a
// try-before-you-install run; see `runTui` for what that costs.

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawn } = require('node:child_process');

// The names to put on PATH. `cargo install` produces both, so this matches what
// a from-source install gives you.
const INSTALL_NAMES = ['snapback', 'sb'];

// ...but only ONE binary is shipped per platform, and both names are copies of
// it. That is not a shortcut, it is what the crate itself guarantees: src/main.rs
// and src/bin/sb.rs are the same 250-byte shim calling `snapback::run()`, and
// "no `argv[0]` dispatch, so `sb` and `snapback` are the same program under two
// names" is stated as a deliberate contract in src/bin/sb.rs and src/lib.rs.
// Shipping the second copy would double the tarball (6.2MB -> 3.1MB measured) to
// carry the same logic twice. The release workflow pins the contract before
// relying on it, by diffing the two real binaries' behaviour on every build.
const SHIPPED_BIN_NAME = 'snapback';

// Where the per-platform binaries sit inside the published tarball. Populated by
// .github/workflows/npm-release.yml, NOT committed — see npm/.gitignore.
const BIN_ROOT = path.join(__dirname, 'bin');

// The platforms the release workflow builds, as `${process.platform}-${process.arch}`.
// This list and the workflow's build matrix are ONE fact in two files: adding a
// target means adding it to both, or `install` reports a platform as supported
// and then fails to find its binary.
const SUPPORTED_PLATFORMS = ['darwin-arm64', 'darwin-x64', 'linux-x64', 'linux-arm64'];

// Default install target. `~/.local/bin` is the de facto user-level bin dir on
// both macOS and Linux and is already on PATH in most setups; when it is not, we
// say so rather than silently installing somewhere inert.
const DEFAULT_INSTALL_DIR = path.join(os.homedir(), '.local', 'bin');

// Env override for the install dir, for anyone who keeps user binaries elsewhere.
const INSTALL_DIR_ENV = 'SNAPBACK_INSTALL_DIR';

// rwxr-xr-x. Copied files inherit the source mode, but the tarball round trip is
// not something to bet the executable bit on, so it is set explicitly.
const MODE_EXEC = 0o755;

// Shell rc file per shell, for the "not on PATH" hint. Bare `sh`/unknown shells
// fall through to a generic hint rather than a wrong filename.
const SHELL_RC = {
  zsh: '~/.zshrc',
  bash: '~/.bashrc',
};

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/** `${platform}-${arch}` key naming a build target. */
function platformKey(platform, arch) {
  return `${platform}-${arch}`;
}

/** Whether the release workflow builds a binary for this key. */
function isSupported(key) {
  return SUPPORTED_PLATFORMS.includes(key);
}

/**
 * Whether `dir` is already an entry in a PATH-shaped string.
 *
 * Resolves both sides so `~/.local/bin`, `/Users/x/.local/bin` and a trailing
 * slash all compare equal — a false "not on PATH" would send the user to edit an
 * rc file that already has the line.
 */
function dirOnPath(dir, pathEnv) {
  const target = path.resolve(dir);
  return String(pathEnv || '')
    .split(path.delimiter)
    .filter(Boolean)
    .some((entry) => {
      try {
        return path.resolve(entry) === target;
      } catch {
        // A malformed PATH entry is not a reason to fail the install.
        return false;
      }
    });
}

/** The rc file to suggest for a given `$SHELL`, or null if we cannot tell. */
function shellRcFor(shellPath) {
  if (!shellPath) return null;
  return SHELL_RC[path.basename(String(shellPath))] || null;
}

// ---------------------------------------------------------------------------
// Impure drivers
// ---------------------------------------------------------------------------

/**
 * Absolute path to the bundled binary for this host.
 *
 * Fails LOUDLY, with the reason and a route forward. An installer that guesses
 * on an unsupported host would hand back a binary that cannot exec, and the
 * error would surface later wearing a worse disguise.
 */
function resolveBinary() {
  const key = platformKey(process.platform, process.arch);

  if (!isSupported(key)) {
    throw new Error(
      `snapback has no prebuilt binary for ${key}.\n` +
        `Prebuilt: ${SUPPORTED_PLATFORMS.join(', ')}.\n` +
        `Build from source instead (needs the Rust toolchain):\n` +
        `  cargo install --git https://github.com/ilfroloff/snapback`
    );
  }

  const binPath = path.join(BIN_ROOT, key, SHIPPED_BIN_NAME);
  if (!fs.existsSync(binPath)) {
    // Supported platform, missing file => the published tarball is malformed
    // (the build matrix and SUPPORTED_PLATFORMS drifted apart). Say that,
    // rather than blaming the user's machine.
    throw new Error(
      `This snapback-tui package is missing its ${key} binary (expected ${binPath}).\n` +
        `That is a packaging bug, not a problem on your end — please report it:\n` +
        `  https://github.com/ilfroloff/snapback/issues`
    );
  }
  return binPath;
}

/**
 * Copy one binary into place.
 *
 * Writes a temp file and renames it, rather than copying onto the destination.
 * Two reasons, both real: a plain copy onto a RUNNING binary fails with ETXTBSY
 * on Linux, and a copy that dies midway leaves a truncated, executable file
 * behind. rename(2) is atomic within a filesystem, so the destination is either
 * the old binary or the new one, never half of one.
 */
function installBinary(srcPath, dstPath) {
  const tmpPath = `${dstPath}.tmp-${process.pid}`;
  try {
    fs.copyFileSync(srcPath, tmpPath);
    fs.chmodSync(tmpPath, MODE_EXEC);
    fs.renameSync(tmpPath, dstPath);
  } catch (err) {
    try {
      fs.rmSync(tmpPath, { force: true });
    } catch {
      // Best-effort cleanup; the original error is the one worth reporting.
    }
    throw err;
  }
}

/** `install` — put the native binaries on PATH. */
function runInstall() {
  const targetDir = process.env[INSTALL_DIR_ENV] || DEFAULT_INSTALL_DIR;

  // Resolve BEFORE writing anything, so an unsupported platform or a malformed
  // package fails having changed nothing on disk.
  const srcPath = resolveBinary();

  fs.mkdirSync(targetDir, { recursive: true });
  for (const name of INSTALL_NAMES) {
    installBinary(srcPath, path.join(targetDir, name));
  }

  console.log(`Installed ${INSTALL_NAMES.join(', ')} to ${targetDir}`);

  if (dirOnPath(targetDir, process.env.PATH)) {
    console.log('\nRun it:\n  snapback        # this folder\'s sessions\n  sb -a           # every folder, grouped');
  } else {
    const rc = shellRcFor(process.env.SHELL);
    console.log(
      `\nWARNING: ${targetDir} is not on your PATH, so the commands above will not resolve yet.` +
        `\nAdd it${rc ? ` (${rc})` : ''}:` +
        `\n  export PATH="${targetDir}:$PATH"`
    );
  }

  console.log(`\nUninstall:\n  rm ${INSTALL_NAMES.map((n) => path.join(targetDir, n)).join(' ')}`);
}

/**
 * Bare `npx snapback-tui [args]` — run the TUI straight from the package.
 *
 * A try-before-you-install convenience, NOT the blessed path: this leaves node
 * sitting between the terminal and a program that owns raw mode, the alt screen,
 * and a `claude` child. Two things follow, and both are load-bearing:
 *
 *  - stdio is INHERITED, never piped. The TUI needs the real tty; piping would
 *    also deadlock the `claude` hand-off once a pipe filled.
 *  - SIGINT/SIGTSTP are IGNORED HERE and left to the child. Ctrl-C and Ctrl-Z
 *    reach the whole foreground process group, so node gets them too; on the
 *    default handler node would exit FIRST and hand back a terminal still in raw
 *    mode with the alt screen up, stealing the restore the TUI does on its way
 *    out. Ignoring them lets the child run its own teardown and lets us report
 *    its real exit status.
 */
function runTui(args) {
  const child = spawn(resolveBinary(), args, { stdio: 'inherit' });

  for (const sig of ['SIGINT', 'SIGTERM', 'SIGHUP', 'SIGTSTP']) {
    process.on(sig, () => {});
  }

  child.on('error', (err) => {
    console.error(`Failed to run snapback: ${err.message}`);
    process.exitCode = 1;
  });

  child.on('exit', (code, signal) => {
    // Report the child's status honestly: its exit code, or the shell's
    // 128+signum convention when a signal killed it. Never a flat 0 — snapback
    // distinguishes a clean quit from a failed hand-off by exit status, and a
    // wrapper that flattens that is lying about what happened.
    process.exitCode = signal ? 128 + (os.constants.signals[signal] || 0) : code ?? 1;
  });
}

function main() {
  const [command, ...rest] = process.argv.slice(2);

  try {
    // `install` is intercepted here and never reaches the binary. Safe: snapback
    // takes no positional arguments and ignores unrecognized ones, so there is
    // no `snapback install` being shadowed. Note the asymmetry that follows —
    // the INSTALLED `snapback install` just launches the TUI, ignoring the word.
    // Only the npm wrapper installs, which is why cli.rs's USAGE does not
    // mention it.
    if (command === 'install') {
      runInstall();
      return;
    }
    runTui(command === undefined ? [] : [command, ...rest]);
  } catch (err) {
    console.error(err.message);
    process.exitCode = 1;
  }
}

// Only run when invoked as the command; `require`d (by scripts/preflight.js) this
// is a module exporting the facts below, so the preflight's idea of what must be
// in the tarball cannot drift from the installer's idea of what it will look for.
if (require.main === module) {
  main();
}

module.exports = {
  INSTALL_NAMES,
  SHIPPED_BIN_NAME,
  BIN_ROOT,
  SUPPORTED_PLATFORMS,
  platformKey,
  isSupported,
  dirOnPath,
};
