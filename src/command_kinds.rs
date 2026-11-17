//! Which kind each `/` row of the compose pick list is: a skill, a command, or a
//! built-in. claude's `initialize` reply names none of them (it flags only
//! `builtin`), so the kind of everything else is read off the folders claude
//! loads — what its loader does with them is `docs/agents/CLAUDE_CLI.md`'s
//! ("Which folder a `/` row came from", claude 2.1.306).
//!
//! The decision is one precedence, asked of a name: the reply's own `builtin`
//! flag (set by `claude_catalog`, never overridden here), then a skill, then a
//! command, else no label. SKILL FIRST, always: claude loads every skills folder
//! before every commands folder and keeps the first entry of a name, so a name
//! both folders define runs the skill. The one case that reads wrong is a name
//! defined by a `commands/` file this module reads AND by a skill in a folder it
//! does not (managed, synced): it shows `command` while claude runs the skill.
//! Plugin, managed and synced skills are not read, so their rows stay unlabelled.
//!
//! Pure core ([`claude_folders`], [`skill_name`], [`commands_in_dir`],
//! [`kind_of`], [`apply_kinds`], [`label_with`]) and a thin impure reader
//! ([`read_names`], [`label_commands`]). The reads are FAIL-SOFT, the same
//! discipline as every other file claude owns (AGENTS.md FAIL-SOFT parsing): a
//! missing or unreadable folder, a non-UTF-8 name, a bad frontmatter or a cap hit
//! only leaves a row unlabelled, never an error and never a guess. They are
//! blocking FS work, so [`label_commands`] is called on the catalog fetch's worker
//! thread alone, after the reply, with the verdict that fetch already holds
//! (AGENTS.md OFF-UI-THREAD); it lives outside `src/store/` and runs no `git`
//! (AGENTS.md PURE, GIT-FREE STORE CORE).

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::claude_settings;
use crate::claude_trust::{self, FolderTrust};
use crate::defined_agents::{parse_frontmatter, CLAUDE_DIR};
use crate::store::skills::{CommandKind, ListingEntry};

/// The folder under a `.claude` directory whose subfolders each hold a
/// `SKILL.md`.
const SKILLS_SUBDIR: &str = "skills";

/// The folder under a `.claude` directory whose Markdown files are commands.
const COMMANDS_SUBDIR: &str = "commands";

/// The file that makes a folder a skill (in `skills/`) or a command named by its
/// folder (in `commands/`).
const SKILL_FILE: &str = "SKILL.md";

/// The extension of a command file.
const COMMAND_EXTENSION: &str = "md";

/// What joins a command's folders and file name: `commands/a/b.md` is `a:b`.
const COMMAND_NAMESPACE_SEPARATOR: &str = ":";

/// The most directory entries one [`read_names`] call looks at, over every
/// folder together. Real folders hold tens of entries (the owner's holds ~100);
/// a synced or generated tree must not make the worker's read unbounded. Past it
/// the remaining entries are not read and their rows stay unlabelled.
const MAX_ENTRIES: usize = 5_000;

/// The deepest `commands/` subfolder [`read_names`] descends into. A namespace
/// is one or two folders in practice; the cap ends a symlink cycle, which the
/// follow-symlinks `metadata` calls would otherwise walk until [`MAX_ENTRIES`].
const MAX_COMMAND_DEPTH: usize = 8;

/// The most bytes of a `SKILL.md` read for its frontmatter. A frontmatter is a
/// handful of lines; one that does not close inside this reads as no frontmatter
/// and the skill is named by its folder.
const MAX_SKILL_HEAD_BYTES: u64 = 16 * 1024;

/// The names the readable folders define, one set per kind.
#[derive(Debug, Default, PartialEq, Eq)]
struct DefinedNames {
    skills: HashSet<String>,
    commands: HashSet<String>,
}

/// Name the commands in `entries` that carry no kind yet, from the folders claude
/// loads for `cwd`. `trust` is the verdict the SAME fetch's argv was built from:
/// project `.claude/` folders are read only under [`FolderTrust::Trusted`], so a
/// folder whose fetch skipped its project items is never labelled from them.
///
/// The ONE site naming the real environment: `$CLAUDE_CONFIG_DIR` through
/// `claude_settings`, the home directory and the `.git` stat. Blocking FS reads:
/// the catalog fetch's worker thread only.
pub fn label_commands(entries: &mut [ListingEntry], cwd: &Path, trust: FolderTrust) {
    label_with(
        entries,
        cwd,
        trust,
        claude_settings::user_config_dir().as_deref(),
        dirs::home_dir().as_deref(),
        claude_trust::has_git_entry,
    );
}

