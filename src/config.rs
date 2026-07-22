//! Snapback-owned path resolution — the SINGLE place that reads the environment
//! to locate snapback's own directories.
//!
//! Every snapback-owned path (config today; state below; any future cache) is
//! resolved HERE and nowhere else: no other module reads the environment for
//! these locations. When a new snapback-owned path is needed (a cache dir, a
//! settings file, …), add its resolver to THIS module rather than reading an env
//! var elsewhere, so the "one env reader" invariant holds and the tests keep a
//! single injection seam. (No such resolver exists yet — YAGNI; this module is
//! simply their documented home.)
//!
//! The root is `$SNAPBACK_CONFIG_DIR` if set and non-empty (the test/override
//! seam, mirroring `$CLAUDE_PROJECTS_DIR` for the read-only Claude store), else
//! `~/.config/snapback`.
//!
//! **Deliberately non-XDG on macOS.** The default is `~/.config/snapback` on
//! EVERY platform, built from [`dirs::home_dir`] joined with `.config` — NOT
//! from [`dirs::config_dir`], which resolves to `~/Library/Application Support`
//! on macOS. snapback keeps ONE predictable, greppable `~/.config/snapback`
//! everywhere so a user finds (and can hand-edit or delete) their state in a
//! single documented place regardless of OS; a per-OS location would be a worse
//! fit for a personal terminal tool whose store (`~/.claude/projects`) already
//! lives under the home dir.

use std::path::PathBuf;

/// Env var that overrides snapback's config dir — the test/override seam,
/// mirroring `$CLAUDE_PROJECTS_DIR` for the read-only store. Set and non-empty
/// wins over the default; an empty value is treated as unset.
const CONFIG_DIR_ENV: &str = "SNAPBACK_CONFIG_DIR";

/// Directory name (under `~/.config`) that holds snapback's own files. Kept
/// DISTINCT from the Claude store so the read-only invariant there is never
/// crossed — this is snapback's dir, not `~/.claude/projects/`.
const CONFIG_DIR_NAME: &str = "snapback";

/// Subdirectory of the config dir holding snapback's PERSISTENT state (today the
/// hidden-session id set). Split out from the config root so a future
/// settings/config file at the root never sits beside churny state files.
const STATE_SUBDIR: &str = "state";

/// Resolve snapback's config directory — the ROOT of every snapback-owned path.
///
/// `$SNAPBACK_CONFIG_DIR` if set and non-empty, else `~/.config/snapback`.
///
/// The default is `~/.config/snapback` on EVERY platform (see the module doc for
/// the deliberate non-XDG-on-macOS choice): it is built from [`dirs::home_dir`]
/// joined with `.config`, NEVER [`dirs::config_dir`]. Home-less fallback: a
/// RELATIVE `.config/snapback` rather than a panic, mirroring the fail-soft
/// home-less fallback in `store::discover::store_root` (and the one the retired
/// `hidden::hidden_state_dir` used) — a missing home must never abort the board.
pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var(CONFIG_DIR_ENV) {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Some(home) = dirs::home_dir() {
        return home.join(".config").join(CONFIG_DIR_NAME);
    }
    // Last resort if the home dir cannot be resolved: a relative dir rather than
    // a panic, matching `store_root`'s home-less fallback.
    PathBuf::from(".config").join(CONFIG_DIR_NAME)
}

/// Resolve snapback's PERSISTENT-state directory: `<config>/state`
/// (`~/.config/snapback/state` by default). This is where snapback's own state
/// — today the hidden-session id set (`hidden::save_hidden`) — lives. The nested
/// `state/` need not pre-exist: the atomic write `create_dir_all`s it on demand.
pub fn state_dir() -> PathBuf {
    config_dir().join(STATE_SUBDIR)
}

