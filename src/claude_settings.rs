//! What `claude` decides about the model when snapback sends no `--model`, read
//! from the same settings files and environment variables `claude` reads.
//!
//! # Why this exists
//!
//! Each compose box names the model its launch will run on — `model: …` on the
//! quick-reply box (`Ctrl-R`) and on the new-session draft (`Ctrl-N`) — and with no
//! pick made there (`Ctrl-L`), snapback sends no `--model`, so the answer is
//! `claude`'s own. It is NOT one model for every launch, and this module answers
//! the two questions the two boxes need:
//!
//! * **A NEW session** runs on the default the user's `claude` settings name
//!   ([`resolve_new_session_model`]); the draft box shows it as
//!   `default (<value>) (new sessions only)`.
//! * **A `-r` launch** (the quick reply) normally restores the model the session
//!   last answered with — which the preview already parsed — UNLESS an environment
//!   override makes `claude` use its startup model instead
//!   ([`resolve_restore_overridden`]); the reply box then says `default` rather than
//!   naming the session's model. See `docs/agents/CLAUDE_CLI.md`, "Which model a
//!   launch runs on without `--model`".
//!
//! # How `claude` decides — mirrored, not invented
//!
//! Read out of the installed `claude` 2.1.282 binary rather than taken from the
//! docs (the capture is recorded in `docs/agents/CLAUDE_CLI.md`):
//!
//! * **Settings precedence.** `claude` merges its settings sources in the order
//!   user → project → local → (flag) → managed, each later source overriding the
//!   earlier one. snapback passes no `--settings`, so the flag source is empty and
//!   the order it mirrors, HIGHEST first, is: managed → `<launch dir>/.claude/
//!   settings.local.json` → `<launch dir>/.claude/settings.json` →
//!   `$CLAUDE_CONFIG_DIR/settings.json` (else `~/.claude/settings.json`). The
//!   launch dir is the right project because a New session always starts there
//!   (`resume::check_new`).
//! * **Managed settings** are `managed-settings.json` in a per-platform directory
//!   ([`MANAGED_SETTINGS_DIR`]), overridden by every `*.json` file in its
//!   `managed-settings.d/` drop-in directory, applied in file-name order.
//! * **`ANTHROPIC_MODEL` beats every file's `model`.** At startup `claude` copies
//!   each settings file's `env` block onto its own process environment, lowest
//!   layer first — so the highest layer that sets `env.ANTHROPIC_MODEL` wins, and
//!   ANY layer's value beats the one snapback's (inherited) environment carries.
//!   It then takes `ANTHROPIC_MODEL || <merged settings>.model`.
//! * **A `-r` launch skips restoring the session's model** when `ANTHROPIC_MODEL`
//!   or any `ANTHROPIC_DEFAULT_{FABLE,OPUS,SONNET,HAIKU}_MODEL` is set — each read
//!   the same way, the highest settings layer's `env` over the inherited
//!   environment.
//!
//! [`resolve_new_session_model`] and [`resolve_restore_overridden`] state those
//! decisions as pure functions over already-read inputs; [`model_defaults`] is the
//! thin impure driver that reads the environment and the files ONCE for both.
//!
//! # What it deliberately does not mirror
//!
//! The managed tier's MDM (plist/registry) and server-delivered layers, the
//! global config's (`~/.claude.json`) `env` block, and the git-root relocation
//! of `settings.local.json` that `claude` performs when the launch dir is not its
//! repository's canonical root. Each needs something this module will not do — a
//! network fetch, a platform preference store, a multi-megabyte config file or a
//! `git` walk — for a label that is cosmetic.
//!
//! It also does not detect the other ways `claude` skips or declines a restore, so
//! each leaves [`ModelDefaults::restore_overridden`] `false` and the reply naming
//! the session's model:
//!
//! * a merged settings `model` of `opusplan` or `haiku` when the session's model is
//!   compatible with it — `claude` keeps the alias; that check needs the
//!   transcript's model, which this module never sees;
//! * a session bound to a defined agent whose frontmatter names its own `model:`.
//!   snapback reads no agent's `model:` (`crate::defined_agents` keeps only the name
//!   and description), and that field also outranks `ANTHROPIC_MODEL` and every
//!   settings `model` for a `Ctrl-N` draft, so [`ModelDefaults::new_session`]
//!   misnames an agent draft whose agent sets one;
//! * a non-first-party PROVIDER, whose selecting variables
//!   `docs/agents/CLAUDE_CLI.md` does not record;
//! * a model `claude` rejects when it resumes (retired, of an unknown family, or
//!   not allowed for the account), which only `claude` can know at that moment.
//!
//! And it reads the LAUNCH dir's project settings, where a reply runs in the
//! session's own `cwd` — an `env` override set in only one of those two projects
//! is misread either way.
//!
//! Every miss above only mislabels a compose: the label is display-only, and with
//! no pick snapback sends no `--model`, so it never changes what `claude` runs.
//! `docs/agents/DOMAIN.md`, "Known limits — the label is display-only", lists the
//! cases the label is known to get wrong.
//!
//! # Fail-soft
//!
//! Every settings file is parsed as a `serde_json::Value` (AGENTS.md: FAIL-SOFT
//! parsing), never a typed struct. A missing file, an unreadable one, garbage
//! JSON, a non-object top level or a wrong-typed field means "no value from this
//! file" — never a panic and never an error the caller has to handle. The
//! new-session answer collapses to `None`, which the draft box reads as "nothing to
//! name", so its failure mode is `model: default` — true, merely less specific. The
//! restore answer collapses to `false` (no override seen), so the reply box keeps
//! naming the session's model — which is what `claude` does in every case this
//! module can observe.
//!
//! Like [`crate::model_aliases`], this is a plain blocking function that knows
//! nothing about threads or events; a caller puts it on its own thread (AGENTS.md:
//! OFF-UI-THREAD) and it must never run on a keystroke or the render path.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::defined_agents::CLAUDE_DIR;

