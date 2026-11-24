//! The IDE that owns snapback's terminal, and the command that opens a file in it.
//!
//! A click on a file link in the preview (`store::target`) opens the file in the
//! IDE snapback is RUNNING INSIDE, derived from snapback's own process ancestry:
//! the first ancestor that is an IDE program names the IDE, and the launcher that
//! hands it a file is read off that ancestor. No "frontmost IDE", no "IDE that
//! has the project open", no `$VISUAL` / `$EDITOR`, no `open` / `xdg-open`: outside
//! an IDE a click refuses and says so ([`Refusal::NoIde`]).
//!
//! The split is the project's usual one. [`choose`] is PURE: the ancestors'
//! executable paths and a "does this file exist" question in, an [`Ide`] or
//! `None` out. [`detect`] is the thin impure driver that walks the process tree
//! (`proc_pidinfo` / `proc_pidpath` on macOS, `/proc/<pid>/stat` and
//! `/proc/<pid>/exe` on Linux) and runs on a thread of its own, ONCE per process,
//! delivering one [`AppEvent::IdeDetected`]. The open itself ([`spawn_open`]) is
//! a second one-shot worker delivering one [`AppEvent::FileOpened`].
//!
//! EXTERNAL INPUT, so FAIL-SOFT: a process that cannot be read, a path that is not
//! UTF-8, a layout that matches nothing, all end as "no IDE" and never a panic.
//! No environment variable picks an IDE: a `tmux` server keeps the environment it
//! was STARTED in, not that of the terminal attached to it, so an environment hint
//! could open an IDE the user is not looking at. A multiplexer, or any terminal
//! whose parent processes include no IDE, is "no IDE", never a guessed launcher.
//! Mechanism and rationale: `docs/agents/DOMAIN.md`, "File links open in the host
//! IDE".

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use crate::watch::AppEvent;

/// The most ancestors the walk follows: a real shell sits a handful of processes
/// below its IDE, so this only bounds a pathological or cyclic table.
const MAX_ANCESTORS: usize = 64;

/// How long the launcher may run before it is killed and reported.
///
/// A launcher handing a file to a RUNNING IDE returns at once (measured: JetBrains'
/// `webstorm --line N <file>` and VS Code Insiders' `code -r -g <file>:N` both
/// exited 0 against the running instance without starting another). One that does
/// not return in this long is not delivering a file, and the worker must end rather
/// than leave a click that never reports.
const LAUNCHER_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the worker asks a running launcher whether it has exited.
const LAUNCHER_POLL: Duration = Duration::from_millis(25);

/// The word every refusal opens with, so the file refusals and the link refusals
/// (`tui::update::LINK_REFUSED_PREFIX`, which is this) read as one family.
pub const REFUSED_PREFIX: &str = "not opening ";

/// What a successful open says, then the IDE's name.
const OPENED_PREFIX: &str = "opened in ";

/// The character that makes a path unsafe to hand a VS Code-family or Zed
/// launcher with a line: VS Code's `-g` parser (`doResolveFilePath`, measured in
/// VS Code Insiders' `mainImpl.js`) splits on every one and reads an all-digit
/// segment as the line or column, with no fallback to the raw path.
const PATH_COLON: char = ':';

/// What a click says while the ancestry walk has not answered yet.
///
/// A nudge, not a refusal: nothing is wrong, the answer is a moment away, and a
/// click made in that moment is not a decision about the file.
pub const DETECTING_NUDGE: &str =
    "still looking for the IDE that owns this terminal - click again in a moment";

/// The IDE launchers JetBrains ships, by product executable name: the macOS
/// `Contents/MacOS/<name>` binary, and the `bin/<name>.sh` script beside the
/// bundled runtime on Linux. `studio` is Android Studio, built on the same
/// platform and launcher.
const JETBRAINS_LAUNCHERS: &[&str] = &[
    "idea",
    "webstorm",
    "pycharm",
    "phpstorm",
    "rubymine",
    "goland",
    "clion",
    "rustrover",
    "datagrip",
    "rider",
    "dataspell",
    "aqua",
    "studio",
];

/// The command-line launchers of the VS Code family, by the name each ships its
/// `bin/` script under: VS Code (Insiders included, whose script is still `code`,
/// measured on its bundle), Cursor, Windsurf, VSCodium. All take `-r -g`.
const VSCODE_LAUNCHERS: &[&str] = &[
    "code",
    "code-insiders",
    "code-oss",
    "cursor",
    "windsurf",
    "codium",
    "vscodium",
];