/// The ONE crate-wide lock serializing EVERY test that mutates a process-global
/// env var (`SNAPBACK_CONFIG_DIR`, `CLAUDE_PROJECTS_DIR`, …). It lives HERE — the
/// module that OWNS env resolution for snapback's paths — because env vars are
/// process-global: a test in ANY module holding a per-module lock does not
/// exclude a test in another, so they race on `set_var`/`remove_var` under a
/// parallel `cargo test`. Every env-mutating test in every module (`config`,
/// `tui::app`, `tui::update`) reaches it via [`env_lock`] so there is exactly one
/// lock, one accessor, one reason.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquire the shared [`ENV_LOCK`], POISON-TOLERANT: a test that panics while
/// holding it poisons the mutex, but the env it guards is process-global, so the
/// next env-mutating test still needs exclusion. Recover the guard on poison
/// rather than letting one failing test cascade a `PoisonError` into every other
/// env test.
#[cfg(test)]
pub(crate) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::{SystemTime, UNIX_EPOCH};

    /// A unique, isolated temp dir under `std::env::temp_dir()` — NEVER the real
    /// config dir. Mirrors the `snapback-<tag>-<pid>-<nanos>` convention used
    /// across the crate's tests.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "snapback-config-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    // The env var, the `.config` parent, the `snapback` dir name and the `state`
    // subdir are the USER-FACING contract (a person types the env var and greps
    // the path), so these tests set the var by its LITERAL name and assert the
    // literal path segments — NEVER via `CONFIG_DIR_ENV` / `CONFIG_DIR_NAME` /
    // `STATE_SUBDIR`, which would move in lockstep with the code under test and
    // pass vacuously if the value were renamed.

    // --- config_dir -------------------------------------------------------

    #[test]
    fn config_dir_prefers_the_env_override_when_set() {
        let _guard = env_lock();
        let dir = unique_temp_dir("config-override");
        std::env::set_var("SNAPBACK_CONFIG_DIR", &dir);
        assert_eq!(
            config_dir(),
            dir,
            "a set, non-empty `SNAPBACK_CONFIG_DIR` must win over the default"
        );
        std::env::remove_var("SNAPBACK_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_dir_defaults_under_dot_config_and_ends_in_snapback() {
        let _guard = env_lock();
        // Both an unset AND an empty override must fall through to the default.
        std::env::remove_var("SNAPBACK_CONFIG_DIR");
        let unset = config_dir();
        std::env::set_var("SNAPBACK_CONFIG_DIR", "");
        let empty = config_dir();
        std::env::remove_var("SNAPBACK_CONFIG_DIR");

        assert_eq!(unset, empty, "an empty override is treated as unset");
        // The default always ends in `snapback`, whether resolved from the home
        // dir or the home-less fallback.
        assert_eq!(
            unset.file_name().and_then(|n| n.to_str()),
            Some("snapback"),
            "the default config dir must be a `snapback` dir, not the Claude store"
        );
        // ...and it always sits directly under a `.config` parent — NEVER under
        // `dirs::config_dir()` (which is `~/Library/Application Support` on
        // macOS). This pins the deliberate non-XDG-on-macOS choice.
        assert_eq!(
            unset
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str()),
            Some(".config"),
            "the default lives under `~/.config` on every platform, not `dirs::config_dir()`"
        );
        if let Some(home) = dirs::home_dir() {
            assert_eq!(unset, home.join(".config").join("snapback"));
        }
    }

    // --- state_dir --------------------------------------------------------

    #[test]
    fn state_dir_is_the_state_subdir_of_config_dir() {
        let _guard = env_lock();
        let dir = unique_temp_dir("state-sub");
        std::env::set_var("SNAPBACK_CONFIG_DIR", &dir);
        assert_eq!(
            state_dir(),
            config_dir().join("state"),
            "state_dir is always config_dir() joined with the `state` subdir"
        );
        assert_eq!(
            state_dir(),
            dir.join("state"),
            "with the override set, persistent state lands under `<config>/state`"
        );
        std::env::remove_var("SNAPBACK_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