/// The environment variable `claude` reads for a model that overrides every
/// settings file's `model` — and the key a settings file's `env` block sets it
/// under, which is why ONE name serves both lookups.
const ANTHROPIC_MODEL_ENV: &str = "ANTHROPIC_MODEL";

/// Every environment variable whose presence makes `claude` SKIP restoring a
/// session's own model on a `-r` launch and use its startup model instead: the
/// model override itself plus the four per-family default overrides, as the
/// `claude 2.1.282` bundle checks them (`docs/agents/CLAUDE_CLI.md`, "Which model a
/// launch runs on without `--model`"). One list, read by
/// [`resolve_restore_overridden`] alone. (NO MAGIC VALUES: the names are spelled
/// here and nowhere else.)
const RESTORE_OVERRIDE_ENV: [&str; 5] = [
    ANTHROPIC_MODEL_ENV,
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
];

/// What `claude`'s settings and environment decide about the model when snapback
/// sends no `--model` — the one answer [`model_defaults`] reads per board session
/// and the board carries as
/// [`AppEvent::SettingsModel`](crate::watch::AppEvent::SettingsModel).
///
/// Two facts from ONE read of the same files, because both are that read's
/// answers: the settings layers and their `env` blocks are loaded once and asked
/// both questions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelDefaults {
    /// The model a NEW session in the launch dir runs on
    /// ([`resolve_new_session_model`]), or `None` when the settings and
    /// `ANTHROPIC_MODEL` name none.
    pub new_session: Option<String>,
    /// Whether an environment override is in effect that makes `claude` SKIP
    /// restoring a session's own model on `-r` ([`resolve_restore_overridden`]),
    /// so a reply with no pick runs on the startup model rather than the one the
    /// session last answered with.
    pub restore_overridden: bool,
}

/// The environment variable that relocates `claude`'s user config directory
/// (default `~/.claude`). Set and non-empty wins; an empty value is treated as
/// unset, matching snapback's own `$CLAUDE_PROJECTS_DIR` / `$SNAPBACK_CONFIG_DIR`
/// convention rather than resolving settings relative to whatever directory the
/// board happens to run in.
const CLAUDE_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";

/// The settings file name in the user config dir and in a project's `.claude/`.
const SETTINGS_FILE: &str = "settings.json";

/// The project-local, git-ignored settings file in a project's `.claude/`. It
/// outranks the shared [`SETTINGS_FILE`] beside it.
const LOCAL_SETTINGS_FILE: &str = "settings.local.json";

/// The managed-settings file inside [`MANAGED_SETTINGS_DIR`].
const MANAGED_SETTINGS_FILE: &str = "managed-settings.json";

/// The managed drop-in directory inside [`MANAGED_SETTINGS_DIR`]; every file in
/// it that [`drop_in_order`] keeps overrides [`MANAGED_SETTINGS_FILE`].
const MANAGED_DROP_IN_DIR: &str = "managed-settings.d";

/// The name suffix a managed drop-in must carry to be read.
const DROP_IN_SUFFIX: &str = ".json";

/// The leading character that hides a drop-in from `claude` (a dotfile is never
/// read, whatever its suffix).
const HIDDEN_NAME_PREFIX: char = '.';

/// Where `claude` looks for managed (enterprise policy) settings on macOS.
#[cfg(target_os = "macos")]
const MANAGED_SETTINGS_DIR: &str = "/Library/Application Support/ClaudeCode";

/// Where `claude` looks for managed (enterprise policy) settings on Windows.
#[cfg(windows)]
const MANAGED_SETTINGS_DIR: &str = r"C:\Program Files\ClaudeCode";