/// [`label_commands`] over stated locations, so the suite runs it against temp
/// folders: reads nothing when every entry already has a kind.
fn label_with(
    entries: &mut [ListingEntry],
    cwd: &Path,
    trust: FolderTrust,
    user_config_dir: Option<&Path>,
    home: Option<&Path>,
    has_git: impl Fn(&Path) -> bool,
) {
    if entries.iter().all(|entry| entry.kind.is_some()) {
        return;
    }
    let folders = claude_folders(user_config_dir, cwd, trust, home, has_git);
    apply_kinds(entries, &read_names(&folders));
}

/// The `.claude`-style folders claude loads skills and commands from for `cwd`:
/// the user's own `user_config_dir` first, then — only under
/// [`FolderTrust::Trusted`] — each project `.claude/` from `cwd` upward, ending
/// AT the first directory `has_git` says holds a `.git` entry, or just BELOW
/// `home` (the home directory's own `.claude` is the user's, never a project's);
/// outside a repository and outside `home` the walk ends at `/`. A relative
/// `cwd` names no project folder.
///
/// claude 2.1.306's walk, probed on 2026-10-21 (CLAUDE_CLI.md). Whether claude
/// stops at a linked worktree's own `.git` FILE was not probed: any `.git` entry
/// is the stop here.
fn claude_folders(
    user_config_dir: Option<&Path>,
    cwd: &Path,
    trust: FolderTrust,
    home: Option<&Path>,
    has_git: impl Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    let mut folders: Vec<PathBuf> = user_config_dir.map(Path::to_path_buf).into_iter().collect();
    if trust == FolderTrust::Trusted && cwd.is_absolute() {
        for dir in cwd.ancestors() {
            if Some(dir) == home {
                break;
            }
            folders.push(dir.join(CLAUDE_DIR));
            if has_git(dir) {
                break;
            }
        }
    }
    folders
}

/// The kind of the command `name`: [`CommandKind::Skill`] when a skill defines
/// it, else [`CommandKind::Command`] when a command does, else none. Skill FIRST
/// (the module doc's one wrong-label case is the price of the other order being
/// wrong for real clashes).
fn kind_of(name: &str, defined: &DefinedNames) -> Option<CommandKind> {
    if defined.skills.contains(name) {
        Some(CommandKind::Skill)
    } else if defined.commands.contains(name) {
        Some(CommandKind::Command)
    } else {
        None
    }
}

/// Give every entry without a kind the one [`kind_of`] finds. An entry that has
/// one — the reply's `builtin` — keeps it, whatever the folders define.
fn apply_kinds(entries: &mut [ListingEntry], defined: &DefinedNames) {
    for entry in entries.iter_mut().filter(|entry| entry.kind.is_none()) {
        entry.kind = kind_of(&entry.name, defined);
    }
}

/// The name a skill listed under `skills/<folder>/` goes by: the `name:` of its
/// `SKILL.md` frontmatter when it has a non-empty one, else `folder`. Reuses
/// `defined_agents`' hand-parsed frontmatter, which already falls back to the
/// stem it is given for a nameless block and fails soft to `None` for a missing
/// or unclosed one.
fn skill_name(folder: &str, skill_md: &str) -> String {
    parse_frontmatter(skill_md, folder).map_or_else(|| folder.to_owned(), |parsed| parsed.name)
}

/// The command names one `commands/` directory contributes, given its
/// `namespace` (the folders from `commands/` down to it), the stems of its `.md`
/// files and whether it holds a [`SKILL_FILE`]. A directory holding one names
/// THAT folder (`commands/pkg/SKILL.md` is `pkg`, none at the top) and drops its
/// other `.md` files; any other directory's file is its stem under the
/// namespace, `commands/a/b.md` is `a:b`.
fn commands_in_dir(namespace: &[String], stems: &[String], holds_skill: bool) -> Vec<String> {
    if holds_skill {
        return if namespace.is_empty() {
            Vec::new()
        } else {
            vec![namespace.join(COMMAND_NAMESPACE_SEPARATOR)]
        };
    }
    stems
        .iter()
        .map(|stem| {
            namespace
                .iter()
                .map(String::as_str)
                .chain(std::iter::once(stem.as_str()))
                .collect::<Vec<_>>()
                .join(COMMAND_NAMESPACE_SEPARATOR)
        })
        .collect()
}

