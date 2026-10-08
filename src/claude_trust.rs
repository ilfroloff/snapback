//! Whether `claude` trusts a folder: its workspace-trust verdict, answered the
//! way the `claude 2.1.284` `--bg` trust gate answers it. The record it reads,
//! the rule and their provenance are `docs/agents/CLAUDE_CLI.md`'s ("The
//! `initialize` control handshake", "Workspace trust"); this module mirrors them
//! and states only where it deviates.
//!
//! # Who asks
//!
//! The compose pick list's catalog fetch (`crate::claude_catalog`). A folder
//! claude trusts is fetched with its project settings; any other with
//! `--setting-sources user`, so opening a compose box never runs a repository's
//! own helper commands or applies its `env` block where claude would not. The
//! `Ctrl-X w` move (`crate::claude_move`) asks the same question of the session's
//! current folder, and takes the same form from the answer.
//!
//! # Fail-soft, toward UNTRUSTED
//!
//! The record is parsed as a `serde_json::Value`, never a typed struct (AGENTS.md
//! FAIL-SOFT). A missing, unreadable or malformed record, an unknown folder, and
//! anything the mirror cannot settle all answer [`FolderTrust::Untrusted`]. A
//! wrong "untrusted" costs one list the repository's own items; a wrong "trusted"
//! runs the repository's code.
//!
//! # Where it runs
//!
//! [`folder_trust`] makes blocking FS reads, so only the catalog fetch's and the
//! move's worker threads call it (AGENTS.md OFF-UI-THREAD), never a key handler
//! or the render path.
//!
//! # Where it lives
//!
//! It reads files claude owns: the global config and a repository's `.git`
//! pointer files. They are not snapback-owned, so this is not `crate::config`.
//! They are not session files, and the store must never stat `.git` up a tree
//! (AGENTS.md PURE, GIT-FREE STORE CORE), so it is not `src/store/`. It answers a
//! security question that `crate::claude_settings`, a cosmetic model label that
//! deliberately leaves the global config out, should not own; it borrows only that
//! module's one `$CLAUDE_CONFIG_DIR` read. It spawns no process: `git` stays in
//! `crate::worktrees`, and a linked worktree is resolved from its pointer files,
//! as claude resolves it.
//!
//! # Deviations from claude, every one toward UNTRUSTED
//!
//! * A flag must be the JSON boolean `true`. claude's parent walk accepts any
//!   truthy value.
//! * Keys are the raw UTF-8 of the canonical path, with no NFC normalisation.
//!   claude writes NFC keys, so a non-NFC path matches none of them, and a
//!   non-UTF-8 or relative path is never looked up at all.
//! * Not mirrored, because each only ever ADDS trust: `CLAUDE_CODE_SANDBOXED`, the
//!   home directory's session-only trust, and the fallback to a symlinked
//!   spelling's own entry.
//! * Any `.git` entry bounds the parent walk, whatever its kind, and so does one
//!   that cannot be stat'ed. claude's bound is a `.git` directory or file, so this
//!   walk is never longer than claude's.
//! * The worktree → main checkout step accepts only regular, non-symlink pointer
//!   files within [`GIT_POINTER_MAX_BYTES`], symlink-free targets, a matching
//!   back-pointer and a non-bare main repository, all on one local mount. claude
//!   refuses a pointer into a network or magic location, or across `/home/<user>`
//!   automounts (claude 2.1.284 `Hi`); [`main_checkout_root`] refuses a superset
//!   of those. Any failure keys the folder on the worktree's own root, which
//!   claude's walk checks too.
//!
//! claude's internal `-local-oauth` / `-staging-oauth` record files are not
//! mirrored either: public builds never select them.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Read;
use std::ops::RangeInclusive;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::claude_settings;
use crate::defined_agents::CLAUDE_DIR;

/// claude's global config file, where `projects` records workspace trust. It
/// sits directly in `$CLAUDE_CONFIG_DIR`, else in the home directory (claude
/// 2.1.284 `M7n`; the env-vars docs: "`.claude.json` (the global config) lives
/// directly in the specified directory").
const GLOBAL_CONFIG_FILE: &str = ".claude.json";

/// The global config's name when [`CUSTOM_OAUTH_URL_ENV`] is set (claude 2.1.284
/// `tq`: the `-custom-oauth` suffix).
const CUSTOM_OAUTH_GLOBAL_CONFIG_FILE: &str = ".claude-custom-oauth.json";

/// A legacy global config in claude's config home (`$CLAUDE_CONFIG_DIR`, else
/// `~/.claude`). When it exists claude reads it INSTEAD of the global config
/// (claude 2.1.284 `Lo`).
const LEGACY_GLOBAL_CONFIG_FILE: &str = ".config.json";

/// Set and non-empty, it renames the global config to
/// [`CUSTOM_OAUTH_GLOBAL_CONFIG_FILE`] (claude 2.1.284 `tq` tests it for
/// truthiness, so an empty value counts as unset).
const CUSTOM_OAUTH_URL_ENV: &str = "CLAUDE_CODE_CUSTOM_OAUTH_URL";

/// The global config's key mapping a folder's key to its project entry.
const PROJECTS_KEY: &str = "projects";

/// The project entry's workspace-trust flag: `true` once the user accepted the
/// trust dialog for that folder.
const TRUST_ACCEPTED_KEY: &str = "hasTrustDialogAccepted";

/// The entry that makes a directory a git root: a directory in a main checkout,
/// a pointer file in a linked worktree.
const GIT_ENTRY: &str = ".git";

/// How a linked worktree's `.git` pointer file starts; the rest of its one line
/// names the worktree's own git directory.
const GITDIR_PREFIX: &str = "gitdir:";

/// The file in a linked worktree's git directory naming the main repository's
/// shared git directory, relative to it.
const COMMONDIR_FILE: &str = "commondir";

/// The file in a linked worktree's git directory pointing BACK at the
/// worktree's `.git` file. It is what proves a `.git` file really is that
/// worktree's, rather than a folder claiming a repository it does not belong to.
const GITDIR_FILE: &str = "gitdir";

