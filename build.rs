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
//! We deliberately emit NO `rerun-if-changed` instructions. Cargo's default is
//! then to re-run this script whenever any package file changes, so the `-dirty`
//! flag refreshes on the common dev loop (edit source, `cargo dev`). The one gap
//! is a commit that touches no tracked package file: the hash can lag one build.
//! Acceptable for a personal dev indicator; documented so it is not mistaken for
//! a bug.

use std::process::Command;

/// Fallback when git can't be queried (no binary, not a repository).
const GIT_HASH_UNKNOWN: &str = "unknown";

fn main() {
    let hash =
        git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| GIT_HASH_UNKNOWN.to_string());
    // `status --porcelain` prints one line per change; empty output == clean.
    let dirty = git(&["status", "--porcelain"]).is_some_and(|out| !out.is_empty());

    println!("cargo:rustc-env=SNAPBACK_GIT_HASH={hash}");
    println!("cargo:rustc-env=SNAPBACK_GIT_DIRTY={}", u8::from(dirty));
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
