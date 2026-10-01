//! Environment resolution — the SINGLE place that reads the environment for ANY
//! path snapback resolves: its OWN (`SNAPBACK_CONFIG_DIR`) AND the external
//! Claude profile it operates against (`CLAUDE_CONFIG_DIR`, `CLAUDE_PROJECTS_DIR`).
//!
//! Two reasons this stays ONE module rather than each consumer reading its own
//! variable:
//!
//! - **One env reader.** No other module may read `CLAUDE_CONFIG_DIR` or
//!   `CLAUDE_PROJECTS_DIR` (or `SNAPBACK_CONFIG_DIR`) directly. A second reader
//!   could resolve a second, disagreeing profile — the store view, every
//!   spawned `claude` child, and the user-level `agents/*.md` list must always
//!   agree on which profile they mean, or the delete guard's writer check
//!   (profile-scoped) silently widens against the wrong store.
//! - **One test-injection seam.** Every env-mutating test acquires [`env_lock`]
//!   from here, so a parallel `cargo test` cannot race two tests mutating the
//!   same process-global variable.
//!
//! Every snapback-owned path (config today; state below; any future cache) is
//! resolved HERE and nowhere else: no other module reads the environment for
//! these locations. When a new snapback-owned path is needed (a cache dir, a
//! settings file, …), add its resolver to THIS module rather than reading an env
//! var elsewhere, so the "one env reader" invariant holds and the tests keep a
//! single injection seam.
//!
//! Snapback's own config root is `$SNAPBACK_CONFIG_DIR` if set and non-empty
//! (the test/override seam, mirroring `$CLAUDE_PROJECTS_DIR` for the read-only
//! Claude store), else `~/.config/snapback`. The external Claude profile root is
//! [`claude_config_dir`] — see its doc comment for its own override and
//! fallback shape.
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

/// Env var that selects the Claude PROFILE snapback operates against: the store
/// root, every spawned `claude` child, and the user-level `agents/*.md` list all
/// derive from it (see [`claude_config_dir`]).
///
/// Honored by the installed `claude` binary — confirmed empirically (`strings -a
/// <claude binary> | grep -c CLAUDE_CONFIG_DIR` reports dozens of hits) — but it
/// is UNDOCUMENTED upstream: <https://code.claude.com/docs/en/settings> still
/// describes `~/.claude` as fixed. The feature has outrun its docs, so
/// [`claude_config_dir`] must always fall back to `~/.claude` — if a future
/// `claude` release drops the variable, snapback degrades to today's behavior
/// rather than breaking.
///
/// `pub(crate)` (rather than private) so [`crate::claude_cmd::claude_command`]
/// can stamp the SAME literal onto a spawned child's env — the read side
/// (this module) and the write side (`claude_cmd`) share one name, so they can
/// never drift apart on what the variable is called. This does not weaken the
/// "one env reader" invariant above: `claude_cmd` never calls `std::env::var`
/// on it, it only writes `Command::env` with the name this module owns.
pub(crate) const CLAUDE_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";

/// Env var that overrides the STORE VIEW only — snapback's OWN invention (zero
/// hits for this name in the installed `claude` binary's strings). It is a
/// FIXTURES/DEMO override: it does NOT change the profile any spawned `claude`
/// child uses, so setting it alone can point the board at a directory the child
/// cannot see. Kept as the highest-precedence override for the store root
/// because the board tests in `src/tui/update.rs`, the release workflow's
/// `sb`/`snapback` parity check, and the demo-GIF recording procedure all depend
/// on pointing the board at an exact fixtures directory regardless of the
/// ambient profile.
const CLAUDE_PROJECTS_DIR_ENV: &str = "CLAUDE_PROJECTS_DIR";

/// The default Claude profile directory name, matching the installed `claude`
/// binary's own default (`~/.claude`). The sibling `projects` subdirectory name
/// stays a literal in `store::discover` for now (the module that joins it) — one
/// literal, one home, per NO MAGIC VALUES; do not duplicate it here.
const CLAUDE_DIR_NAME: &str = ".claude";

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

/// Read `$CLAUDE_CONFIG_DIR`, treating an empty value as unset — matching
/// [`config_dir`]'s existing rule that an empty override is not a real one.
/// `None` means "no override": [`claude_config_dir`] then falls back to the
/// default profile.
///
/// This, not [`claude_config_dir`], is what every spawned `claude` child is
/// handed (`claude_cmd::claude_command`): a child is stamped only with an
/// override the user set, never with the resolved default, which `claude`
/// treats as a different profile from an unset variable (see `claude_cmd`'s
/// module doc).
pub fn claude_profile_override() -> Option<PathBuf> {
    let dir = std::env::var(CLAUDE_CONFIG_DIR_ENV).ok()?;
    if dir.is_empty() {
        return None;
    }
    Some(PathBuf::from(dir))
}