/// Every name the `skills/` and `commands/` folders under each of `folders`
/// define, within one [`MAX_ENTRIES`] budget. Thin: the rules are
/// [`skill_name`] and [`commands_in_dir`]'s.
fn read_names(folders: &[PathBuf]) -> DefinedNames {
    let mut defined = DefinedNames::default();
    let mut budget = MAX_ENTRIES;
    for folder in folders {
        read_skills(
            &folder.join(SKILLS_SUBDIR),
            &mut budget,
            &mut defined.skills,
        );
        read_commands(
            &folder.join(COMMANDS_SUBDIR),
            &mut Vec::new(),
            &mut budget,
            &mut defined.commands,
        );
    }
    defined
}

/// The entries of `dir` by UTF-8 name, sorted so the budget cuts the same ones
/// every time, each counted against `budget` (an entry that cannot be counted is
/// not returned). A missing or unreadable `dir` is empty; so is a non-UTF-8 name.
fn entries_of(dir: &Path, budget: &mut usize) -> Vec<(String, PathBuf)> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<(String, PathBuf)> = read
        .flatten()
        .filter_map(|entry| Some((entry.file_name().into_string().ok()?, entry.path())))
        .collect();
    entries.sort();
    entries.truncate(*budget);
    *budget -= entries.len();
    entries
}

/// Add the name of every `skills/<folder>/SKILL.md` under `dir` to `out`. A
/// folder without a regular `SKILL.md` is no skill; one that cannot be read
/// contributes nothing.
fn read_skills(dir: &Path, budget: &mut usize, out: &mut HashSet<String>) {
    for (folder, path) in entries_of(dir, budget) {
        let skill_file = path.join(SKILL_FILE);
        if !is_dir(&path) || !is_file(&skill_file) {
            continue;
        }
        let mut head = Vec::new();
        let read = File::open(&skill_file)
            .and_then(|file| file.take(MAX_SKILL_HEAD_BYTES).read_to_end(&mut head));
        if read.is_ok() {
            out.insert(skill_name(&folder, &String::from_utf8_lossy(&head)));
        }
    }
}

/// Add the name of every command under `dir` to `out`, descending into
/// subfolders up to [`MAX_COMMAND_DEPTH`]; `namespace` is the folders walked so
/// far. Whether claude reads below a folder that holds a `SKILL.md` was not
/// probed: this reads it like any other.
fn read_commands(
    dir: &Path,
    namespace: &mut Vec<String>,
    budget: &mut usize,
    out: &mut HashSet<String>,
) {
    let mut stems = Vec::new();
    let mut subfolders = Vec::new();
    let mut holds_skill = false;
    for (name, path) in entries_of(dir, budget) {
        if is_dir(&path) {
            subfolders.push((name, path));
        } else if is_file(&path) {
            if name == SKILL_FILE {
                holds_skill = true;
            } else if let Some(stem) = name.strip_suffix(&format!(".{COMMAND_EXTENSION}")) {
                stems.push(stem.to_owned());
            }
        }
    }
    out.extend(commands_in_dir(namespace, &stems, holds_skill));
    if namespace.len() >= MAX_COMMAND_DEPTH {
        return;
    }
    for (name, path) in subfolders {
        namespace.push(name);
        read_commands(&path, namespace, budget, out);
        namespace.pop();
    }
}

/// Whether `path` is a directory, symlinks followed (claude follows them).
fn is_dir(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_dir())
}

