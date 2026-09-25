//! Build script: capture the source commit into compile-time env vars so a
//! running debug build can state exactly which code it was built from.
//!
//! Emits two vars consumed by `src/tui/view.rs::version_label`:
//! `SNAPBACK_GIT_HASH` (`git rev-parse --short HEAD`, or `unknown`) and
//! `SNAPBACK_GIT_DIRTY` (`1` when the working tree had uncommitted changes at
//! build time, else `0`).
//!
//! Only debug builds render these (release shows `v<crate-version>`), so a
//! missing git binary or a non-repo checkout (e.g. `cargo install` from a
//! packaged crate) degrades to `unknown`/`0` rather than failing the build —
//! FAIL-SOFT, matching the crate's stance on hostile inputs.
//!
//! ## What this script must be re-run by, and why the default was not enough
//!
//! Cargo's default — re-run whenever any package file changes — cannot refresh the
//! HASH at all. **`git commit` never changes a file's mtime**: it writes objects and
//! moves a ref, leaving every working-tree file exactly as it was. So the default sees
//! nothing and the baked hash lags after ANY commit, not merely one that touches no
//! tracked package file. (Measured: this worktree's debug binary reported the
//! commit BEFORE its HEAD.) The label then names the wrong source, which is worse than
//! no label — it is the one question the label exists to answer.
//!
//! So the git files are watched explicitly. On a branch, `HEAD` holds a SYMBOLIC ref
//! (`ref: refs/heads/<branch>`) and does not move on commit — the REF FILE it names is
//! what moves — so BOTH are watched: the ref for commits, `HEAD` itself for the
//! checkouts and branch switches that move it instead (and for a detached HEAD, where
//! it holds the hash directly and there is no ref to follow).
//!
//! Their paths are asked of `git rev-parse --git-path` rather than built from `.git`,
//! because `.git` is a DIRECTORY only in a plain checkout: in a WORKTREE it is a file
//! holding `gitdir: <path>`, whose per-worktree dir has a private `HEAD` but no
//! `refs/` — branch refs live in the COMMON dir. Asking git resolves every one of
//! those layouts with no path arithmetic here, and costs nothing new, since this
//! script already shells out to git and already treats a failure as "no signal".
//!
//! Two degradations, NEITHER able to bake a wrong hash — but only one of them is free.
//! A NON-REPO checkout (or no `git` on `PATH`) resolves no paths, so nothing git-shaped
//! is watched and the hash stays a constant `unknown`; every path still declared below
//! exists, so builds stay incremental. A PACKED ref is the one that costs: it has no
//! loose file, and a declared path Cargo cannot stat can never be satisfied — so this
//! script re-runs on EVERY build, where the package-file default ran it only when a
//! package file changed. The re-run itself is five short `git` shell-outs (four on a
//! detached HEAD) — but that is NOT the whole bill, and it is MEASURED, not estimated:
//! a scratch crate whose build script declared one `rerun-if-changed` path that does
//! not exist was built three times, and `cargo build -v` reported, on EVERY build,
//! ``Dirty <crate>: the file `<declared-path>` is missing`` followed by `Compiling`,
//! `Running build-script-build` and `Running rustc`. Cargo marks the CRATE dirty, not
//! merely the script, and re-runs `rustc` every build EVEN THOUGH the script's output
//! was byte-identical all three times. A packed ref therefore costs a FULL CRATE
//! RECOMPILE per build, not just the shell-outs. That cost is bounded and SELF-HEALING
//! rather than permanent: `git clone` writes the checked-out branch's ref LOOSE, so the
//! state is reached by `git gc` / `pack-refs`, and the next commit on that branch writes
//! the loose file back (a ref update never re-packs). It is paid rather than dodged
//! because every cheaper watch is a PROXY for "the ref moved", and the obvious one —
//! `.git/index` — does NOT move under `git reset --soft`, which moves the ref and the
//! hash: a fast build carrying a WRONG label, the one failure this mechanism exists to
//! prevent. A recompile is a steeper price than the shell-outs alone, which makes that
//! trade MORE worth making, not less: the alternative is not a cheaper correct build,
//! it is a wrong one.
//!
//! Emitting any `rerun-if-changed` REPLACES the package-file default, so the sources
//! that decide `-dirty` are re-declared below. `src/` is watched recursively, which
//! covers the dev loop the flag exists for (edit source, `cargo dev`), and `Cargo.lock`
//! joins its manifest because it is TRACKED and dirties on any dependency bump —
//! omitting it from the re-declared set was enough to let the flag report stale.
//! The residual gap is a dirty file OUTSIDE those paths (say `README.md` alone): the
//! flag can lag until the next source change. That is strictly smaller than the gap
//! it replaces — a hash wrong after every single commit — and it is a personal dev
//! indicator, so it is documented rather than chased.

use std::process::Command;

/// Fallback when git can't be queried (no binary, not a repository).
const GIT_HASH_UNKNOWN: &str = "unknown";

/// Package paths whose change can flip `SNAPBACK_GIT_DIRTY`, re-declared because
/// watching the git refs opts this script out of Cargo's watch-everything default.
/// `src` is a directory, which Cargo scans recursively. `Cargo.lock` sits next to its
/// manifest because it is tracked and dirties often, and an unwatched lockfile is a
/// stale flag waiting to happen.
const WATCHED_SOURCES: [&str; 4] = ["src", "Cargo.toml", "Cargo.lock", "build.rs"];

fn main() {
    let hash =
        git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| GIT_HASH_UNKNOWN.to_string());
    // `status --porcelain` prints one line per change; empty output == clean.
    let dirty = git(&["status", "--porcelain"]).is_some_and(|out| !out.is_empty());

    for path in WATCHED_SOURCES {
        rerun_if_changed(path);
    }
    watch_head();

    println!("cargo:rustc-env=SNAPBACK_GIT_HASH={hash}");
    println!("cargo:rustc-env=SNAPBACK_GIT_DIRTY={}", u8::from(dirty));
}

/// Watch the git files whose movement makes the baked hash stale: `HEAD`, and the
/// branch ref it points at when it is symbolic. Silent when git cannot answer.
fn watch_head() {
    if let Some(head) = git(&["rev-parse", "--git-path", "HEAD"]) {
        rerun_if_changed(&head);
    }
    // Non-zero (hence `None`) on a detached HEAD, where `HEAD` above is already the
    // file that moves and there is no ref to follow.
    let Some(reference) = git(&["symbolic-ref", "--quiet", "HEAD"]) else {
        return;
    };
    if let Some(path) = git(&["rev-parse", "--git-path", &reference]) {
        rerun_if_changed(&path);
    }
}

/// Ask Cargo to re-run this script when `path` changes.
fn rerun_if_changed(path: &str) {
    println!("cargo:rerun-if-changed={path}");
}

/// Run `git <args>` in the crate dir and return trimmed stdout, or `None` when
/// git is absent or exits non-zero. Never panics.
fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
