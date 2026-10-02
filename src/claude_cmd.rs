//! The ONE seam that constructs a `claude` child process.
//!
//! Every argv this crate hands to `claude` is built PURE — no process, no
//! environment, no side effect — by one of:
//!
//! * [`crate::resume::build_argv`], [`crate::resume::build_new_argv`],
//!   [`crate::resume::build_attach_argv`]
//! * [`crate::send::build_send_argv`], [`crate::send::build_stop_argv`],
//!   [`crate::send::build_bg_launch_argv`]
//! * [`crate::agents::agents_argv`], [`crate::agents::live_agents_argv`]
//!
//! Turning one of those argvs into an actual [`Command`] is this module's
//! entire job, and [`claude_command`] is the only place that does it. The
//! profile is a value this seam is GIVEN — the override
//! [`crate::config::claude_profile_override`] read, never an environment read
//! of its own — and it is stamped onto the child via [`Command::env`] ONLY when
//! the user actually set one. With no override the child's environment is left
//! untouched, so `claude` resolves its own default profile: the same `~/.claude`
//! the store falls back to (`store::discover::store_root`).
//!
//! Spelling that default out is NOT the same as leaving it unset: `claude`
//! 2.1.284 picks its login (keychain entry) and its global config file by
//! whether the variable is SET, so an explicitly stamped `~/.claude` launched
//! every child logged out. The dated evidence is
//! `docs/agents/CLAUDE_CLI.md` (the `CLAUDE_CONFIG_DIR` section). A stamp, when
//! there is one, restates the value the child would inherit anyway; what the
//! seam guarantees is that no child is ever handed a profile `config` did not
//! read from the user's environment.
//!
//! Store and child naming one profile is what keeps `agents::live_agents` —
//! the hard-delete WRITER guard's sole authority — honest: a probe run under a
//! different profile than the store's reads every row as "no writer" and
//! silently permits an irreversible delete.
//!
//! Every `*_argv` builder above stays PURE; env rides ONLY on this thin
//! impure wrapper, so the argv contract stays directly assertable without
//! spawning anything, exactly as it was before this module existed.
//!
//! **The OS opener is explicitly NOT this module's business.**
//! `resume::open_url` spawns the platform's default-app launcher
//! (`open`/`xdg-open`/`cmd /C start`) to open a link in a browser — that is
//! not `claude`, so it must never carry the profile override. It builds its
//! own plain `Command` via `resume::opener_command` and must NEVER be routed
//! through [`claude_command`]; `resume`'s own test pins that its result
//! carries no `CLAUDE_CONFIG_DIR` override.

use std::path::Path;
use std::process::Command;

use crate::config::CLAUDE_CONFIG_DIR_ENV;

/// Decide the `CLAUDE_CONFIG_DIR` value to stamp onto a spawned `claude`
/// child, or `None` to leave the child's environment untouched.
///
/// `profile_override` is the override the user set, as
/// [`crate::config::claude_profile_override`] read it (`None` when the
/// variable is unset or empty). `Some` only for an ABSOLUTE override:
///
/// - No override ⇒ `None`, never the default profile spelled out — to
///   `claude`, a stamped default is a different profile from an unset one (see
///   the module doc).
/// - A RELATIVE override ⇒ `None`. `resume::launch` `chdir`s into the
///   session's `cwd` before spawning and `send::run_child` uses
///   `.current_dir(cwd)`, so a relative path names a different directory per
///   session; snapback does not itself assert a profile it cannot name
///   unambiguously. The child still inherits the user's own value unchanged.
#[must_use]
fn profile_env_value(profile_override: Option<&Path>) -> Option<&Path> {
    profile_override.filter(|dir| dir.is_absolute())
}