/// The directory in a main repository's git directory that holds one git
/// directory per linked worktree.
const WORKTREES_DIR: &str = "worktrees";

/// The most bytes a git pointer file may hold. One holds a single path line, and
/// a path is at most `PATH_MAX` bytes (1024 on macOS, 4096 on Linux), so a longer
/// file is not a pointer and gets the safe fallback rather than a whole read.
const GIT_POINTER_MAX_BYTES: u64 = 8192;

/// A leading double slash: a UNC-style spelling claude refuses in a worktree
/// pointer (claude 2.1.284 `Hi` → `tQ`/`Pn`).
const UNC_PREFIX: &str = "//";

/// claude reads a backslash as a separator in its UNC and NT-path pointer checks
/// (claude 2.1.284 `Pn`/`QN`); git never writes one into a pointer on unix.
const BACKSLASH: char = '\\';

/// The marker of an NT object path (`\??\`), which claude refuses in a worktree
/// pointer (claude 2.1.284 `QN`).
const NT_OBJECT_MARKER: &str = "??";

/// Format characters claude deletes from a path component before it compares a
/// mount name (claude 2.1.284 `eT`). A pointer or path holding one is refused
/// rather than the deletion mirrored.
const MOUNT_NAME_FORMAT_CHARS: [RangeInclusive<char>; 4] = [
    '\u{200c}'..='\u{200f}',
    '\u{202a}'..='\u{202e}',
    '\u{206a}'..='\u{206f}',
    '\u{feff}'..='\u{feff}',
];

/// The macOS firmlink prefix claude folds away before classifying a path's mount
/// (claude 2.1.284 `JN`), compared after [`fold_mount_name`].
const FIRMLINK_PREFIX: [&str; 3] = ["system", "volumes", "data"];

/// First path components (after [`fold_mount_name`]) of network and magic
/// locations claude refuses a worktree pointer into (claude 2.1.284 `Hi`): the
/// `/net` and `/Network` automounts, and macOS's `/.vol`, `/.file`, `/.nofollow`
/// and `/.resolve`. claude lets a `/net/<host>` pointer through when the worktree
/// sits on the same host; this list refuses it whatever the host.
const REFUSED_MOUNT_ROOTS: [&str; 6] = ["net", "network", ".vol", ".file", ".nofollow", ".resolve"];

/// The first path component of the `/home/<user>` automounts claude refuses a
/// worktree pointer to cross between (claude 2.1.284 `nt`/`CQn`).
const HOME_MOUNT_ROOT: &str = "home";

/// claude's workspace-trust verdict for one folder at one instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderTrust {
    /// claude would load the folder's project settings.
    Trusted,
    /// claude has not been told to trust the folder, or this module could not
    /// tell (see the module doc's fail-soft direction).
    Untrusted,
}

/// Where claude's global config, the trust record, lives (claude 2.1.284
/// `Lo`/`M7n`):
///
/// 1. The legacy [`LEGACY_GLOBAL_CONFIG_FILE`] in the config home, when `exists`
///    says it is there. The config home is `config_dir_override`, else
///    `home/.claude`.
/// 2. Else [`CUSTOM_OAUTH_GLOBAL_CONFIG_FILE`] when `custom_oauth`, otherwise
///    [`GLOBAL_CONFIG_FILE`], in `config_dir_override`, else in `home`.
///
/// `None` when neither the override nor a home is known.
#[must_use]
pub(crate) fn global_config_path(
    config_dir_override: Option<&Path>,
    home: Option<&Path>,
    custom_oauth: bool,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let config_home = config_dir_override
        .map(Path::to_path_buf)
        .or_else(|| home.map(|home| home.join(CLAUDE_DIR)));
    if let Some(legacy) = config_home.map(|dir| dir.join(LEGACY_GLOBAL_CONFIG_FILE)) {
        if exists(&legacy) {
            return Some(legacy);
        }
    }
    let file = if custom_oauth {
        CUSTOM_OAUTH_GLOBAL_CONFIG_FILE
    } else {
        GLOBAL_CONFIG_FILE
    };
    config_dir_override.or(home).map(|dir| dir.join(file))
}

/// `path` with its `.` and `..` components folded lexically, never touching the
/// FS: the `path.resolve` step claude applies before it compares worktree
/// pointers. `..` at the root stays at the root, and a relative path stays
/// relative (a leading `..` is kept).
#[must_use]
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match normal.components().next_back() {
                Some(Component::Normal(_)) => {
                    normal.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                Some(Component::ParentDir | Component::CurDir) | None => normal.push(".."),
            },
            other => normal.push(other),
        }
    }
    normal
}

/// Whether claude deletes `c` from a mount name ([`MOUNT_NAME_FORMAT_CHARS`]).
fn is_mount_name_format_char(c: char) -> bool {
    MOUNT_NAME_FORMAT_CHARS
        .iter()
        .any(|range| range.contains(&c))
}

/// The one path a git pointer file holds: `text` trimmed, or `None` when it is
/// empty or holds what no pointer git writes on unix: a control character (a
/// second line among them), a backslash, a leading [`UNC_PREFIX`], an
/// [`NT_OBJECT_MARKER`], or a [`MOUNT_NAME_FORMAT_CHARS`] character. claude
/// refuses the UNC and NT spellings itself (`Hi`); refusing the rest is stricter.
fn pointer_value(text: &str) -> Option<&str> {
    let value = text.trim();
    let refused = value.is_empty()
        || value.starts_with(UNC_PREFIX)
        || value.contains(NT_OBJECT_MARKER)
        || value
            .chars()
            .any(|c| c.is_control() || c == BACKSLASH || is_mount_name_format_char(c));
    (!refused).then_some(value)
}

/// Where a path sits, as far as claude's worktree pointer checks care.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Mount {
    /// An ordinary local path.
    Local,
    /// Under `/home/<user>`, an automount a pointer may not cross out of.
    Home(String),
    /// A network or magic location claude refuses outright, or a path this
    /// module will not classify.
    Refused,
}