/// Where `claude` looks for managed (enterprise policy) settings everywhere else
/// (Linux and the other unixes).
#[cfg(not(any(target_os = "macos", windows)))]
const MANAGED_SETTINGS_DIR: &str = "/etc/claude-code";

/// The settings key naming the default model.
const MODEL_KEY: &str = "model";

/// The settings key holding the environment block `claude` applies at startup.
const ENV_KEY: &str = "env";

/// The model value that means "no particular model": `claude` resolves it to its
/// own built-in default, so it names nothing the header could show.
const DEFAULT_MODEL_VALUE: &str = "default";

/// A UTF-8 byte-order mark. `claude` strips one before parsing a settings file,
/// and `serde_json` would reject it, so it is stripped here too.
const BYTE_ORDER_MARK: char = '\u{feff}';

/// The model a NEW session gets from `claude`'s settings and environment, or
/// `None` when they name none.
///
/// `process_env_model` is `ANTHROPIC_MODEL` as snapback's own process sees it
/// (which a spawned `claude` inherits). `layers` are the raw contents of the
/// settings files that could be read, **highest precedence first** (see
/// [`settings_layer_paths`]); a file that could not be read is simply absent.
///
/// The decision, mirroring `claude` 2.1.282:
///
/// 1. `ANTHROPIC_MODEL` is the highest layer's `env.ANTHROPIC_MODEL`, or the
///    process environment's when no layer sets it — a settings file's `env` is
///    copied OVER the inherited environment.
/// 2. A non-empty `ANTHROPIC_MODEL` decides alone. An EMPTY one does not: `claude`
///    reads `ANTHROPIC_MODEL || model`, and an empty string falls through.
/// 3. Otherwise the highest layer that carries a string `model` decides — even a
///    blank or `default` one, because `claude`'s merge assigns that value over the
///    lower layers' rather than skipping it.
/// 4. The deciding value names nothing when it is blank or `default` (trimmed,
///    ASCII case-insensitive); anything else is returned trimmed, spelled exactly
///    as the settings spell it (`opus[1m]`, not a friendly name).
///
/// FAIL-SOFT: a layer that is not a JSON object contributes nothing, and a field
/// of the wrong type (a numeric `model`, a non-object `env`, a numeric
/// `ANTHROPIC_MODEL`) is "no value from this file", so a lower layer may still
/// answer.
#[must_use]
pub fn resolve_new_session_model(
    process_env_model: Option<&str>,
    layers: &[String],
) -> Option<String> {
    let settings = parse_layers(layers);

    let env_model = settings_env(&settings, ANTHROPIC_MODEL_ENV).or(process_env_model);
    if let Some(value) = env_model.filter(|value| !value.is_empty()) {
        return named_model(value);
    }

    settings
        .iter()
        .find_map(|layer| layer.get(MODEL_KEY).and_then(Value::as_str))
        .and_then(named_model)
}

/// Whether `claude` would SKIP restoring a session's own model on a `-r` launch
/// because an environment override is in effect — `ANTHROPIC_MODEL` or any
/// `ANTHROPIC_DEFAULT_{FABLE,OPUS,SONNET,HAIKU}_MODEL` ([`RESTORE_OVERRIDE_ENV`]).
///
/// `process_env` answers a variable name as snapback's own process sees it (which
/// a spawned `claude` inherits); `layers` are the raw settings file contents,
/// **highest precedence first**, exactly as [`resolve_new_session_model`] takes
/// them. Each variable is read the way `claude` reads `ANTHROPIC_MODEL`: the highest
/// layer's `env.<NAME>` string, else the process environment's — a settings file's
/// `env` is copied OVER the inherited one — and it counts only when NON-EMPTY, the
/// same rule the `ANTHROPIC_MODEL || model` check applies (an empty value is
/// JavaScript-falsy there). So an empty `env` entry in a settings file also MASKS a
/// set process variable, as the copy-over does.
///
/// FAIL-SOFT, toward `false`: a layer that is not a JSON object, a non-object
/// `env`, or a non-string value is "no value from this file". `false` leaves the
/// reply naming the session's model, which is what `claude` normally does when none
/// of these is set.
///
/// It CANNOT see every case in which `claude` skips or declines the restore, and a
/// pure function over these inputs never will: a model `claude` rejects at resume
/// time (retired, of an unknown family, or not allowed for the account) is known
/// only to `claude` at that moment, and a non-first-party provider, an
/// `opusplan`/`haiku` settings model compatible with the session's, and a bound
/// agent's own `model:` are not detected here (see the module doc). Those cases
/// still read as the session's model.
#[must_use]
pub fn resolve_restore_overridden(
    process_env: impl Fn(&str) -> Option<String>,
    layers: &[String],
) -> bool {
    let settings = parse_layers(layers);
    RESTORE_OVERRIDE_ENV.iter().any(|name| {
        let value = settings_env(&settings, name)
            .map(ToOwned::to_owned)
            .or_else(|| process_env(name));
        value.is_some_and(|value| !value.is_empty())
    })
}