/// Where inside a macOS app bundle the VS Code family keeps its launcher script.
const VSCODE_BUNDLE_BIN: &str = "Contents/Resources/app/bin";

/// Where inside a macOS app bundle Zed keeps its main binary and its CLI.
const ZED_BUNDLE_MACOS: &str = "Contents/MacOS";

/// Zed's CLI binary inside [`ZED_BUNDLE_MACOS`].
const ZED_BUNDLE_CLI: &str = "cli";

/// Which launcher convention an [`Ide`] speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// `<launcher> --line N <file>`.
    JetBrains,
    /// `<launcher> -r -g <file>:N`, or `-r <file>` for a path holding `:`.
    VsCode,
    /// `<launcher> <file>:N`, or `<file>` for a path holding `:`.
    Zed,
}

/// The IDE that owns snapback's terminal: its name for the status line, which
/// convention its launcher speaks, and the launcher itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ide {
    /// What the status line calls it: the app bundle's name on macOS
    /// (`WebStorm 2026.3`), the product's launcher name elsewhere.
    pub name: String,
    /// The launcher's argument convention.
    pub family: Family,
    /// The launcher to run: an absolute path read off the ancestry, never looked
    /// up by name on `PATH`.
    pub launcher: PathBuf,
}

impl Ide {
    /// The line the launcher is actually handed for `file`: `line` itself, except
    /// for the families whose own `<file>:N` parser splits on EVERY `:` and so
    /// reads a `:` inside the path (`docs/plans/2026-11-06T05:42:19Z-x/A.md`) as a
    /// line or column and opens another file ([`PATH_COLON`]): those get no line,
    /// the right file at line 1. JetBrains takes the line as its own argument.
    pub fn passed_line(&self, file: &Path, line: Option<u32>) -> Option<u32> {
        match self.family {
            Family::JetBrains => line,
            Family::VsCode | Family::Zed => line.filter(|_| !holds_path_colon(file)),
        }
    }

    /// The arguments that open `file`, at `line` when one is given and
    /// [`Ide::passed_line`] lets it through.
    ///
    /// `file` is ABSOLUTE by construction ([`pick_file`]), so it can never read as
    /// a flag or as a JetBrains sub-command; the launcher is run with no shell.
    pub fn args(&self, file: &Path, line: Option<u32>) -> Vec<OsString> {
        let line = self.passed_line(file, line);
        let with_line = |sep: &str| {
            let mut spec = file.as_os_str().to_os_string();
            if let Some(line) = line {
                spec.push(format!("{sep}{line}"));
            }
            spec
        };
        match self.family {
            Family::JetBrains => {
                let mut args: Vec<OsString> = Vec::new();
                if let Some(line) = line {
                    args.push("--line".into());
                    args.push(line.to_string().into());
                }
                args.push(file.as_os_str().to_os_string());
                args
            }
            // `-g` is what makes the launcher parse the `:` suffix, so a path
            // holding one opens WITHOUT it: the launcher then takes it verbatim.
            Family::VsCode if holds_path_colon(file) => vec!["-r".into(), with_line(":")],
            Family::VsCode => vec!["-r".into(), "-g".into(), with_line(":")],
            Family::Zed => vec![with_line(":")],
        }
    }
}

/// Whether `file` holds a `:` anywhere ([`PATH_COLON`]).
fn holds_path_colon(file: &Path) -> bool {
    // `:` is ASCII, so a lossy view cannot hide or invent one.
    file.to_string_lossy().contains(PATH_COLON)
}

/// The status of a successful open of `file` in `ide` at `line`: names the line
/// only when one was asked for and [`Ide::passed_line`] withheld it.
fn opened_status(ide: &Ide, file: &Path, line: Option<u32>) -> String {
    match line.filter(|_| ide.passed_line(file, line).is_none()) {
        Some(dropped) => format!(
            "{OPENED_PREFIX}{} - line {dropped} not passed, the path holds '{PATH_COLON}'",
            ide.name
        ),
        None => format!("{OPENED_PREFIX}{}", ide.name),
    }
}

/// What the board knows about the host IDE: not yet, found, or none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdeState {
    /// The detection thread has not answered yet.
    Detecting,
    /// snapback runs inside this IDE.
    Found(Ide),
    /// snapback does not run inside an IDE this module knows.
    Absent,
}