/// `name` folded the way claude compares a mount name: upper-cased, then
/// lower-cased (claude 2.1.284 `N`/`eT`).
fn fold_mount_name(name: &str) -> String {
    name.chars()
        .flat_map(char::to_uppercase)
        .flat_map(char::to_lowercase)
        .collect()
}

/// The [`Mount`] of `path`, a lexically normal absolute path: a superset of the
/// locations claude 2.1.284 refuses a worktree pointer into (`Hi`, `nt`, `rj`,
/// `CQn`). A [`FIRMLINK_PREFIX`] is skipped first; then a [`REFUSED_MOUNT_ROOTS`]
/// root is [`Mount::Refused`] and `/home/<user>` is [`Mount::Home`]. A relative
/// path, a `.`/`..` or non-UTF-8 component, a [`MOUNT_NAME_FORMAT_CHARS`]
/// character and `/home` itself are refused too.
fn mount_of(path: &Path) -> Mount {
    if !path.has_root() {
        return Mount::Refused;
    }
    let mut names = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => match name.to_str() {
                Some(name) if !name.chars().any(is_mount_name_format_char) => names.push(name),
                _ => return Mount::Refused,
            },
            Component::Prefix(_) | Component::CurDir | Component::ParentDir => {
                return Mount::Refused
            }
        }
    }
    let folded: Vec<String> = names.iter().map(|name| fold_mount_name(name)).collect();
    let behind_firmlink = folded.len() >= FIRMLINK_PREFIX.len()
        && folded
            .iter()
            .zip(FIRMLINK_PREFIX)
            .all(|(name, prefix)| name == prefix);
    let root = if behind_firmlink {
        FIRMLINK_PREFIX.len()
    } else {
        0
    };
    match folded.get(root).map(String::as_str) {
        Some(name) if REFUSED_MOUNT_ROOTS.contains(&name) => Mount::Refused,
        Some(HOME_MOUNT_ROOT) => names
            .get(root + 1)
            .map_or(Mount::Refused, |user| Mount::Home((*user).to_owned())),
        _ => Mount::Local,
    }
}

/// The git directory a linked worktree's `.git` file (`dot_git`, its text) names,
/// joined onto `git_root` (an absolute value stays absolute) and lexically
/// normalised. `None` means "not a linked worktree": no [`GITDIR_PREFIX`], or a
/// value [`pointer_value`] rejects.
#[must_use]
pub(crate) fn worktree_gitdir(git_root: &Path, dot_git: &str) -> Option<PathBuf> {
    let value = pointer_value(dot_git.trim().strip_prefix(GITDIR_PREFIX)?)?;
    Some(lexical_normalize(&git_root.join(value)))
}

/// The main checkout a linked worktree at `git_root` belongs to, from its git
/// directory `gitdir` and the texts of that directory's [`COMMONDIR_FILE`] and
/// [`GITDIR_FILE`]. With `a` the normalised `commondir` target, ALL must hold:
///
/// * `gitdir` sits in `a`'s [`WORKTREES_DIR`];
/// * the back-pointer resolves to `git_root`'s own `.git`, so the worktree is the
///   one that repository registered;
/// * `a` is named `.git`. A bare main repository has no checkout to key on;
/// * `git_root`, `gitdir` and `a` share one [`Mount`] that is not
///   [`Mount::Refused`], which is stricter than claude's own pointer checks.
///
/// It then returns `a`'s parent. `None` means "key on `git_root` itself"; every
/// rejection is the safe direction (module doc).
#[must_use]
pub(crate) fn main_checkout_root(
    git_root: &Path,
    gitdir: &Path,
    commondir: &str,
    back_pointer: &str,
) -> Option<PathBuf> {
    let common = lexical_normalize(&gitdir.join(pointer_value(commondir)?));
    if gitdir.parent() != Some(common.join(WORKTREES_DIR).as_path()) {
        return None;
    }
    if lexical_normalize(&gitdir.join(pointer_value(back_pointer)?)) != git_root.join(GIT_ENTRY) {
        return None;
    }
    if common.file_name() != Some(OsStr::new(GIT_ENTRY)) {
        return None;
    }
    let mount = mount_of(git_root);
    if mount == Mount::Refused || mount_of(gitdir) != mount || mount_of(&common) != mount {
        return None;
    }
    common.parent().map(Path::to_path_buf)
}

/// The record keys that can trust one folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrustKeys {
    /// The key whose flag trusts the folder outright: its repository's canonical
    /// root (the main checkout for a linked worktree), else the folder itself.
    exact: String,
    /// The folder and each ancestor, nearest first, up to AND including its git
    /// root, else up to `/`. Any one flagged trusts the folder.
    walk: Vec<String>,
}

/// The [`TrustKeys`] for `folder` (canonical), given its `git_root` (the nearest
/// ancestor-or-self holding a `.git` entry) and that repository's
/// `canonical_root`.
///
/// `exact` is `canonical_root`, else `git_root`, else `folder`. The walk starts
/// at `folder` and ends at `git_root`, or at `/` outside any repository; a
/// `git_root` that is not an ancestor-or-self of `folder` walks nothing, as
/// claude's walk refuses a start outside its bound. Each key is the path's UTF-8,
/// verbatim.
///
/// `None` when `folder` is relative or any key is not UTF-8; the caller reads it
/// as untrusted.
#[must_use]
pub(crate) fn trust_keys(
    folder: &Path,
    git_root: Option<&Path>,
    canonical_root: Option<&Path>,
) -> Option<TrustKeys> {
    if !folder.is_absolute() {
        return None;
    }
    let exact = canonical_root.or(git_root).unwrap_or(folder);
    let mut walk = Vec::new();
    if git_root.is_none_or(|root| folder.starts_with(root)) {
        for dir in folder.ancestors() {
            walk.push(dir.to_str()?.to_owned());
            if Some(dir) == git_root {
                break;
            }
        }
    }
    Some(TrustKeys {
        exact: exact.to_str()?.to_owned(),
        walk,
    })
}