/// The settings files' raw `layers` as JSON objects, in the order given; a layer
/// that does not parse, or is not an object, is dropped ("no value from this
/// file"). A leading byte-order mark is stripped first, as `claude` strips it.
fn parse_layers(layers: &[String]) -> Vec<Value> {
    layers
        .iter()
        .filter_map(|text| {
            serde_json::from_str::<Value>(text.trim_start_matches(BYTE_ORDER_MARK)).ok()
        })
        .filter(Value::is_object)
        .collect()
}

/// The highest layer's `env.<name>` STRING, or `None` when no layer sets one — the
/// value `claude`'s env copy-over would leave on its process for `name`, before the
/// inherited environment is consulted.
fn settings_env<'a>(settings: &'a [Value], name: &str) -> Option<&'a str> {
    settings.iter().find_map(|layer| {
        layer
            .get(ENV_KEY)
            .and_then(|env| env.get(name))
            .and_then(Value::as_str)
    })
}

/// `value` trimmed, or `None` when it names no model — blank, or the
/// [`DEFAULT_MODEL_VALUE`] word `claude` resolves to its own built-in default.
fn named_model(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && !value.eq_ignore_ascii_case(DEFAULT_MODEL_VALUE))
        .then(|| value.to_string())
}

/// The settings files `claude` would read for a new session in `launch_dir`,
/// **highest precedence first** — the order [`resolve_new_session_model`] takes
/// its `layers` in.
///
/// `drop_ins` is the managed drop-in listing in the order `claude` APPLIES it
/// (file-name order, see [`managed_drop_ins`]); the later one wins, so they are
/// emitted reversed. `user_config_dir` is `None` when no home directory can be
/// resolved, in which case the user layer is simply absent.
///
/// Pure, so the precedence — the one thing here most likely to be got backwards —
/// is asserted as a list of paths rather than only through file contents.
#[must_use]
fn settings_layer_paths(
    managed_dir: &Path,
    drop_ins: &[PathBuf],
    launch_dir: &Path,
    user_config_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let project_dir = launch_dir.join(CLAUDE_DIR);
    let mut paths: Vec<PathBuf> = drop_ins.iter().rev().cloned().collect();
    paths.push(managed_dir.join(MANAGED_SETTINGS_FILE));
    paths.push(project_dir.join(LOCAL_SETTINGS_FILE));
    paths.push(project_dir.join(SETTINGS_FILE));
    if let Some(dir) = user_config_dir {
        paths.push(dir.join(SETTINGS_FILE));
    }
    paths
}

/// Which of a managed drop-in directory's file `names` `claude` reads, in the
/// order it APPLIES them: a `.json` suffix and not a dotfile, ascending by name
/// (the later one wins).
///
/// Pure, and split from the directory listing on purpose: a filesystem hands
/// `read_dir` entries back in whatever order it keeps them — often already sorted
/// — so only a stated, unsorted input can prove the sort is here at all.
fn drop_in_order(mut names: Vec<String>) -> Vec<String> {
    names.retain(|name| name.ends_with(DROP_IN_SUFFIX) && !name.starts_with(HIDDEN_NAME_PREFIX));
    names.sort();
    names
}

/// The managed drop-in files under `managed_dir`, in the order `claude` applies
/// them ([`drop_in_order`]), FAIL-SOFT: a missing or unreadable directory is an
/// empty list, and a sub-directory or a non-UTF-8 name is skipped.
fn managed_drop_ins(managed_dir: &Path) -> Vec<PathBuf> {
    let dir = managed_dir.join(MANAGED_DROP_IN_DIR);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let names: Vec<String> = entries
        .flatten()
        .filter(|entry| {
            entry
                .file_type()
                .is_ok_and(|kind| kind.is_file() || kind.is_symlink())
        })
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    drop_in_order(names)
        .into_iter()
        .map(|name| dir.join(name))
        .collect()
}

/// The contents of every path in `paths` that can be read as text, in order,
/// FAIL-SOFT: a missing file, a directory, an unreadable or non-UTF-8 file is
/// skipped rather than reported — "no value from this file".
fn read_layers(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .collect()
}

/// `claude`'s user config directory: `$CLAUDE_CONFIG_DIR` when set and non-empty,
/// else `~/.claude`, or `None` when there is no home directory to resolve.
fn user_config_dir() -> Option<PathBuf> {
    match std::env::var_os(CLAUDE_CONFIG_DIR_ENV) {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => dirs::home_dir().map(|home| home.join(CLAUDE_DIR)),
    }
}