/// Pick the IDE out of snapback's ancestry, nearest ancestor first, or `None`.
///
/// `ancestors` are the executables of the parent, grandparent and so on. The
/// FIRST that is an IDE wins: the nearest owner of the terminal is the IDE the
/// user is looking at. `exists` answers whether a file is there, so the layouts
/// are asserted without a disk.
pub fn choose(ancestors: &[PathBuf], exists: &dyn Fn(&Path) -> bool) -> Option<Ide> {
    ancestors.iter().find_map(|exe| classify(exe, exists))
}

/// Read one executable path as an IDE, or `None` for any other program.
fn classify(exe: &Path, exists: &dyn Fn(&Path) -> bool) -> Option<Ide> {
    let name = exe.file_name()?.to_str()?;
    let lower = name.to_ascii_lowercase();
    match outer_bundle(exe) {
        Some(bundle) => classify_in_bundle(exe, &bundle, &lower, exists),
        None => classify_unbundled(exe, &lower, exists),
    }
}

/// The macOS layouts, where every IDE is an app bundle: the OUTERMOST `.app` in
/// the path, so a VS Code helper nested in its `Contents/Frameworks` still names
/// the bundle that holds the launcher.
fn classify_in_bundle(
    exe: &Path,
    bundle: &Path,
    lower_name: &str,
    exists: &dyn Fn(&Path) -> bool,
) -> Option<Ide> {
    let label = bundle.file_stem()?.to_str()?.to_string();
    let macos = bundle.join(ZED_BUNDLE_MACOS);
    // JetBrains: the product's own binary IS the launcher, and a running
    // instance takes the file (measured on WebStorm and RustRover). Only the
    // main binary, so a helper in the same bundle is not mistaken for it.
    if JETBRAINS_LAUNCHERS.contains(&lower_name) && exe == macos.join(exe.file_name()?) {
        return Some(Ide {
            name: label,
            family: Family::JetBrains,
            launcher: exe.to_path_buf(),
        });
    }
    if lower_name == "zed" && exe == macos.join(exe.file_name()?) {
        let cli = macos.join(ZED_BUNDLE_CLI);
        return exists(&cli).then_some(Ide {
            name: label,
            family: Family::Zed,
            launcher: cli,
        });
    }
    VSCODE_LAUNCHERS
        .iter()
        .map(|n| bundle.join(VSCODE_BUNDLE_BIN).join(n))
        .find(|p| exists(p))
        .map(|launcher| Ide {
            name: label,
            family: Family::VsCode,
            launcher,
        })
}

/// The Linux layouts, where an IDE is a directory: JetBrains' process is its
/// bundled runtime (`<home>/jbr/bin/java`) with the launcher script in
/// `<home>/bin`; VS Code's is `<dir>/<name>` beside `<dir>/bin/<name>`; Zed's is
/// `<dir>/libexec/zed-editor` beside `<dir>/bin/zed`.
fn classify_unbundled(exe: &Path, lower_name: &str, exists: &dyn Fn(&Path) -> bool) -> Option<Ide> {
    if lower_name == "java" {
        let jbr = exe
            .ancestors()
            .find(|a| a.file_name().is_some_and(|n| n == "jbr"))?;
        let home = jbr.parent()?;
        return JETBRAINS_LAUNCHERS
            .iter()
            .map(|n| (*n, home.join("bin").join(format!("{n}.sh"))))
            .find(|(_, script)| exists(script))
            .map(|(n, launcher)| Ide {
                name: n.to_string(),
                family: Family::JetBrains,
                launcher,
            });
    }
    let dir = exe.parent()?;
    if VSCODE_LAUNCHERS.contains(&lower_name) {
        let launcher = dir.join("bin").join(exe.file_name()?);
        return exists(&launcher).then_some(Ide {
            name: lower_name.to_string(),
            family: Family::VsCode,
            launcher,
        });
    }
    if matches!(lower_name, "zed" | "zed-editor") {
        let launcher = dir.parent()?.join("bin").join("zed");
        return exists(&launcher).then_some(Ide {
            name: "Zed".to_string(),
            family: Family::Zed,
            launcher,
        });
    }
    None
}

/// The path up to and including its OUTERMOST macOS app bundle: a `*.app`
/// component that holds a `Contents` directory (so Zed's Linux `zed.app` install
/// dir, which has none, is not one).
fn outer_bundle(exe: &Path) -> Option<PathBuf> {
    let mut bundle = PathBuf::new();
    let mut components = exe.components().peekable();
    while let Some(component) = components.next() {
        bundle.push(component);
        let is_app = component
            .as_os_str()
            .to_str()
            .is_some_and(|c| c.len() > ".app".len() && c.ends_with(".app"));
        if is_app
            && components
                .peek()
                .is_some_and(|next| next.as_os_str() == "Contents")
        {
            return Some(bundle);
        }
    }
    None
}