/// Resolve the Claude profile directory snapback operates against: the store
/// root (`store::discover::store_root`) derives from this path, and the
/// user-level `agents/*.md` list (`defined_agents::user_agents_dir`) from
/// [`claude_config_dir_if_known`], the same resolution without the home-less
/// guess. A spawned `claude` child is NOT handed this path but
/// [`claude_profile_override`]: with no override it resolves `claude`'s own
/// default, the same `~/.claude`.
///
/// [`claude_profile_override`] (i.e. `$CLAUDE_CONFIG_DIR`, set and non-empty) if
/// present, else `~/.claude`. Home-less fallback: a RELATIVE `.claude` rather
/// than a panic — the same three-step fail-soft shape as [`config_dir`] and
/// `store::discover::store_root`'s existing fallback. A missing home must never
/// abort the board.
///
/// That RELATIVE home-less fallback never reaches a spawned child: a child
/// spawned after a `chdir` (`resume::launch`) or with `.current_dir(cwd)`
/// (`send::run_child`) would resolve it against the SESSION's directory —
/// naming a different, wrong profile per session, and possibly inviting
/// `claude` to create one inside a user's repo. Children are handed only the
/// override, and `claude_cmd`'s `profile_env_value` refuses to stamp even a
/// user-set relative one.
pub fn claude_config_dir() -> PathBuf {
    // Last resort if the home dir cannot be resolved: a relative dir rather than
    // a panic, matching `config_dir`'s and `store_root`'s home-less fallbacks.
    claude_config_dir_if_known().unwrap_or_else(|| PathBuf::from(CLAUDE_DIR_NAME))
}

/// [`claude_config_dir`] WITHOUT its home-less guess: `None` when there is
/// neither an override nor a home directory.
///
/// For a reader that must not guess: a relative `.claude` — resolved against the
/// launch dir — names that PROJECT's own `.claude`, so `claude_settings` would
/// re-read the project's `settings.json` as the user's, and `defined_agents`
/// would list the project's agents a second time as user-level ones. A relative
/// value the user SETS (`CLAUDE_CONFIG_DIR=.claude-work`) is still returned as
/// given: refusing relative paths is the spawn seam's rule (`claude_cmd`), not a
/// reader's.
pub fn claude_config_dir_if_known() -> Option<PathBuf> {
    claude_config_dir_from(claude_profile_override(), dirs::home_dir())
}

/// The pure decision behind [`claude_config_dir_if_known`] and
/// [`claude_config_dir`]: `override_dir` if present, else `<home>/.claude`, else
/// `None`. Takes both inputs as values so the order is asserted with no env
/// mutation.
fn claude_config_dir_from(override_dir: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    override_dir.or_else(|| home.map(|home| home.join(CLAUDE_DIR_NAME)))
}

/// Read `$CLAUDE_PROJECTS_DIR`, treating an empty value as unset. `None` means
/// "no override": `store::discover::store_root_from` then resolves the store
/// root from [`claude_config_dir`] instead. Kept as a `String` rather than a
/// `PathBuf` since the only consumer (`store_root_from`) compares it against
/// `<claude_config_dir>/projects` before ever allocating a path.
pub fn claude_projects_override() -> Option<String> {
    let dir = std::env::var(CLAUDE_PROJECTS_DIR_ENV).ok()?;
    if dir.is_empty() {
        return None;
    }
    Some(dir)
}