/// The settings-file half of [`model_defaults`], with every location — and the
/// process environment — a parameter so the whole read (paths, drop-in listing,
/// file reads, both decisions) runs against a temp dir and a stated environment in
/// the suite instead of the real machine. The layers are read ONCE and handed to
/// both decisions.
fn defaults_from_disk(
    process_env: impl Fn(&str) -> Option<String>,
    managed_dir: &Path,
    launch_dir: &Path,
    user_config_dir: Option<&Path>,
) -> ModelDefaults {
    let paths = settings_layer_paths(
        managed_dir,
        &managed_drop_ins(managed_dir),
        launch_dir,
        user_config_dir,
    );
    let layers = read_layers(&paths);
    let process_env_model = process_env(ANTHROPIC_MODEL_ENV);
    ModelDefaults {
        new_session: resolve_new_session_model(process_env_model.as_deref(), &layers),
        restore_overridden: resolve_restore_overridden(process_env, &layers),
    }
}

/// What the user's `claude` settings and environment decide about the model for a
/// launch from `launch_dir` that carries no `--model` — see [`ModelDefaults`].
///
/// The thin impure driver: it reads the process environment (`ANTHROPIC_MODEL`,
/// the `ANTHROPIC_DEFAULT_*_MODEL` overrides and `$CLAUDE_CONFIG_DIR`), lists and
/// reads the settings files, and delegates every decision to
/// [`resolve_new_session_model`] and [`resolve_restore_overridden`]. Blocking file
/// I/O — call it off the UI thread, and never on a keystroke or the render path.
#[must_use]
pub fn model_defaults(launch_dir: &Path) -> ModelDefaults {
    defaults_from_disk(
        |name| std::env::var(name).ok(),
        Path::new(MANAGED_SETTINGS_DIR),
        launch_dir,
        user_config_dir().as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A settings file whose only content is `{"model": <model>}`.
    fn model_layer(model: &str) -> String {
        serde_json::json!({ MODEL_KEY: model }).to_string()
    }

    /// A settings file whose only content is `{"env": {"ANTHROPIC_MODEL": <model>}}`.
    fn env_layer(model: &str) -> String {
        serde_json::json!({ ENV_KEY: { ANTHROPIC_MODEL_ENV: model } }).to_string()
    }

    /// A unique temp dir for one test, so parallel tests never share files.
    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!(
            "snapback-claude-settings-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create the test's temp dir");
        dir
    }

    /// Write `contents` to `path`, creating its parent directories.
    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().expect("a file path has a parent"))
            .expect("create the parent dir");
        std::fs::write(path, contents).expect("write the settings file");
    }

    /// The layer ORDER is the whole precedence claim, so it is pinned as paths:
    /// managed drop-ins (the last-applied first), the managed file, then the launch
    /// dir's local and shared project files, then the user file.
    #[test]
    fn the_layer_paths_run_managed_then_local_then_project_then_user() {
        let managed = Path::new("/managed");
        let drop_ins = [
            PathBuf::from("/managed/managed-settings.d/10-base.json"),
            PathBuf::from("/managed/managed-settings.d/20-team.json"),
        ];
        let paths = settings_layer_paths(
            managed,
            &drop_ins,
            Path::new("/work/repo"),
            Some(Path::new("/home/me/.claude")),
        );
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/managed/managed-settings.d/20-team.json"),
                PathBuf::from("/managed/managed-settings.d/10-base.json"),
                PathBuf::from("/managed/managed-settings.json"),
                PathBuf::from("/work/repo/.claude/settings.local.json"),
                PathBuf::from("/work/repo/.claude/settings.json"),
                PathBuf::from("/home/me/.claude/settings.json"),
            ],
            "highest precedence first: the later drop-in beats the earlier one and \
             every drop-in beats the managed file"
        );

        let homeless = settings_layer_paths(managed, &[], Path::new("/work/repo"), None);
        assert_eq!(
            homeless.last(),
            Some(&PathBuf::from("/work/repo/.claude/settings.json")),
            "with no home to resolve, the user layer is absent rather than guessed"
        );
    }

    /// The highest layer naming a model wins, and dropping it hands the answer to
    /// the next one down — so each rung of the order is observed deciding.
    #[test]
    fn the_highest_precedence_file_that_names_a_model_wins() {
        let layers = [
            model_layer("managed-pick"),
            model_layer("local-pick"),
            model_layer("project-pick"),
            model_layer("user-pick"),
        ];
        for (rung, expected) in ["managed-pick", "local-pick", "project-pick", "user-pick"]
            .iter()
            .enumerate()
        {
            assert_eq!(
                resolve_new_session_model(None, &layers[rung..]).as_deref(),
                Some(*expected),
                "with the {rung} higher layer(s) gone, {expected} must decide"
            );
        }
        assert_eq!(
            resolve_new_session_model(None, &[]),
            None,
            "no settings and no environment name no model"
        );

        // A layer with no `model` key at all does not mask the ones below it.
        let silent_top = vec![r#"{"theme":"dark"}"#.to_string(), model_layer("opus[1m]")];
        assert_eq!(
            resolve_new_session_model(None, &silent_top).as_deref(),
            Some("opus[1m]"),
            "a file that says nothing about the model leaves it to the next file, \
             and the value is spelled exactly as the settings spell it"
        );
    }

    /// `ANTHROPIC_MODEL` beats every file's `model`, from either source — and a
    /// settings file's `env` beats the inherited process environment, the highest
    /// such file winning.
    #[test]
    fn anthropic_model_beats_every_files_model_and_a_settings_env_beats_the_process() {
        let files = vec![model_layer("managed-pick"), model_layer("user-pick")];
        assert_eq!(
            resolve_new_session_model(Some("sonnet"), &files).as_deref(),
            Some("sonnet"),
            "the process environment's ANTHROPIC_MODEL beats even the managed model"
        );

        let with_env = vec![
            model_layer("managed-pick"),
            env_layer("local-env"),
            env_layer("user-env"),
        ];
        assert_eq!(
            resolve_new_session_model(Some("process-env"), &with_env).as_deref(),
            Some("local-env"),
            "a settings file's env is applied OVER the process environment, and the \
             higher file's value is applied last"
        );
        assert_eq!(
            resolve_new_session_model(None, &with_env[2..]).as_deref(),
            Some("user-env"),
            "even the lowest file's env beats every file's model"
        );
    }

    /// A blank or `default` value names no model. For `model` the value still
    /// DECIDES — it masks the files below it, as `claude`'s merge does — and for
    /// `ANTHROPIC_MODEL` only an EMPTY value falls through to `model`.
    #[test]
    fn blank_and_default_values_name_no_model() {
        let below = model_layer("opus");
        for masking in ["", "   ", "default", " Default "] {
            assert_eq!(
                resolve_new_session_model(None, &[model_layer(masking), below.clone()]),
                None,
                "a {masking:?} model names nothing and masks the file below it"
            );
            assert_eq!(
                resolve_new_session_model(Some(masking), &[]),
                None,
                "a {masking:?} ANTHROPIC_MODEL names nothing"
            );
        }

        assert_eq!(
            resolve_new_session_model(Some(""), std::slice::from_ref(&below)).as_deref(),
            Some("opus"),
            "an EMPTY ANTHROPIC_MODEL falls through to the settings model"
        );
        assert_eq!(
            resolve_new_session_model(None, &[env_layer(""), below.clone()]).as_deref(),
            Some("opus"),
            "and so does an empty env.ANTHROPIC_MODEL"
        );
        assert_eq!(
            resolve_new_session_model(Some("default"), &[below]),
            None,
            "a `default` ANTHROPIC_MODEL decides — the built-in default — and never \
             reaches the settings model"
        );
    }

    /// FAIL-SOFT: garbage JSON, a non-object top level and wrong-typed fields are
    /// "no value from this file", so a readable file below still answers — and
    /// none of it panics.
    #[test]
    fn garbage_json_and_wrong_typed_fields_fail_soft() {
        let answer = model_layer("haiku");
        let hostile = [
            "{ not json",
            "",
            "[\"model\", \"opus\"]",
            "\"opus\"",
            r#"{"model": 5}"#,
            r#"{"model": null}"#,
            r#"{"model": {"name": "opus"}}"#,
            r#"{"env": "ANTHROPIC_MODEL=opus"}"#,
            r#"{"env": {"ANTHROPIC_MODEL": 7}}"#,
            r#"{"env": ["ANTHROPIC_MODEL"]}"#,
        ];
        for text in hostile {
            assert_eq!(
                resolve_new_session_model(None, &[text.to_string(), answer.clone()]).as_deref(),
                Some("haiku"),
                "{text:?} must contribute nothing and leave the next file to answer"
            );
        }

        let with_bom = format!("{BYTE_ORDER_MARK}{}", model_layer("sonnet[1m]"));
        assert_eq!(
            resolve_new_session_model(None, &[with_bom]).as_deref(),
            Some("sonnet[1m]"),
            "a leading byte-order mark is stripped, as claude strips it"
        );
    }

    /// Only readable files become layers, in order: a missing file and a directory
    /// where a file was expected are skipped without an error.
    #[test]
    fn missing_and_unreadable_files_are_skipped() {
        let dir = temp_dir("read");
        let present = dir.join("present.json");
        let missing = dir.join("missing.json");
        let a_directory = dir.join("settings.json");
        let last = dir.join("last.json");
        write(&present, "first");
        std::fs::create_dir_all(&a_directory).expect("make a directory in a file's place");
        write(&last, "second");

        assert_eq!(
            read_layers(&[present, missing, a_directory, last]),
            vec!["first".to_string(), "second".to_string()],
            "unreadable paths drop out and the rest keep their order"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// [`drop_in_order`] keeps `.json` names that are not dotfiles and SORTS them —
    /// stated over an unsorted input, since a real directory listing often comes
    /// back sorted already and would let a missing sort pass.
    #[test]
    fn drop_ins_apply_in_name_order_and_skip_dotfiles_and_other_suffixes() {
        let listed = ["20-team.json", "notes.txt", ".hidden.json", "10-base.json"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            drop_in_order(listed),
            vec!["10-base.json".to_string(), "20-team.json".to_string()],
            "claude applies drop-ins in ascending name order, so the later one wins"
        );
    }

    /// The drop-in listing on a real directory: the `.json` files only, skipping
    /// sub-directories as well as the names [`drop_in_order`] rejects — while a
    /// missing directory is an empty listing rather than an error.
    #[test]
    fn managed_drop_ins_are_the_json_files_in_name_order() {
        let managed = temp_dir("drop-ins");
        assert!(
            managed_drop_ins(&managed).is_empty(),
            "no drop-in directory is no drop-ins"
        );

        let drop_in_dir = managed.join(MANAGED_DROP_IN_DIR);
        for name in ["20-team.json", "10-base.json", ".hidden.json", "notes.txt"] {
            write(&drop_in_dir.join(name), "{}");
        }
        std::fs::create_dir_all(drop_in_dir.join("30-dir.json")).expect("make a sub-dir");

        assert_eq!(
            managed_drop_ins(&managed),
            vec![
                drop_in_dir.join("10-base.json"),
                drop_in_dir.join("20-team.json")
            ]
        );
        std::fs::remove_dir_all(&managed).ok();
    }

    /// The whole read against real files: every layer in place, each one's model
    /// removed in turn from the top, with a managed drop-in overriding the managed
    /// file and a garbage user file along the way.
    #[test]
    fn the_disk_read_follows_claudes_settings_order() {
        let root = temp_dir("disk");
        let managed = root.join("managed");
        let launch = root.join("launch");
        let user = root.join("user");
        let drop_in = managed.join(MANAGED_DROP_IN_DIR).join("50-policy.json");
        let managed_file = managed.join(MANAGED_SETTINGS_FILE);
        let local = launch.join(CLAUDE_DIR).join(LOCAL_SETTINGS_FILE);
        let project = launch.join(CLAUDE_DIR).join(SETTINGS_FILE);
        let user_file = user.join(SETTINGS_FILE);
        let read = || defaults_from_disk(no_env, &managed, &launch, Some(&user)).new_session;

        assert_eq!(read(), None, "no files at all name no model");

        write(&user_file, "{ garbage");
        assert_eq!(read(), None, "a garbage user file names nothing");

        for (path, model) in [
            (&user_file, "user-pick"),
            (&project, "project-pick"),
            (&local, "local-pick"),
            (&managed_file, "managed-pick"),
            (&drop_in, "drop-in-pick"),
        ] {
            write(path, &model_layer(model));
            assert_eq!(
                read().as_deref(),
                Some(model),
                "{} must outrank every file written before it",
                path.display()
            );
        }

        assert_eq!(
            defaults_from_disk(
                env_with(&[(ANTHROPIC_MODEL_ENV, "env-pick")]),
                &managed,
                &launch,
                Some(&user)
            )
            .new_session
            .as_deref(),
            Some("env-pick"),
            "ANTHROPIC_MODEL still beats every file's model"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// A stated process environment with nothing set, so no case reads the real
    /// machine's variables.
    fn no_env(_: &str) -> Option<String> {
        None
    }

    /// A stated process environment holding exactly `vars`.
    fn env_with(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect();
        move |name| {
            vars.iter()
                .find(|(set, _)| set == name)
                .map(|(_, value)| value.clone())
        }
    }

    /// A settings file whose only content is `{"env": {<name>: <value>}}`.
    fn env_var_layer(name: &str, value: &str) -> String {
        serde_json::json!({ ENV_KEY: { name: value } }).to_string()
    }

    /// EACH of the five variables `claude` checks overrides the restore, from
    /// EITHER source — the inherited process environment or a settings file's `env`
    /// block — and nothing overrides it when none is set. A variable outside the
    /// list (a fifth family nobody ships, an unrelated `ANTHROPIC_*`) does not, so
    /// the list is pinned as the whole rule rather than as a prefix match.
    #[test]
    fn any_of_the_five_model_variables_overrides_the_restore_from_either_source() {
        assert!(
            !resolve_restore_overridden(no_env, &[]),
            "with nothing set, claude restores the session's model"
        );
        for name in RESTORE_OVERRIDE_ENV {
            assert!(
                resolve_restore_overridden(env_with(&[(name, "some-model")]), &[]),
                "{name} set in the process environment overrides the restore"
            );
            assert!(
                resolve_restore_overridden(no_env, &[env_var_layer(name, "some-model")]),
                "{name} set in a settings file's env block overrides the restore"
            );
        }
        for unrelated in [
            "ANTHROPIC_DEFAULT_FOO_MODEL",
            "ANTHROPIC_API_KEY",
            "CLAUDE_MODEL",
        ] {
            assert!(
                !resolve_restore_overridden(env_with(&[(unrelated, "x")]), &[]),
                "{unrelated} is not one of the variables claude checks"
            );
        }
        assert_eq!(
            RESTORE_OVERRIDE_ENV,
            [
                "ANTHROPIC_MODEL",
                "ANTHROPIC_DEFAULT_FABLE_MODEL",
                "ANTHROPIC_DEFAULT_OPUS_MODEL",
                "ANTHROPIC_DEFAULT_SONNET_MODEL",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            ],
            "the five names docs/agents/CLAUDE_CLI.md records for claude 2.1.282"
        );
    }

    /// Only a NON-EMPTY value counts, and a settings file's `env` is applied OVER
    /// the inherited environment — so an EMPTY entry there masks a set process
    /// variable, while a non-string one is "no value from this file" and leaves the
    /// next source to answer. The highest layer carrying a string decides.
    #[test]
    fn an_empty_override_does_not_count_and_a_settings_env_masks_the_process() {
        let opus = "ANTHROPIC_DEFAULT_OPUS_MODEL";
        assert!(
            !resolve_restore_overridden(env_with(&[(opus, "")]), &[]),
            "an empty variable is not set"
        );
        assert!(
            !resolve_restore_overridden(env_with(&[(opus, "x")]), &[env_var_layer(opus, "")]),
            "a settings file's empty env entry is copied over the process value"
        );
        assert!(
            !resolve_restore_overridden(
                no_env,
                &[env_var_layer(opus, ""), env_var_layer(opus, "x")]
            ),
            "the HIGHEST layer that sets it decides, even when it sets it empty"
        );
        let numeric = serde_json::json!({ ENV_KEY: { opus: 7 } }).to_string();
        assert!(
            resolve_restore_overridden(env_with(&[(opus, "x")]), &[numeric]),
            "a non-string entry is no value from that file, so the process decides"
        );
    }

    /// FAIL-SOFT, toward `false`: garbage JSON, a non-object top level and a
    /// non-object `env` contribute nothing — none of it panics, and a readable
    /// layer below still answers.
    #[test]
    fn garbage_layers_never_claim_a_restore_override() {
        let hostile = [
            "{ not json",
            "",
            "[\"env\"]",
            r#"{"env": "ANTHROPIC_MODEL=opus"}"#,
            r#"{"env": ["ANTHROPIC_MODEL"]}"#,
            r#"{"env": null}"#,
        ];
        for text in hostile {
            assert!(
                !resolve_restore_overridden(no_env, &[text.to_string()]),
                "{text:?} must not read as an override"
            );
            assert!(
                resolve_restore_overridden(
                    no_env,
                    &[text.to_string(), env_var_layer(ANTHROPIC_MODEL_ENV, "opus")]
                ),
                "{text:?} must leave the next file to answer"
            );
        }
    }

    /// The disk read answers BOTH questions from one pass over the files: an
    /// override variable in any readable layer's `env` block, or in the stated
    /// process environment, sets `restore_overridden`, beside the new-session
    /// model the same files name.
    #[test]
    fn the_disk_read_reports_a_restore_override_beside_the_new_session_model() {
        let root = temp_dir("restore");
        let managed = root.join("managed");
        let launch = root.join("launch");
        let user = root.join("user");
        let user_file = user.join(SETTINGS_FILE);
        let local = launch.join(CLAUDE_DIR).join(LOCAL_SETTINGS_FILE);
        let read = |env: &dyn Fn(&str) -> Option<String>| {
            defaults_from_disk(env, &managed, &launch, Some(&user))
        };

        assert_eq!(
            read(&no_env),
            ModelDefaults::default(),
            "no files and no environment: no model named, no override"
        );

        write(&user_file, &model_layer("opus[1m]"));
        assert_eq!(
            read(&no_env),
            ModelDefaults {
                new_session: Some("opus[1m]".to_string()),
                restore_overridden: false,
            },
            "a settings model names the new-session default and overrides nothing"
        );

        write(
            &local,
            &env_var_layer("ANTHROPIC_DEFAULT_SONNET_MODEL", "x"),
        );
        let with_file_env = read(&no_env);
        assert!(
            with_file_env.restore_overridden,
            "a per-family override in the local settings' env block is seen"
        );
        assert_eq!(
            with_file_env.new_session.as_deref(),
            Some("opus[1m]"),
            "and it leaves the new-session model alone"
        );

        std::fs::remove_file(&local).expect("remove the local settings file");
        assert!(
            read(&env_with(&[("ANTHROPIC_DEFAULT_HAIKU_MODEL", "x")])).restore_overridden,
            "an override in the stated process environment is seen too"
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