/// The IDE this process runs inside, found ONCE per process and remembered, or
/// `None`. Walks the process tree, so it runs on a
/// thread of its own ([`spawn_detect`]), never on a key or a frame.
pub fn detect() -> Option<Ide> {
    static DETECTED: OnceLock<Option<Ide>> = OnceLock::new();
    DETECTED
        .get_or_init(|| choose(&ancestor_exes(), &|p| p.is_file()))
        .clone()
}

/// Deliver [`detect`]'s answer as ONE [`AppEvent::IdeDetected`] from a thread of
/// its own — a one-shot in `watch::spawn_model_alias_thread`'s shape, whose
/// reasoning (no loop, no flag, a failed send is a dropped result) applies as is.
/// Not an entry on AGENTS.md's list of UI-thread exceptions: it runs off the UI
/// thread and reports with an event, which is that rule's ordinary case.
pub fn spawn_detect(tx: Sender<AppEvent>) {
    thread::spawn(move || {
        let _ = tx.send(AppEvent::IdeDetected(detect()));
    });
}

/// The executables of this process's ancestors, parent first. A process whose
/// path cannot be read is skipped (its parents are still followed); a parent that
/// cannot be read ends the walk.
fn ancestor_exes() -> Vec<PathBuf> {
    let mut exes = Vec::new();
    let mut seen: HashSet<i32> = HashSet::new();
    let Ok(own) = i32::try_from(std::process::id()) else {
        return exes;
    };
    let mut next = parent_of(own);
    while let Some(pid) = next.filter(|&p| p > 1) {
        if exes.len() >= MAX_ANCESTORS || !seen.insert(pid) {
            break;
        }
        if let Some(exe) = exe_of(pid) {
            exes.push(exe);
        }
        next = parent_of(pid);
    }
    exes
}

/// `pid`'s parent, from `proc_pidinfo(PROC_PIDTBSDINFO)` — the call
/// `agents::kernel_says_gone` already makes, with the same buffer discipline.
#[cfg(target_os = "macos")]
fn parent_of(pid: i32) -> Option<i32> {
    let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    // SAFETY: `proc_pidinfo` writes at most `size` bytes into `info`, a buffer of
    // exactly `size` bytes that lives across the call, and keeps no pointer to it.
    // `info` is read only after the call returned exactly `size` bytes, which is
    // libproc's answer that the whole struct was filled.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast::<libc::c_void>(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: the call above filled the whole struct (`written == size`).
    let info = unsafe { info.assume_init() };
    i32::try_from(info.pbi_ppid).ok()
}

/// `pid`'s executable path, from `proc_pidpath`.
#[cfg(target_os = "macos")]
fn exe_of(pid: i32) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let cap = u32::try_from(buf.len()).ok()?;
    // SAFETY: `proc_pidpath` writes at most `cap` bytes into `buf`, a buffer of
    // exactly `cap` bytes that outlives the call, and keeps no pointer to it.
    let written = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast::<libc::c_void>(), cap) };
    let len = usize::try_from(written).ok().filter(|&n| n > 0)?;
    buf.truncate(len);
    Some(PathBuf::from(OsString::from_vec(buf)))
}

/// `pid`'s parent, from `/proc/<pid>/stat` ([`ppid_from_stat`]).
#[cfg(target_os = "linux")]
fn parent_of(pid: i32) -> Option<i32> {
    ppid_from_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// `pid`'s executable path, from the `/proc/<pid>/exe` link.
#[cfg(target_os = "linux")]
fn exe_of(pid: i32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

/// No ancestry walk on this platform, so no IDE is ever found.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn parent_of(_pid: i32) -> Option<i32> {
    None
}

/// See [`parent_of`]'s fallback.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn exe_of(_pid: i32) -> Option<PathBuf> {
    None
}

/// The parent pid in a `/proc/<pid>/stat` line: the second field after the
/// `(comm)`, which may itself hold spaces and parentheses, so it is found from the
/// LAST `)`. Compiled under test everywhere so the rule is pinned off Linux too.
#[cfg(any(target_os = "linux", test))]
fn ppid_from_stat(stat: &str) -> Option<i32> {
    let after_comm = &stat[stat.rfind(')')? + 1..];
    after_comm.split_whitespace().nth(1)?.parse().ok()
}