/// Whether claude's global `config` trusts the folder `keys` were built for:
/// `config.projects` is an object whose entry for `keys.exact`, or for any
/// `keys.walk` key, is an object whose [`TRUST_ACCEPTED_KEY`] is exactly
/// `Value::Bool(true)`. Any other shape answers `false`.
#[must_use]
pub(crate) fn is_trusted(config: &Value, keys: &TrustKeys) -> bool {
    let Some(projects) = config.get(PROJECTS_KEY).and_then(Value::as_object) else {
        return false;
    };
    std::iter::once(&keys.exact).chain(&keys.walk).any(|key| {
        projects
            .get(key)
            .and_then(Value::as_object)
            .and_then(|entry| entry.get(TRUST_ACCEPTED_KEY))
            == Some(&Value::Bool(true))
    })
}

/// Whether `dir` holds a `.git` entry of ANY kind, as `symlink_metadata` sees it.
/// An error other than "not found" counts as one too: it may hide a repository,
/// and a shorter walk can only remove trust.
fn has_git_entry(dir: &Path) -> bool {
    match fs::symlink_metadata(dir.join(GIT_ENTRY)) {
        Ok(_) => true,
        Err(err) => err.kind() != std::io::ErrorKind::NotFound,
    }
}

/// `folder`'s git root: its nearest ancestor-or-self holding a `.git` entry
/// ([`has_git_entry`]), or `None` outside any repository.
fn find_git_root(folder: &Path) -> Option<PathBuf> {
    folder
        .ancestors()
        .find(|dir| has_git_entry(dir))
        .map(Path::to_path_buf)
}

/// The text of the git pointer file at `path`, or `None` unless it is a REGULAR
/// file (not a symlink, per `symlink_metadata`) of at most
/// [`GIT_POINTER_MAX_BYTES`] of UTF-8. The read itself is bounded too, so a file
/// that grows after the check is still never read whole.
fn read_pointer_file(path: &Path) -> Option<String> {
    if !fs::symlink_metadata(path).ok()?.file_type().is_file() {
        return None;
    }
    let mut text = String::new();
    let read = File::open(path)
        .ok()?
        .take(GIT_POINTER_MAX_BYTES + 1)
        .read_to_string(&mut text)
        .ok()?;
    u64::try_from(read)
        .is_ok_and(|read| read <= GIT_POINTER_MAX_BYTES)
        .then_some(text)
}

/// `path` when it is already canonical, so no symlink lies along it.
fn symlink_free(path: &Path) -> Option<&Path> {
    (fs::canonicalize(path).ok()? == path).then_some(path)
}

/// The main checkout of the linked worktree at `git_root`, when every pointer
/// checks out (see [`main_checkout_root`] and the module doc); `None` otherwise.
fn resolve_main_checkout(git_root: &Path) -> Option<PathBuf> {
    let dot_git = read_pointer_file(&git_root.join(GIT_ENTRY))?;
    let gitdir = worktree_gitdir(git_root, &dot_git)?;
    symlink_free(&gitdir)?;
    let commondir = read_pointer_file(&gitdir.join(COMMONDIR_FILE))?;
    let back_pointer = read_pointer_file(&gitdir.join(GITDIR_FILE))?;
    let main = main_checkout_root(git_root, &gitdir, &commondir, &back_pointer)?;
    symlink_free(&main.join(GIT_ENTRY))?;
    Some(main)
}

/// The canonical root of the repository whose git root is `git_root`: the main
/// checkout for a linked worktree, else `git_root` itself, which is also the
/// answer whenever the worktree's pointers do not check out.
fn canonical_root(git_root: &Path) -> PathBuf {
    resolve_main_checkout(git_root).unwrap_or_else(|| git_root.to_path_buf())
}

/// claude's verdict for `folder`, read from the global config at `config_file`.
///
/// The folder is canonicalised first, because claude keys trust by realpath.
/// Any failure answers `Untrusted`: a folder that cannot be canonicalised, a key
/// that is not UTF-8, a record that cannot be read or is not JSON. A leading
/// byte-order mark is NOT stripped: whether claude strips one from this file is
/// unverified, and a record that fails to parse must read as untrusted rather
/// than be coaxed into trusting.
#[must_use]
pub(crate) fn folder_trust_in(config_file: &Path, folder: &Path) -> FolderTrust {
    let trusted = || -> Option<bool> {
        let folder = fs::canonicalize(folder).ok()?;
        let git_root = find_git_root(&folder);
        let canonical = git_root.as_deref().map(canonical_root);
        let keys = trust_keys(&folder, git_root.as_deref(), canonical.as_deref())?;
        let record = fs::read_to_string(config_file).ok()?;
        let config: Value = serde_json::from_str(&record).ok()?;
        Some(is_trusted(&config, &keys))
    };
    if trusted().unwrap_or(false) {
        FolderTrust::Trusted
    } else {
        FolderTrust::Untrusted
    }
}

/// claude's verdict for `folder`, read from the real global config.
///
/// The ONE site naming the real environment and home: `$CLAUDE_CONFIG_DIR`
/// through `claude_settings`, [`CUSTOM_OAUTH_URL_ENV`] and the home directory.
/// No locatable record answers `Untrusted`. Blocking FS reads: the catalog
/// fetch's and the `Ctrl-X w` move's worker threads only.
#[must_use]
pub fn folder_trust(folder: &Path) -> FolderTrust {
    let custom_oauth = std::env::var_os(CUSTOM_OAUTH_URL_ENV).is_some_and(|url| !url.is_empty());
    let home = dirs::home_dir();
    let override_dir = claude_settings::claude_config_dir_override();
    match global_config_path(
        override_dir.as_deref(),
        home.as_deref(),
        custom_oauth,
        Path::exists,
    ) {
        Some(config_file) => folder_trust_in(&config_file, folder),
        None => FolderTrust::Untrusted,
    }
}