/// The ONE crate-wide lock serializing EVERY test that mutates a process-global
/// env var (`SNAPBACK_CONFIG_DIR`, `CLAUDE_CONFIG_DIR`, `CLAUDE_PROJECTS_DIR`,
/// …). It lives HERE — the module that OWNS env resolution for snapback's own
/// paths AND the Claude profile — because env vars are process-global: a test in
/// ANY module holding a per-module lock does not exclude a test in another, so
/// they race on `set_var`/`remove_var` under a parallel `cargo test`. Every
/// env-mutating test in every module (`config`, `tui::app`, `tui::update`)
/// reaches it via [`env_lock`] so there is exactly one lock, one accessor, one
/// reason.
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

    // --- claude_config_dir / claude_profile_override -----------------------

    // Same convention as above: the env var name and the `.claude` segment are
    // the USER-FACING contract, so these tests set/assert the LITERAL strings —
    // never via `CLAUDE_CONFIG_DIR_ENV` / `CLAUDE_DIR_NAME`. Each test also
    // SAVES whatever value was already present (`std::env::var_os`) and RESTORES
    // it at the end — mirroring `src/tui/update.rs`'s
    // `ctrl_n_with_no_defined_agents_opens_the_draft_pane_with_no_agent` test —
    // so a developer who has `CLAUDE_CONFIG_DIR` exported in their own shell
    // does not have it wiped for the rest of the test process.

    #[test]
    fn claude_config_dir_prefers_the_env_override_when_set() {
        let _guard = env_lock();
        let previous = std::env::var_os("CLAUDE_CONFIG_DIR");
        let dir = unique_temp_dir("claude-profile-override");

        std::env::set_var("CLAUDE_CONFIG_DIR", &dir);
        let resolved = claude_config_dir();

        match previous {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            resolved, dir,
            "a set, non-empty CLAUDE_CONFIG_DIR must win over the default profile"
        );
    }

    #[test]
    fn claude_config_dir_treats_an_empty_override_as_unset() {
        let _guard = env_lock();
        let previous = std::env::var_os("CLAUDE_CONFIG_DIR");

        std::env::set_var("CLAUDE_CONFIG_DIR", "");
        let empty = claude_config_dir();
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        let unset = claude_config_dir();

        match previous {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }

        assert_eq!(
            empty, unset,
            "an empty CLAUDE_CONFIG_DIR must be treated as unset, matching config_dir"
        );
    }

    #[test]
    fn claude_config_dir_defaults_to_dot_claude_under_home() {
        let _guard = env_lock();
        let previous = std::env::var_os("CLAUDE_CONFIG_DIR");

        std::env::remove_var("CLAUDE_CONFIG_DIR");
        let default = claude_config_dir();

        match previous {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }

        assert_eq!(
            default.file_name().and_then(|n| n.to_str()),
            Some(".claude"),
            "the default profile dir's final component must be `.claude`, matching the installed claude binary's own default"
        );
        if let Some(home) = dirs::home_dir() {
            assert_eq!(default, home.join(".claude"));
        }
    }

    /// The pure order behind both profile resolvers, stated as values — no env
    /// mutation, so it needs no [`env_lock`]. The `(None, None)` case is the one
    /// `claude_settings` and `defined_agents` depend on: a guessed relative
    /// `.claude` there would read the launch project's own `.claude` as the user's.
    #[test]
    fn claude_config_dir_from_is_override_then_home_then_nothing() {
        let home = PathBuf::from("/home/me");
        let absolute = PathBuf::from("/profiles/work");
        let relative = PathBuf::from(".claude-work");

        assert_eq!(
            claude_config_dir_from(Some(absolute.clone()), Some(home.clone())),
            Some(absolute),
            "a set override wins over the home default"
        );
        assert_eq!(
            claude_config_dir_from(Some(relative.clone()), None),
            Some(relative),
            "a relative override is returned as given — no absolute-path filter here"
        );
        assert_eq!(
            claude_config_dir_from(None, Some(home)),
            Some(PathBuf::from("/home/me/.claude")),
            "with no override, the profile is the literal `.claude` segment under home"
        );
        assert_eq!(
            claude_config_dir_from(None, None),
            None,
            "no override and no home name no profile — never a guessed relative `.claude`"
        );
    }

    #[test]
    fn claude_profile_override_is_none_when_unset_or_empty() {
        let _guard = env_lock();
        let previous = std::env::var_os("CLAUDE_CONFIG_DIR");

        std::env::remove_var("CLAUDE_CONFIG_DIR");
        let unset = claude_profile_override();
        std::env::set_var("CLAUDE_CONFIG_DIR", "");
        let empty = claude_profile_override();

        match previous {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }

        assert_eq!(unset, None, "an unset CLAUDE_CONFIG_DIR must be None");
        assert_eq!(empty, None, "an empty CLAUDE_CONFIG_DIR must also be None");
    }

    // --- claude_projects_override -------------------------------------------

    // `claude_projects_override` mirrors `claude_profile_override`'s shape (a
    // set, non-empty value wins; empty is unset) for the pre-existing
    // `CLAUDE_PROJECTS_DIR` fixtures/demo override — same LITERAL-name and
    // save/restore convention as above.

    #[test]
    fn claude_projects_override_is_some_only_when_set_and_non_empty() {
        let _guard = env_lock();
        let previous = std::env::var_os("CLAUDE_PROJECTS_DIR");

        std::env::remove_var("CLAUDE_PROJECTS_DIR");
        let unset = claude_projects_override();
        std::env::set_var("CLAUDE_PROJECTS_DIR", "");
        let empty = claude_projects_override();
        std::env::set_var("CLAUDE_PROJECTS_DIR", "/tmp/fixtures");
        let set = claude_projects_override();

        match previous {
            Some(v) => std::env::set_var("CLAUDE_PROJECTS_DIR", v),
            None => std::env::remove_var("CLAUDE_PROJECTS_DIR"),
        }

        assert_eq!(unset, None, "an unset CLAUDE_PROJECTS_DIR must be None");
        assert_eq!(empty, None, "an empty CLAUDE_PROJECTS_DIR must be None");
        assert_eq!(
            set,
            Some("/tmp/fixtures".to_string()),
            "a set, non-empty CLAUDE_PROJECTS_DIR must be returned verbatim"
        );
    }
}