/// Why a click did not open its file. Each reason has its own words, so "no IDE
/// here", "the file is gone" and "the launcher failed" cannot be mistaken for one
/// another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// snapback does not run inside an IDE it knows.
    NoIde,
    /// A relative path with no absolute folder to resolve against.
    NoFolder,
    /// No candidate path exists.
    Missing,
    /// The path exists and is not a regular file (a folder, a socket).
    NotRegular,
    /// The launcher could not be started (the OS error).
    LauncherStart(String),
    /// The launcher ran and exited unsuccessfully (its status).
    LauncherExit(String),
    /// The launcher did not finish within [`LAUNCHER_TIMEOUT`].
    LauncherTimeout,
}

impl Refusal {
    /// The status line for a refusal about `path` — the full path, so the reader
    /// can open it by hand.
    pub fn status(&self, path: &Path) -> String {
        let reason = match self {
            Refusal::NoIde => "snapback is not running inside an IDE terminal".to_string(),
            Refusal::NoFolder => "no folder to resolve it against".to_string(),
            Refusal::Missing => "no such file".to_string(),
            Refusal::NotRegular => "not a regular file".to_string(),
            Refusal::LauncherStart(err) => format!("could not start the IDE launcher: {err}"),
            Refusal::LauncherExit(status) => format!("the IDE launcher exited with {status}"),
            Refusal::LauncherTimeout => format!(
                "the IDE launcher did not finish within {} s",
                LAUNCHER_TIMEOUT.as_secs()
            ),
        };
        format!("{REFUSED_PREFIX}{} - {reason}", path.display())
    }
}

/// What a worker needs to open one file: the IDE, the absolute paths the click
/// can mean (in the order they are tried), and the line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenRequest {
    /// The IDE to hand the file to.
    pub ide: Ide,
    /// The candidate absolute paths, never empty (a click with none is refused
    /// on the spot, [`Refusal::NoFolder`]).
    pub candidates: Vec<PathBuf>,
    /// The line to open at.
    pub line: Option<u32>,
}

/// The first candidate that is a regular file, or why none is: the first that
/// exists without being one ([`Refusal::NotRegular`]), else [`Refusal::Missing`]
/// — each with the path to name. The ONLY thing that decides a path may reach the
/// launcher, and it admits an existing, REGULAR file alone.
pub fn pick_file(candidates: &[PathBuf]) -> Result<PathBuf, (Refusal, PathBuf)> {
    let mut not_regular: Option<&PathBuf> = None;
    for candidate in candidates {
        match std::fs::metadata(candidate) {
            Ok(meta) if meta.is_file() => return Ok(candidate.clone()),
            Ok(_) => {
                not_regular.get_or_insert(candidate);
            }
            Err(_) => {}
        }
    }
    Err(match (not_regular, candidates.first()) {
        (Some(path), _) => (Refusal::NotRegular, path.clone()),
        (None, Some(first)) => (Refusal::Missing, first.clone()),
        (None, None) => (Refusal::Missing, PathBuf::new()),
    })
}

/// Open the request's file, returning the status and whether it is a success.
/// Blocking (a child), so only [`spawn_open`]'s thread calls it.
pub fn open(req: &OpenRequest) -> (String, bool) {
    let file = match pick_file(&req.candidates) {
        Ok(file) => file,
        Err((refusal, path)) => return (refusal.status(&path), false),
    };
    match run_launcher(&req.ide, &file, req.line) {
        Ok(()) => (opened_status(&req.ide, &file, req.line), true),
        Err(refusal) => (refusal.status(&file), false),
    }
}

/// Run the launcher with no shell, no stdin and no output, and wait for it up to
/// [`LAUNCHER_TIMEOUT`].
fn run_launcher(ide: &Ide, file: &Path, line: Option<u32>) -> Result<(), Refusal> {
    let mut child = Command::new(&ide.launcher)
        .args(ide.args(file, line))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Refusal::LauncherStart(e.to_string()))?;
    let deadline = Instant::now() + LAUNCHER_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(Refusal::LauncherExit(status.to_string())),
            Ok(None) if Instant::now() < deadline => thread::sleep(LAUNCHER_POLL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Refusal::LauncherTimeout);
            }
            Err(e) => return Err(Refusal::LauncherStart(e.to_string())),
        }
    }
}