/// Whether `path` is a regular file, symlinks followed. A FIFO or a device is
/// neither, so it is never opened (a read of one could block the worker).
fn is_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defined(skills: &[&str], commands: &[&str]) -> DefinedNames {
        DefinedNames {
            skills: skills.iter().map(|s| (*s).to_owned()).collect(),
            commands: commands.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn entry(name: &str, kind: Option<CommandKind>) -> ListingEntry {
        ListingEntry {
            name: name.to_owned(),
            description: None,
            kind,
        }
    }

    fn strings(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    /// A unique temp dir for one test; never a real `~/.claude`.
    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let base = fs::canonicalize(std::env::temp_dir()).expect("canonicalise the temp dir");
        let dir = base.join(format!(
            "snapback-command-kinds-{tag}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create the test's temp dir");
        dir
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().expect("a file path has a parent"))
            .expect("create the parent dir");
        fs::write(path, contents).expect("write the file");
    }

    fn kinds(entries: &[ListingEntry]) -> Vec<Option<CommandKind>> {
        entries.iter().map(|e| e.kind).collect()
    }

    #[test]
    fn a_skill_outranks_a_command_of_the_same_name() {
        let both = defined(&["clash"], &["clash", "only-command"]);
        assert_eq!(kind_of("clash", &both), Some(CommandKind::Skill));
        assert_eq!(kind_of("only-command", &both), Some(CommandKind::Command));
        assert_eq!(kind_of("neither", &both), None);
    }

    #[test]
    fn a_built_in_flag_outranks_both_folders() {
        let mut entries = [
            entry("loop", Some(CommandKind::BuiltIn)),
            entry("clash", None),
            entry("plain", None),
        ];
        apply_kinds(
            &mut entries,
            &defined(&["loop", "clash"], &["clash", "loop"]),
        );
        assert_eq!(
            kinds(&entries),
            [Some(CommandKind::BuiltIn), Some(CommandKind::Skill), None]
        );
    }

    #[test]
    fn a_frontmatter_name_overrides_the_folder_name() {
        assert_eq!(
            skill_name("dir-x", "---\nname: other-y\n---\nbody"),
            "other-y"
        );
        assert_eq!(skill_name("dir-x", "---\nname: 'quoted'\n---\n"), "quoted");
    }

    #[test]
    fn a_skill_without_a_usable_frontmatter_name_goes_by_its_folder() {
        for text in [
            "no frontmatter at all",
            "---\ndescription: nameless\n---\n",
            "---\nname:\n---\n",
            "---\nname: never closed\n",
            "",
        ] {
            assert_eq!(skill_name("dir-x", text), "dir-x", "{text:?}");
        }
    }

    #[test]
    fn a_nested_command_file_is_named_by_its_folders() {
        assert_eq!(
            commands_in_dir(&strings(&["a"]), &strings(&["b"]), false),
            ["a:b"]
        );
        assert_eq!(
            commands_in_dir(&strings(&["a", "b"]), &strings(&["c", "d"]), false),
            ["a:b:c", "a:b:d"]
        );
        assert_eq!(commands_in_dir(&[], &strings(&["top"]), false), ["top"]);
    }

    #[test]
    fn a_skill_file_in_commands_names_its_folder_and_hides_its_siblings() {
        assert_eq!(
            commands_in_dir(&strings(&["pkg"]), &strings(&["sibling", "other"]), true),
            ["pkg"]
        );
        assert_eq!(
            commands_in_dir(&strings(&["a", "pkg"]), &[], true),
            ["a:pkg"]
        );
        assert!(commands_in_dir(&[], &strings(&["x"]), true).is_empty());
    }

    #[test]
    fn an_untrusted_verdict_names_no_project_folder() {
        let user = PathBuf::from("/u/.claude");
        let cwd = Path::new("/home/me/proj/sub");
        let home = Some(Path::new("/home/me"));
        assert_eq!(
            claude_folders(Some(&user), cwd, FolderTrust::Untrusted, home, |_| true),
            std::slice::from_ref(&user)
        );
        assert_eq!(
            claude_folders(Some(&user), cwd, FolderTrust::Trusted, home, |d| d
                == Path::new("/home/me/proj")),
            [
                user,
                PathBuf::from("/home/me/proj/sub/.claude"),
                PathBuf::from("/home/me/proj/.claude")
            ]
        );
    }

    #[test]
    fn the_walk_stops_at_a_git_entry_and_below_home() {
        let git_root = Path::new("/home/me/proj");
        let from = |cwd: &str, home: Option<&Path>, has_git: &dyn Fn(&Path) -> bool| {
            claude_folders(None, Path::new(cwd), FolderTrust::Trusted, home, has_git)
        };
        let home = Some(Path::new("/home/me"));

        // The git root is included and nothing above it is.
        assert_eq!(
            from("/home/me/proj/a/b", home, &|d| d == git_root),
            [
                PathBuf::from("/home/me/proj/a/b/.claude"),
                PathBuf::from("/home/me/proj/a/.claude"),
                PathBuf::from("/home/me/proj/.claude"),
            ]
        );
        // The git stop holds without a home to bound the walk too.
        assert_eq!(
            from("/srv/repo/a", None, &|d| d == Path::new("/srv/repo")),
            [
                PathBuf::from("/srv/repo/a/.claude"),
                PathBuf::from("/srv/repo/.claude"),
            ]
        );
        // No git entry: up to but excluding home, never `~/.claude` as a project.
        assert_eq!(
            from("/home/me/x/y", home, &|_| false),
            [
                PathBuf::from("/home/me/x/y/.claude"),
                PathBuf::from("/home/me/x/.claude"),
            ]
        );
        // A git root ABOVE home never gets reached.
        assert_eq!(
            from("/home/me/x", home, &|d| d == Path::new("/home")),
            [PathBuf::from("/home/me/x/.claude")]
        );
        // Standing in home itself names no project folder.
        assert!(from("/home/me", home, &|_| false).is_empty());
        // Outside home and any repository the walk runs to `/`.
        assert_eq!(
            from("/srv/app", home, &|_| false),
            [
                PathBuf::from("/srv/app/.claude"),
                PathBuf::from("/srv/.claude"),
                PathBuf::from("/.claude"),
            ]
        );
        // A relative folder names none (it would be read against the process's).
        assert!(from("rel/dir", home, &|_| false).is_empty());
    }

    #[test]
    fn a_git_entry_on_disk_stops_the_real_walk() {
        let root = temp_dir("git-stop");
        let repo = root.join("repo");
        let sub = repo.join("sub");
        fs::create_dir_all(&sub).expect("create the folders");
        fs::write(repo.join(".git"), "gitdir: elsewhere\n").expect("a worktree's .git FILE");
        let folders = claude_folders(
            None,
            &sub,
            FolderTrust::Trusted,
            None,
            claude_trust::has_git_entry,
        );
        assert_eq!(folders, [sub.join(".claude"), repo.join(".claude")]);
        fs::remove_dir_all(&root).ok();
    }

    /// The tree every reader test uses: user skills and commands, and project
    /// ones in a git repository.
    fn tree(root: &Path) -> (PathBuf, PathBuf) {
        let user = root.join("user-claude");
        let repo = root.join("repo");
        let cwd = repo.join("sub");
        fs::create_dir_all(&cwd).expect("create the folders");
        fs::create_dir_all(repo.join(".git")).expect("a git root");
        write(&user.join("skills/plain/SKILL.md"), "# no frontmatter");
        write(
            &user.join("skills/dir-x/SKILL.md"),
            "---\nname: other-y\n---\n",
        );
        write(&user.join("skills/no-skill-file/notes.md"), "x");
        write(&user.join("commands/top.md"), "x");
        write(&user.join("commands/a/b.md"), "x");
        write(&user.join("commands/pkg/SKILL.md"), "x");
        write(&user.join("commands/pkg/sibling.md"), "x");
        write(&user.join("commands/clash.md"), "x");
        write(&user.join("skills/clash/SKILL.md"), "x");
        write(&repo.join(".claude/skills/proj-skill/SKILL.md"), "x");
        write(&repo.join(".claude/commands/proj-cmd.md"), "x");
        write(&cwd.join(".claude/commands/inner.md"), "x");
        (user, cwd)
    }

    #[test]
    fn the_reader_names_what_the_folders_define() {
        let root = temp_dir("reader");
        let (user, cwd) = tree(&root);
        let folders = claude_folders(
            Some(&user),
            &cwd,
            FolderTrust::Trusted,
            None,
            claude_trust::has_git_entry,
        );
        let names = read_names(&folders);
        assert_eq!(
            names,
            defined(
                &["plain", "other-y", "clash", "proj-skill"],
                &["top", "a:b", "pkg", "clash", "proj-cmd", "inner"]
            )
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_untrusted_folder_gets_no_label_from_its_project_folders() {
        let root = temp_dir("untrusted");
        let (user, cwd) = tree(&root);
        let labelled = |trust| {
            let mut entries = [
                entry("proj-skill", None),
                entry("proj-cmd", None),
                entry("plain", None),
            ];
            label_with(
                &mut entries,
                &cwd,
                trust,
                Some(&user),
                None,
                claude_trust::has_git_entry,
            );
            kinds(&entries)
        };
        assert_eq!(
            labelled(FolderTrust::Untrusted),
            [None, None, Some(CommandKind::Skill)],
            "only the user's own folder is read"
        );
        assert_eq!(
            labelled(FolderTrust::Trusted),
            [
                Some(CommandKind::Skill),
                Some(CommandKind::Command),
                Some(CommandKind::Skill)
            ]
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_name_both_folders_define_reads_skill() {
        let root = temp_dir("clash");
        let (user, cwd) = tree(&root);
        let mut entries = [entry("clash", None)];
        label_with(
            &mut entries,
            &cwd,
            FolderTrust::Untrusted,
            Some(&user),
            None,
            claude_trust::has_git_entry,
        );
        assert_eq!(kinds(&entries), [Some(CommandKind::Skill)]);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_missing_or_unreadable_folder_leaves_the_row_unlabelled() {
        let root = temp_dir("missing");
        let mut entries = [entry("anything", None)];
        label_with(
            &mut entries,
            &root.join("not-there"),
            FolderTrust::Trusted,
            Some(&root.join("no-config")),
            None,
            claude_trust::has_git_entry,
        );
        assert_eq!(kinds(&entries), [None]);
        // A FILE where a folder belongs, and a skills folder that is a file.
        write(&root.join("cfg/skills"), "not a folder");
        write(&root.join("cfg/commands/ok.md"), "x");
        assert_eq!(
            read_names(&[root.join("cfg")]),
            defined(&[], &["ok"]),
            "the bad entry costs only itself"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_skill_file_that_is_a_directory_is_no_skill() {
        let root = temp_dir("not-regular");
        // `SKILL.md` as a directory: `File::open` would succeed on it, a read fail.
        fs::create_dir_all(root.join("cfg/skills/weird/SKILL.md")).expect("a SKILL.md dir");
        write(&root.join("cfg/skills/fine/SKILL.md"), "x");
        assert_eq!(read_names(&[root.join("cfg")]), defined(&["fine"], &[]));
        fs::remove_dir_all(&root).ok();
    }

    /// A FIFO named `SKILL.md` is never opened: opening one blocks until a
    /// writer appears, which would hang the fetch's worker.
    #[cfg(unix)]
    #[test]
    fn a_fifo_named_skill_md_is_never_opened() {
        let root = temp_dir("fifo");
        let skill = root.join("cfg/skills/pipe");
        fs::create_dir_all(&skill).expect("create the skill folder");
        let made = std::process::Command::new("mkfifo")
            .arg(skill.join(SKILL_FILE))
            .status()
            .is_ok_and(|status| status.success());
        assert!(made, "premise: mkfifo made the FIFO");
        write(&root.join("cfg/skills/fine/SKILL.md"), "x");
        assert_eq!(read_names(&[root.join("cfg")]), defined(&["fine"], &[]));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_entry_budget_bounds_the_read() {
        let root = temp_dir("budget");
        for n in 0..5 {
            write(&root.join(format!("cfg/commands/c{n}.md")), "x");
        }
        let mut out = HashSet::new();
        let mut budget = 3;
        read_commands(
            &root.join("cfg/commands"),
            &mut Vec::new(),
            &mut budget,
            &mut out,
        );
        assert_eq!(budget, 0);
        assert_eq!(out.len(), 3, "the entries past the budget are not read");
        fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_cycle_in_commands_ends_at_the_depth_cap() {
        let root = temp_dir("cycle");
        let commands = root.join("cfg/commands");
        fs::create_dir_all(&commands).expect("create commands");
        write(&commands.join("x.md"), "x");
        std::os::unix::fs::symlink(&commands, commands.join("loop")).expect("a cycle");
        let names = read_names(&[root.join("cfg")]);
        assert!(names.commands.contains("x"));
        assert!(names.commands.contains("loop:x"));
        assert!(
            names.commands.len() <= MAX_COMMAND_DEPTH + 1,
            "one name per depth at most: {:?}",
            names.commands
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn entries_that_already_have_a_kind_cost_no_read() {
        let mut entries = [entry("loop", Some(CommandKind::BuiltIn))];
        // A user folder that does not exist and a verdict that would read it:
        // nothing observable, and the kind stays.
        label_with(
            &mut entries,
            Path::new("/nowhere"),
            FolderTrust::Trusted,
            None,
            None,
            |_| panic!("no entry needs a folder, so no stat is made"),
        );
        assert_eq!(kinds(&entries), [Some(CommandKind::BuiltIn)]);
    }
}