/// Build the [`Command`] for a `claude` child from a pure argv and the
/// profile override the user set, if any.
///
/// `argv[0]` becomes the program, `argv[1..]` the arguments — verbatim, no
/// reinterpretation. `CLAUDE_CONFIG_DIR` is set via [`Command::env`] only when
/// [`profile_env_value`] returns `Some` (an absolute override); otherwise NO
/// env var is touched at all and the child inherits snapback's environment
/// as is.
///
/// Sets NO other env var and touches NO other [`Command`] setting —
/// `current_dir`, stdio, and any `chdir` stay with each caller, which already
/// owns them (a resume `chdir`s first; a send/bg-launch sets
/// `.current_dir`; the agents probe wants neither). Keeping this wrapper to
/// exactly one job is what keeps it assertable without spawning: every test
/// below inspects the built [`Command`] via `get_program`/`get_args`/
/// `get_envs`, never `.spawn()`.
#[must_use]
pub(crate) fn claude_command(argv: &[String], profile_override: Option<&Path>) -> Command {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    if let Some(dir) = profile_env_value(profile_override) {
        cmd.env(CLAUDE_CONFIG_DIR_ENV, dir);
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::ffi::OsStr;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_string()).collect()
    }

    /// (a) The program and args match the argv exactly — no reinterpretation,
    /// no dropped/added element.
    #[test]
    fn claude_command_carries_the_program_and_args_verbatim() {
        let cmd = claude_command(
            &argv(&["claude", "-r", "abc-123"]),
            Some(Path::new("/tmp/profile")),
        );
        assert_eq!(cmd.get_program(), OsStr::new("claude"));
        let args: Vec<&OsStr> = cmd.get_args().collect();
        assert_eq!(args, vec![OsStr::new("-r"), OsStr::new("abc-123")]);
    }

    /// (b) With NO override the child's environment is left untouched — not
    /// even the default profile spelled out. A stamped `~/.claude` launched
    /// every child logged out under `claude` 2.1.284 (see the module doc), so
    /// this asserts the whole env diff is empty rather than only that one
    /// variable is absent.
    #[test]
    fn claude_command_leaves_the_env_untouched_with_no_override() {
        let cmd = claude_command(&argv(&["claude", "-r", "abc-123"]), None);
        let envs: Vec<(&OsStr, Option<&OsStr>)> = cmd.get_envs().collect();
        assert!(
            envs.is_empty(),
            "with no CLAUDE_CONFIG_DIR override the child must inherit snapback's env as is: {envs:?}"
        );
    }

    /// (c) An ABSOLUTE override stamps `CLAUDE_CONFIG_DIR` explicitly, and the
    /// assertion is against the LITERAL variable name — never the const under
    /// test — so a rename of `CLAUDE_CONFIG_DIR_ENV` cannot silently desync
    /// from the string `claude` itself honors.
    #[test]
    fn claude_command_stamps_claude_config_dir_for_an_absolute_override() {
        let cmd = claude_command(
            &argv(&["claude", "agents", "--json"]),
            Some(Path::new("/Users/me/.claude-work")),
        );
        let envs: Vec<(&OsStr, Option<&OsStr>)> = cmd.get_envs().collect();
        assert!(
            envs.iter().any(|(k, v)| {
                *k == OsStr::new("CLAUDE_CONFIG_DIR")
                    && *v == Some(OsStr::new("/Users/me/.claude-work"))
            }),
            "expected CLAUDE_CONFIG_DIR=/Users/me/.claude-work among {envs:?}"
        );
    }

    /// (d) A RELATIVE override yields NO `CLAUDE_CONFIG_DIR` stamp — snapback
    /// never itself asserts a profile that would name a different directory
    /// per `chdir`ed child.
    #[test]
    fn claude_command_does_not_stamp_a_relative_override() {
        let cmd = claude_command(
            &argv(&["claude", "agents", "--json"]),
            Some(Path::new(".claude-work")),
        );
        let envs: Vec<(&OsStr, Option<&OsStr>)> = cmd.get_envs().collect();
        assert!(
            !envs
                .iter()
                .any(|(k, _)| *k == OsStr::new("CLAUDE_CONFIG_DIR")),
            "a RELATIVE override must never be stamped onto a spawned child: {envs:?}"
        );
    }

    /// (e) No env var OTHER than `CLAUDE_CONFIG_DIR` is ever added.
    #[test]
    fn claude_command_adds_no_env_var_other_than_claude_config_dir() {
        let cmd = claude_command(
            &argv(&["claude", "-r", "abc-123"]),
            Some(Path::new("/tmp/profile")),
        );
        let envs: Vec<(&OsStr, Option<&OsStr>)> = cmd.get_envs().collect();
        assert_eq!(
            envs.len(),
            1,
            "claude_command must set exactly one env override: {envs:?}"
        );
        assert_eq!(envs[0].0, OsStr::new("CLAUDE_CONFIG_DIR"));
    }

    /// `profile_env_value` itself — the pure decision the stamp rests on: no
    /// override and a relative one stamp nothing, an absolute one is stamped
    /// as given.
    #[test]
    fn profile_env_value_stamps_only_an_absolute_override() {
        assert_eq!(profile_env_value(None), None);
        assert_eq!(profile_env_value(Some(Path::new(".claude"))), None);
        assert_eq!(profile_env_value(Some(Path::new("relative/.claude"))), None);
        let abs = Path::new("/Users/me/.claude-work");
        assert_eq!(profile_env_value(Some(abs)), Some(abs));
    }
}