/// Open the file on a thread of its own and deliver ONE [`AppEvent::FileOpened`]:
/// the click never waits on the launcher (`send::spawn_send`'s shape).
pub fn spawn_open(req: OpenRequest, tx: Sender<AppEvent>) {
    thread::spawn(move || {
        let (status, success) = open(&req);
        let _ = tx.send(AppEvent::FileOpened { status, success });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exists_in<'a>(files: &'a [&'a str]) -> impl Fn(&Path) -> bool + 'a {
        move |p| files.iter().any(|f| Path::new(f) == p)
    }

    fn paths(list: &[&str]) -> Vec<PathBuf> {
        list.iter().map(PathBuf::from).collect()
    }

    const WEBSTORM: &str = "/Users/u/Applications/WebStorm 2026.3.app/Contents/MacOS/webstorm";
    const INSIDERS_BUNDLE: &str = "/Applications/Visual Studio Code - Insiders.app";

    #[test]
    fn a_jetbrains_ancestor_is_its_own_launcher() {
        let ide = choose(
            &paths(&["/bin/zsh", WEBSTORM, "/sbin/launchd"]),
            &exists_in(&[]),
        )
        .expect("a WebStorm bundle binary in the ancestry names the IDE");
        assert_eq!(ide.family, Family::JetBrains);
        assert_eq!(ide.name, "WebStorm 2026.3");
        assert_eq!(ide.launcher, Path::new(WEBSTORM));
    }

    #[test]
    fn two_installs_of_one_product_each_name_their_own_launcher() {
        let other = "/Applications/WebStorm.app/Contents/MacOS/webstorm";
        let a = choose(&paths(&[WEBSTORM]), &exists_in(&[])).unwrap();
        let b = choose(&paths(&[other]), &exists_in(&[])).unwrap();
        assert_ne!(a.launcher, b.launcher);
        assert_eq!(b.launcher, Path::new(other));
    }

    #[test]
    fn a_jetbrains_helper_in_the_bundle_is_not_the_launcher() {
        for helper in [
            "/Applications/WebStorm.app/Contents/jbr/Contents/Home/bin/java",
            // Named like the product, but not the bundle's main binary.
            "/Applications/WebStorm.app/Contents/plugins/remote/bin/webstorm",
        ] {
            assert_eq!(choose(&paths(&[helper]), &exists_in(&[])), None, "{helper}");
        }
    }

    #[test]
    fn a_vscode_helper_names_the_outermost_bundles_launcher() {
        let helper = format!(
            "{INSIDERS_BUNDLE}/Contents/Frameworks/Code - Insiders Helper (Plugin).app/Contents/MacOS/Code - Insiders Helper (Plugin)"
        );
        let launcher = format!("{INSIDERS_BUNDLE}/Contents/Resources/app/bin/code");
        let ide = choose(&paths(&[&helper]), &exists_in(&[&launcher]))
            .expect("the bundle holds a launcher script");
        assert_eq!(ide.family, Family::VsCode);
        assert_eq!(ide.name, "Visual Studio Code - Insiders");
        assert_eq!(ide.launcher, Path::new(&launcher));
        // The same helper, with no launcher script on disk: no IDE, no guess.
        assert_eq!(choose(&paths(&[&helper]), &exists_in(&[])), None);
    }

    #[test]
    fn zed_is_its_bundles_cli() {
        let exe = "/Applications/Zed.app/Contents/MacOS/zed";
        let cli = "/Applications/Zed.app/Contents/MacOS/cli";
        let ide = choose(&paths(&[exe]), &exists_in(&[cli])).unwrap();
        assert_eq!(
            (ide.family, ide.launcher),
            (Family::Zed, PathBuf::from(cli))
        );
        assert_eq!(choose(&paths(&[exe]), &exists_in(&[])), None);
    }

    #[test]
    fn linux_layouts_resolve_a_launcher_that_exists() {
        let jb = choose(
            &paths(&["/opt/webstorm/jbr/bin/java"]),
            &exists_in(&["/opt/webstorm/bin/webstorm.sh"]),
        )
        .unwrap();
        assert_eq!(jb.family, Family::JetBrains);
        assert_eq!(jb.launcher, Path::new("/opt/webstorm/bin/webstorm.sh"));
        let code = choose(
            &paths(&["/usr/share/code/code"]),
            &exists_in(&["/usr/share/code/bin/code"]),
        )
        .unwrap();
        assert_eq!(code.family, Family::VsCode);
        let zed = choose(
            &paths(&["/home/u/.local/zed.app/libexec/zed-editor"]),
            &exists_in(&["/home/u/.local/zed.app/bin/zed"]),
        )
        .unwrap();
        assert_eq!(zed.family, Family::Zed);
        // A bare `java` with no JetBrains runtime around it is just java.
        assert_eq!(choose(&paths(&["/usr/bin/java"]), &exists_in(&[])), None);
    }

    /// The no-IDE answer: an ancestry of ordinary programs, an empty one, a
    /// terminal emulator that is not an IDE, and a multiplexer server (the shape of
    /// a tmux pane, whose parents are the server and never the IDE it may have been
    /// started from) all end as `None`.
    #[test]
    fn no_ide_in_the_ancestry_is_none() {
        let plain = paths(&[
            "/bin/zsh",
            "/opt/homebrew/bin/tmux",
            "/Applications/iTerm.app/Contents/MacOS/iTerm2",
            "/Applications/Terminal.app/Contents/MacOS/Terminal",
            "/sbin/launchd",
        ]);
        assert_eq!(choose(&plain, &exists_in(&[])), None);
        assert_eq!(choose(&[], &exists_in(&[])), None);
    }

    #[test]
    fn detection_delivers_exactly_one_event_whatever_it_finds() {
        let (tx, rx) = std::sync::mpsc::channel();
        spawn_detect(tx);
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(AppEvent::IdeDetected(_)) => {}
            other => panic!("expected one IdeDetected, got {other:?}"),
        }
        assert!(
            rx.recv_timeout(Duration::from_secs(1)).is_err(),
            "a one-shot sends once and its sender is dropped"
        );
    }

    #[test]
    fn the_nearest_ide_wins() {
        let launcher = format!("{INSIDERS_BUNDLE}/Contents/Resources/app/bin/code");
        let helper = format!("{INSIDERS_BUNDLE}/Contents/MacOS/Electron");
        let ide = choose(&paths(&[&helper, WEBSTORM]), &exists_in(&[&launcher])).unwrap();
        assert_eq!(ide.family, Family::VsCode);
    }

    #[test]
    fn each_family_builds_its_own_command_line() {
        let file = Path::new("/w/src/a.rs");
        let make = |family| Ide {
            name: "x".into(),
            family,
            launcher: PathBuf::from("/l"),
        };
        let strs = |args: Vec<OsString>| -> Vec<String> {
            args.into_iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(
            strs(make(Family::JetBrains).args(file, Some(21))),
            ["--line", "21", "/w/src/a.rs"]
        );
        assert_eq!(
            strs(make(Family::JetBrains).args(file, None)),
            ["/w/src/a.rs"]
        );
        assert_eq!(
            strs(make(Family::VsCode).args(file, Some(21))),
            ["-r", "-g", "/w/src/a.rs:21"]
        );
        assert_eq!(
            strs(make(Family::VsCode).args(file, None)),
            ["-r", "-g", "/w/src/a.rs"]
        );
        assert_eq!(
            strs(make(Family::Zed).args(file, Some(3))),
            ["/w/src/a.rs:3"]
        );
        assert_eq!(strs(make(Family::Zed).args(file, None)), ["/w/src/a.rs"]);
    }

    #[test]
    fn a_path_holding_a_colon_opens_without_its_line_in_vscode_and_zed_only() {
        let file = Path::new("/w/docs/plans/2026-11-06T05:42:19Z-x/HANDOVER.md");
        let make = |family| Ide {
            name: "x".into(),
            family,
            launcher: PathBuf::from("/l"),
        };
        let strs = |args: Vec<OsString>| -> Vec<String> {
            args.into_iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        let path = file.to_string_lossy().into_owned();
        for line in [Some(21), None] {
            assert_eq!(
                strs(make(Family::VsCode).args(file, line)),
                ["-r", path.as_str()],
                "no -g, the path verbatim and last"
            );
            assert_eq!(strs(make(Family::Zed).args(file, line)), [path.as_str()]);
        }
        assert_eq!(
            strs(make(Family::JetBrains).args(file, Some(21))),
            ["--line", "21", path.as_str()]
        );
    }

    #[test]
    fn the_opened_status_names_a_dropped_line_and_only_then() {
        let colon = Path::new("/w/05:42/a.md");
        let plain = Path::new("/w/a.md");
        let make = |family| Ide {
            name: "Code".into(),
            family,
            launcher: PathBuf::from("/l"),
        };
        let code = make(Family::VsCode);
        assert_eq!(
            opened_status(&code, colon, Some(9)),
            "opened in Code - line 9 not passed, the path holds ':'"
        );
        assert_eq!(opened_status(&code, colon, None), "opened in Code");
        assert_eq!(opened_status(&code, plain, Some(9)), "opened in Code");
        assert_eq!(
            opened_status(&make(Family::JetBrains), colon, Some(9)),
            "opened in Code"
        );
    }

    #[test]
    fn a_proc_stat_line_gives_its_parent_past_a_hostile_comm() {
        assert_eq!(ppid_from_stat("812 (zsh) S 811 812 812 0 -1"), Some(811));
        assert_eq!(ppid_from_stat("9 (a) b) c (d) R 4242 9 9"), Some(4242));
        assert_eq!(ppid_from_stat("no parens"), None);
        assert_eq!(ppid_from_stat("1 (x) S"), None);
        assert_eq!(ppid_from_stat("1 (x) S notanumber"), None);
    }

    #[test]
    fn this_process_has_a_readable_ancestry_walk_that_ends() {
        // The test runner is not an IDE, but its walk must terminate and never
        // panic whatever it is run under.
        let exes = ancestor_exes();
        assert!(exes.len() <= MAX_ANCESTORS);
    }

    // --- the worker: what a click ends up saying -----------------------------

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("snapback-ide-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    fn fake_launcher(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("launcher.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn request(launcher: PathBuf, candidates: Vec<PathBuf>) -> OpenRequest {
        OpenRequest {
            ide: Ide {
                name: "TestIDE".into(),
                family: Family::JetBrains,
                launcher,
            },
            candidates,
            line: Some(7),
        }
    }

    #[test]
    fn pick_file_takes_the_first_regular_file_and_names_the_failure_otherwise() {
        let dir = scratch("pick");
        let file = dir.join("a.md");
        std::fs::write(&file, "x").unwrap();
        let missing = dir.join("nope.md");
        assert_eq!(
            pick_file(&[missing.clone(), file.clone()]),
            Ok(file.clone())
        );
        assert_eq!(
            pick_file(std::slice::from_ref(&missing)),
            Err((Refusal::Missing, missing.clone()))
        );
        assert_eq!(
            pick_file(&[missing, dir.clone()]),
            Err((Refusal::NotRegular, dir.clone()))
        );
        assert_eq!(pick_file(&[]), Err((Refusal::Missing, PathBuf::new())));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_launcher_that_exits_zero_reports_the_ide_and_gets_the_file() {
        let dir = scratch("ok");
        let file = dir.join("a.md");
        std::fs::write(&file, "x").unwrap();
        let log = dir.join("args.txt");
        let launcher = fake_launcher(
            &dir,
            &format!("printf '%s\\n' \"$@\" > '{}'", log.display()),
        );
        let (status, ok) = open(&request(launcher, vec![file.clone()]));
        assert_eq!((status.as_str(), ok), ("opened in TestIDE", true));
        let args = std::fs::read_to_string(&log).unwrap();
        assert_eq!(args, format!("--line\n7\n{}\n", file.display()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_file_a_failing_launcher_and_an_unstartable_one_each_refuse_in_their_own_words() {
        let dir = scratch("refuse");
        let file = dir.join("a.md");
        std::fs::write(&file, "x").unwrap();
        let gone = dir.join("gone.md");

        let ok = fake_launcher(&dir, "exit 0");
        let (status, success) = open(&request(ok, vec![gone.clone()]));
        assert!(!success);
        assert_eq!(
            status,
            format!("not opening {} - no such file", gone.display())
        );

        let failing = fake_launcher(&dir, "exit 3");
        let (status, success) = open(&request(failing, vec![file.clone()]));
        assert!(!success);
        assert!(
            status.starts_with(&format!(
                "not opening {} - the IDE launcher exited with",
                file.display()
            )),
            "{status}"
        );

        let (status, success) = open(&request(dir.join("no-such-launcher"), vec![file.clone()]));
        assert!(!success);
        assert!(
            status.starts_with(&format!(
                "not opening {} - could not start the IDE launcher",
                file.display()
            )),
            "{status}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_no_ide_refusal_names_the_path_and_the_reason() {
        assert_eq!(
            Refusal::NoIde.status(Path::new("/w/docs/a.md")),
            "not opening /w/docs/a.md - snapback is not running inside an IDE terminal"
        );
    }

    #[test]
    fn spawn_open_delivers_exactly_one_event() {
        let (tx, rx) = std::sync::mpsc::channel();
        spawn_open(
            request(PathBuf::from("/nope"), vec![PathBuf::from("/nope/a.md")]),
            tx,
        );
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(AppEvent::FileOpened {
                success: false,
                status,
            }) => {
                assert!(status.contains("/nope/a.md"), "{status}");
            }
            other => panic!("expected one FileOpened refusal, got {other:?}"),
        }
        assert!(
            rx.recv_timeout(Duration::from_secs(1)).is_err(),
            "one event per open"
        );
    }
}