/// The rule's two clauses, as the tests name them (CLAUDE_CLI.md owns the rule):
/// clause (i) is the EXACT key, the canonical root's flag; clause (ii) is the
/// WALK, any flagged folder from the folder up to its git root, else to `/`.
#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::{json, Map};

    /// A global config whose `projects` flags exactly `flagged`, the shape claude
    /// writes.
    fn record(flagged: &[&str]) -> Value {
        let projects: Map<String, Value> = flagged
            .iter()
            .map(|key| ((*key).to_owned(), json!({ TRUST_ACCEPTED_KEY: true })))
            .collect();
        json!({ PROJECTS_KEY: projects })
    }

    /// A global config whose `projects` holds `entry` under `key` alone.
    fn record_with_entry(key: &str, entry: Value) -> Value {
        let mut projects = Map::new();
        projects.insert(key.to_owned(), entry);
        json!({ PROJECTS_KEY: projects })
    }

    /// The pure verdict for `folder`, given its git root and canonical root.
    fn trusts(
        config: &Value,
        folder: &str,
        git_root: Option<&str>,
        canonical_root: Option<&str>,
    ) -> bool {
        let keys = trust_keys(
            Path::new(folder),
            git_root.map(Path::new),
            canonical_root.map(Path::new),
        )
        .expect("an absolute UTF-8 folder has keys");
        is_trusted(config, &keys)
    }

    /// (a) A folder the record does not name is untrusted however many OTHER
    /// folders it trusts, a sibling sharing its name as a prefix among them.
    #[test]
    fn an_unknown_folder_is_untrusted() {
        let others = record(&["/elsewhere", "/work/other", "/work/folder-sibling"]);
        assert!(
            !trusts(&others, "/work/folder", None, None),
            "no key for the folder or any ancestor means no trust"
        );
        assert!(
            trusts(&record(&["/work/folder"]), "/work/folder", None, None),
            "control: the same folder flagged is trusted"
        );
    }

    /// (b) Only the JSON boolean `true` trusts. claude's walk would accept a
    /// truthy `1` or `"true"`; this mirror never does (a deviation toward
    /// untrusted).
    #[test]
    fn only_a_literal_true_flag_trusts() {
        let not_true = [
            json!({ TRUST_ACCEPTED_KEY: 1 }),
            json!({ TRUST_ACCEPTED_KEY: "true" }),
            json!({ TRUST_ACCEPTED_KEY: null }),
            json!({ TRUST_ACCEPTED_KEY: false }),
            json!({ "allowedTools": [] }),
            json!(true),
            json!("hasTrustDialogAccepted"),
            json!([{ TRUST_ACCEPTED_KEY: true }]),
        ];
        for entry in not_true {
            let config = record_with_entry("/work/folder", entry.clone());
            assert!(
                !trusts(&config, "/work/folder", None, None),
                "the entry {entry} must not trust the folder"
            );
        }
        let flagged = record_with_entry("/work/folder", json!({ TRUST_ACCEPTED_KEY: true }));
        assert!(trusts(&flagged, "/work/folder", None, None));
    }

    /// (c) Outside any repository the walk (clause (ii)) climbs to `/`, so a
    /// trusted parent covers every subfolder; a child's trust never covers its
    /// parent.
    #[test]
    fn a_trusted_parent_covers_a_subfolder_outside_any_repository() {
        assert!(trusts(&record(&["/work"]), "/work/a/b", None, None));
        assert!(
            trusts(&record(&["/"]), "/work/a/b", None, None),
            "the walk reaches the root itself"
        );
        assert!(
            !trusts(&record(&["/work/a/b/c"]), "/work/a/b", None, None),
            "the walk only climbs"
        );
    }

    /// (d) The walk (clause (ii)) stops AT the git root, so a trusted folder
    /// ABOVE a repository never reaches into it.
    #[test]
    fn a_trusted_parent_above_a_repository_does_not_reach_into_it() {
        assert!(!trusts(
            &record(&["/a"]),
            "/a/repo/sub",
            Some("/a/repo"),
            Some("/a/repo")
        ));
    }

    /// (e) A trusted repository root covers its subfolders (clause (i) and the
    /// walk's last step), and a trusted folder INSIDE a repository covers its own
    /// subfolders through the walk (clause (ii)) alone.
    #[test]
    fn a_trusted_repository_root_covers_its_subfolders() {
        assert!(trusts(&record(&["/r"]), "/r/a/b", Some("/r"), Some("/r")));
        assert!(
            trusts(&record(&["/r/a"]), "/r/a/b", Some("/r"), Some("/r")),
            "only the walk passes /r/a"
        );
    }

    /// (f) A linked worktree keys on its MAIN checkout (clause (i)): the walk
    /// (clause (ii)) stops at the worktree's own root, inside the main checkout,
    /// and never reaches it.
    #[test]
    fn a_worktree_inherits_its_main_checkouts_trust() {
        let main_trusted = record(&["/m"]);
        assert!(trusts(
            &main_trusted,
            "/m/.agents/wt/src",
            Some("/m/.agents/wt"),
            Some("/m")
        ));
        assert!(
            !trusts(
                &main_trusted,
                "/m/.agents/wt/src",
                Some("/m/.agents/wt"),
                Some("/m/.agents/wt")
            ),
            "keyed on the worktree's own root, nothing reaches /m"
        );
    }

    /// (g) Keys match verbatim: no trailing-slash folding and no symlink
    /// spelling (`/tmp` is `/private/tmp` on macOS; the reader canonicalises the
    /// FOLDER, never the record's keys).
    #[test]
    fn keys_match_verbatim() {
        assert!(!trusts(&record(&["/a/b/"]), "/a/b", None, None));
        assert!(!trusts(&record(&["/tmp/x"]), "/private/tmp/x", None, None));
        assert!(trusts(&record(&["/a/b"]), "/a/b", None, None), "control");
    }

    /// (h) A record of the wrong shape trusts nothing, even where it spells a
    /// trusted flag for the folder somewhere inside.
    #[test]
    fn a_malformed_record_is_untrusted() {
        let projects = |value: Value| json!({ PROJECTS_KEY: value });
        let flag = json!({ TRUST_ACCEPTED_KEY: true });
        let hostile = [
            json!([{ PROJECTS_KEY: { "/a": flag.clone() } }]),
            json!("projects"),
            json!(null),
            json!(7),
            projects(json!(["/a"])),
            projects(json!("/a")),
            projects(Value::Null),
            projects(json!({ "/a": true })),
            projects(json!({ "/a": [flag.clone()] })),
            json!({ "Projects": { "/a": flag.clone() } }),
            json!({ "/a": flag }),
        ];
        for config in hostile {
            assert!(
                !trusts(&config, "/a", None, None),
                "{config} must trust nothing"
            );
        }
    }

    /// (i) The keys' shapes: which key is `exact`, where the walk ends, and when
    /// there are no keys at all.
    #[test]
    fn trust_keys_choose_the_exact_key_and_bound_the_walk() {
        let keys = |folder: &str, git_root: Option<&str>, canonical_root: Option<&str>| {
            trust_keys(
                Path::new(folder),
                git_root.map(Path::new),
                canonical_root.map(Path::new),
            )
        };
        let exact = |folder, git_root, canonical_root| {
            keys(folder, git_root, canonical_root).expect("keys").exact
        };
        let walk = |folder, git_root, canonical_root| {
            keys(folder, git_root, canonical_root).expect("keys").walk
        };

        assert_eq!(exact("/m/wt/src", Some("/m/wt"), Some("/m")), "/m");
        assert_eq!(exact("/r/src", Some("/r"), None), "/r");
        assert_eq!(exact("/x/y", None, None), "/x/y");

        assert_eq!(
            walk("/r/a/b", Some("/r"), Some("/r")),
            ["/r/a/b", "/r/a", "/r"]
        );
        assert_eq!(walk("/r", Some("/r"), Some("/r")), ["/r"]);
        assert_eq!(walk("/x/y", None, None), ["/x/y", "/x", "/"]);
        assert!(
            walk("/r2/a", Some("/r"), Some("/r")).is_empty(),
            "a git root that is not an ancestor bounds a walk that never starts"
        );

        assert_eq!(keys("x/y", None, None), None, "a relative folder");
    }

    /// (i, unix) A path that is not UTF-8 can match no key claude wrote, so it
    /// has no keys at all, wherever it sits.
    #[cfg(unix)]
    #[test]
    fn a_non_utf8_path_has_no_trust_keys() {
        use std::os::unix::ffi::OsStrExt;

        let not_utf8 = Path::new(OsStr::from_bytes(b"/work/\xff/sub"));
        assert_eq!(trust_keys(not_utf8, None, None), None);
        assert_eq!(
            trust_keys(
                Path::new("/r/sub"),
                Some(Path::new("/r")),
                Some(Path::new(OsStr::from_bytes(b"/m\xff")))
            ),
            None,
            "a canonical root that is not UTF-8"
        );
    }

    /// (j) Lexical normalisation folds `.` and `..` without the filesystem.
    #[test]
    fn lexical_normalize_folds_dots_lexically() {
        for (input, expected) in [
            ("/a/./b/../c", "/a/c"),
            ("/m/.git/worktrees/wt/../..", "/m/.git"),
            ("/a/b/../../..", "/"),
            ("/..", "/"),
            ("/a/b/", "/a/b"),
            ("./a", "a"),
            ("../x", "../x"),
            ("a/../../b", "../b"),
        ] {
            assert_eq!(
                lexical_normalize(Path::new(input)),
                PathBuf::from(expected),
                "{input}"
            );
        }
    }

    /// (k) A worktree's `.git` file names one git directory on one line.
    #[test]
    fn worktree_gitdir_reads_one_pointer_line() {
        let root = Path::new("/m/.agents/wt");
        let gitdir = Some(PathBuf::from("/m/.git/worktrees/wt"));
        assert_eq!(
            worktree_gitdir(root, "gitdir: /m/.git/worktrees/wt\n"),
            gitdir
        );
        assert_eq!(
            worktree_gitdir(root, "gitdir: ../../.git/worktrees/wt"),
            gitdir,
            "a relative value is joined onto the worktree root"
        );
        assert_eq!(
            worktree_gitdir(root, "  gitdir:\t/m/.git/worktrees/wt  \r\n"),
            gitdir,
            "padding is trimmed"
        );
        for not_a_pointer in [
            "/m/.git/worktrees/wt",
            "gitdir:   \n",
            "gitdir: /m/.git/worktrees/wt\n/elsewhere",
            "gitdir: /m/\u{1b}[0m/.git/worktrees/wt",
            "gitdir: //server/m/.git/worktrees/wt",
            "gitdir: \\\\server\\m\\.git\\worktrees\\wt",
            "gitdir: /m/../??/m/.git/worktrees/wt",
            "gitdir: /m/.git/work\u{200c}trees/wt",
        ] {
            assert_eq!(
                worktree_gitdir(root, not_a_pointer),
                None,
                "{not_a_pointer:?}"
            );
        }
    }

    /// (l) The main checkout is resolved only when the worktree's git directory,
    /// its `commondir` and its back-pointer all agree.
    #[test]
    fn main_checkout_root_needs_every_pointer_to_agree() {
        let wt = Path::new("/m/.agents/wt");
        let gitdir = Path::new("/m/.git/worktrees/wt");
        let back = "/m/.agents/wt/.git\n";
        let main = Some(PathBuf::from("/m"));

        assert_eq!(main_checkout_root(wt, gitdir, "../..\n", back), main);
        assert_eq!(
            main_checkout_root(wt, gitdir, "../..", "../../../.agents/wt/.git"),
            main,
            "git's relative back-pointer (worktree.useRelativePaths)"
        );

        assert_eq!(
            main_checkout_root(wt, Path::new("/m/.git/other/wt"), "../..", back),
            None,
            "a git directory outside the repository's worktrees directory"
        );
        assert_eq!(
            main_checkout_root(wt, gitdir, "../../../../other/.git", back),
            None,
            "a commondir naming another repository"
        );
        assert_eq!(
            main_checkout_root(wt, gitdir, "../..", "/elsewhere/.git"),
            None,
            "a back-pointer to another worktree"
        );
        assert_eq!(
            main_checkout_root(
                Path::new("/srv/wt"),
                Path::new("/srv/repo.git/worktrees/wt"),
                "../..",
                "/srv/wt/.git"
            ),
            None,
            "a bare main repository"
        );
        assert_eq!(
            main_checkout_root(wt, gitdir, "", back),
            None,
            "no commondir"
        );
    }

    /// (l) The main checkout is resolved only on one local mount: never into a
    /// network or magic location claude refuses a pointer into, and never across
    /// `/home/<user>` automounts, while a worktree inside its own user's home
    /// (the usual Linux layout) still resolves.
    #[test]
    fn main_checkout_root_stays_on_one_local_mount() {
        let resolve = |wt: &str, main: &str| {
            let gitdir = format!("{main}/.git/worktrees/wt");
            main_checkout_root(
                Path::new(wt),
                Path::new(&gitdir),
                "../..",
                &format!("{wt}/.git"),
            )
        };
        assert_eq!(
            resolve("/home/me/m/.agents/wt", "/home/me/m"),
            Some(PathBuf::from("/home/me/m"))
        );
        for (wt, main) in [
            ("/net/host/m/.agents/wt", "/net/host/m"),
            ("/NET/host/m/.agents/wt", "/NET/host/m"),
            ("/Network/Servers/host/m/wt", "/Network/Servers/host/m"),
            (
                "/System/Volumes/Data/Network/m/wt",
                "/System/Volumes/Data/Network/m",
            ),
            ("/.vol/1/2/m/wt", "/.vol/1/2/m"),
            ("/home/me/wt", "/home/other/m"),
            ("/srv/wt", "/home/me/m"),
            ("/home/me/wt", "/srv/m"),
        ] {
            assert_eq!(resolve(wt, main), None, "{wt} -> {main}");
        }
    }

    /// The mount classes behind the one-mount rule.
    #[test]
    fn mount_of_classifies_the_locations_claude_refuses() {
        for (path, mount) in [
            ("/Users/me/m", Mount::Local),
            ("/Volumes/Development/m", Mount::Local),
            ("/", Mount::Local),
            ("/home/me/m", Mount::Home("me".to_owned())),
            ("/HOME/me", Mount::Home("me".to_owned())),
            (
                "/System/Volumes/Data/home/me/m",
                Mount::Home("me".to_owned()),
            ),
            ("/home", Mount::Refused),
            ("/net", Mount::Refused),
            ("/net/host/m", Mount::Refused),
            ("/Network/Servers/host", Mount::Refused),
            ("/.file/id=1", Mount::Refused),
            ("/.nofollow/m", Mount::Refused),
            ("/.resolve/1/2", Mount::Refused),
            ("relative/m", Mount::Refused),
            ("/m/../net", Mount::Refused),
            ("/ne\u{200c}t/host", Mount::Refused),
        ] {
            assert_eq!(mount_of(Path::new(path)), mount, "{path}");
        }
    }

    /// `exists` that answers yes for `present` alone.
    fn only(present: &'static str) -> impl Fn(&Path) -> bool {
        move |path| path == Path::new(present)
    }

    /// (m) The record's location follows claude's lookup: the legacy file in the
    /// config home first, then the (custom-OAuth) global config in the override,
    /// else the home directory.
    #[test]
    fn global_config_path_follows_claudes_lookup() {
        let cfg = Some(Path::new("/cfg"));
        let home = Some(Path::new("/home/me"));
        let path = |p: &str| Some(PathBuf::from(p));
        let nothing = |_: &Path| false;

        assert_eq!(
            global_config_path(cfg, home, false, nothing),
            path("/cfg/.claude.json")
        );
        assert_eq!(
            global_config_path(cfg, None, false, nothing),
            path("/cfg/.claude.json")
        );
        assert_eq!(
            global_config_path(None, home, false, nothing),
            path("/home/me/.claude.json")
        );
        assert_eq!(
            global_config_path(cfg, home, true, nothing),
            path("/cfg/.claude-custom-oauth.json")
        );
        assert_eq!(
            global_config_path(None, home, true, nothing),
            path("/home/me/.claude-custom-oauth.json")
        );

        assert_eq!(
            global_config_path(cfg, home, false, only("/cfg/.config.json")),
            path("/cfg/.config.json"),
            "the legacy file under the override"
        );
        assert_eq!(
            global_config_path(None, home, true, only("/home/me/.claude/.config.json")),
            path("/home/me/.claude/.config.json"),
            "the legacy file under ~/.claude, even with custom OAuth"
        );
        assert_eq!(
            global_config_path(cfg, home, false, only("/home/me/.claude/.config.json")),
            path("/cfg/.claude.json"),
            "with an override, ~/.claude is not the config home"
        );

        assert_eq!(global_config_path(None, None, false, |_| true), None);
    }

    /// A unique temp dir for one test under the CANONICAL temp dir (macOS
    /// `/var` is `/private/var`), so the paths a test writes into a record are
    /// the realpaths the reader keys on. Never the real `~/.claude.json`.
    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let base = fs::canonicalize(std::env::temp_dir()).expect("canonicalise the temp dir");
        let dir = base.join(format!(
            "snapback-claude-trust-{tag}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create the test's temp dir");
        dir
    }

    /// Write `contents` to `path`, creating its parent directories.
    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().expect("a file path has a parent"))
            .expect("create the parent dir");
        fs::write(path, contents).expect("write the file");
    }

    /// Write a global config at `path` flagging exactly `flagged`.
    fn write_record(path: &Path, flagged: &[&Path]) {
        let keys: Vec<&str> = flagged
            .iter()
            .map(|folder| folder.to_str().expect("a UTF-8 temp path"))
            .collect();
        write(path, &record(&keys).to_string());
    }

    /// (1.11 a) The folder is keyed by its REALPATH: a symlink to a trusted
    /// folder is trusted, and an entry for the symlink's own spelling trusts
    /// nothing.
    #[cfg(unix)]
    #[test]
    fn the_folder_is_keyed_by_its_realpath() {
        let root = temp_dir("realpath");
        let real = root.join("real");
        let link = root.join("link");
        fs::create_dir_all(&real).expect("create the real folder");
        std::os::unix::fs::symlink(&real, &link).expect("link to it");
        let cfg = root.join("claude.json");

        write_record(&cfg, &[&real]);
        assert_eq!(folder_trust_in(&cfg, &link), FolderTrust::Trusted);
        write_record(&cfg, &[&link]);
        assert_eq!(
            folder_trust_in(&cfg, &link),
            FolderTrust::Untrusted,
            "the symlink's spelling is not the key claude writes"
        );
        fs::remove_dir_all(&root).ok();
    }

    /// (1.11 b) The walk (clause (ii)) stops at the git root found on disk: a
    /// flagged folder above the repository never reaches into it, whether its
    /// `.git` is a directory or a file that is no valid pointer.
    #[cfg(unix)]
    #[test]
    fn the_parent_walk_stops_at_the_git_root_on_disk() {
        let root = temp_dir("git-root");
        let p = root.join("p");
        let repo = p.join("repo");
        let sub = repo.join("sub");
        fs::create_dir_all(repo.join(GIT_ENTRY)).expect("create the .git dir");
        fs::create_dir_all(&sub).expect("create the subfolder");
        let cfg = root.join("claude.json");

        write_record(&cfg, &[&p]);
        assert_eq!(folder_trust_in(&cfg, &sub), FolderTrust::Untrusted);

        fs::remove_dir_all(repo.join(GIT_ENTRY)).expect("drop the .git dir");
        write(&repo.join(GIT_ENTRY), "not a pointer\n");
        assert_eq!(
            folder_trust_in(&cfg, &sub),
            FolderTrust::Untrusted,
            "a .git file bounds the walk too"
        );

        write_record(&cfg, &[&p, &repo]);
        assert_eq!(folder_trust_in(&cfg, &sub), FolderTrust::Trusted);
        fs::remove_dir_all(&root).ok();
    }

    /// (1.11 c) A linked worktree synthesized from git's own pointer files (no
    /// `git` runs). It sits INSIDE its main checkout, as this repository's
    /// `.agents/worktrees` do, so the walk (clause (ii)) stops at the worktree
    /// and only the main checkout's key (clause (i)) can trust it: once every
    /// pointer agrees, and never through a tampered back-pointer or a symlinked
    /// `.git`.
    #[cfg(unix)]
    #[test]
    fn a_linked_worktree_inherits_its_main_checkouts_trust_on_disk() {
        let root = temp_dir("worktree");
        let main = root.join("m");
        let wt = main.join(".agents").join("wt");
        let sub = wt.join("sub");
        let gitdir = main.join(GIT_ENTRY).join(WORKTREES_DIR).join("wt");
        let back_pointer = gitdir.join(GITDIR_FILE);
        let dot_git = wt.join(GIT_ENTRY);
        let pointer = format!("{GITDIR_PREFIX} {}\n", gitdir.display());
        let registered = format!("{}\n", dot_git.display());
        fs::create_dir_all(&sub).expect("create the worktree's subfolder");
        write(&gitdir.join(COMMONDIR_FILE), "../..\n");
        write(&back_pointer, &registered);
        write(&dot_git, &pointer);
        let cfg = root.join("claude.json");
        write_record(&cfg, &[&main]);

        assert_eq!(folder_trust_in(&cfg, &sub), FolderTrust::Trusted);

        write(
            &back_pointer,
            &format!("{}\n", root.join("elsewhere").join(GIT_ENTRY).display()),
        );
        assert_eq!(
            folder_trust_in(&cfg, &sub),
            FolderTrust::Untrusted,
            "a back-pointer to another worktree"
        );
        write(&back_pointer, &registered);

        let elsewhere = root.join("pointer");
        write(&elsewhere, &pointer);
        fs::remove_file(&dot_git).expect("drop the .git file");
        std::os::unix::fs::symlink(&elsewhere, &dot_git).expect("link .git to the pointer");
        assert_eq!(
            folder_trust_in(&cfg, &sub),
            FolderTrust::Untrusted,
            "a symlinked .git"
        );
        fs::remove_dir_all(&root).ok();
    }

    /// (1.11 d) Anything unreadable answers untrusted: a missing record, a
    /// directory in its place, an empty, torn or garbage record, one behind a
    /// byte-order mark, and a folder that does not exist, though its parent is
    /// flagged.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_record_or_folder_is_untrusted() {
        let root = temp_dir("unreadable");
        let folder = root.join("folder");
        fs::create_dir_all(&folder).expect("create the folder");
        let good = root.join("good.json");
        write_record(&good, &[&folder, &root]);
        assert_eq!(
            folder_trust_in(&good, &folder),
            FolderTrust::Trusted,
            "control"
        );

        let valid = record(&[folder.to_str().expect("a UTF-8 temp path")]).to_string();
        let torn = &valid[..valid.len() - 1];
        let behind_bom = format!("\u{feff}{valid}");
        for (name, contents) in [
            ("empty.json", ""),
            ("torn.json", torn),
            ("garbage.json", "{ not json"),
            ("bom.json", behind_bom.as_str()),
        ] {
            let cfg = root.join(name);
            write(&cfg, contents);
            assert_eq!(
                folder_trust_in(&cfg, &folder),
                FolderTrust::Untrusted,
                "{name}"
            );
        }

        assert_eq!(
            folder_trust_in(&root.join("missing.json"), &folder),
            FolderTrust::Untrusted,
            "a missing record"
        );
        let directory = root.join("directory.json");
        fs::create_dir_all(&directory).expect("a directory in the record's place");
        assert_eq!(
            folder_trust_in(&directory, &folder),
            FolderTrust::Untrusted,
            "a directory as the record"
        );
        assert_eq!(
            folder_trust_in(&good, &root.join("missing")),
            FolderTrust::Untrusted,
            "a folder that cannot be canonicalised"
        );
        fs::remove_dir_all(&root).ok();
    }
}
