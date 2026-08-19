//! The `--model` aliases the INSTALLED `claude` binary accepts, read out of that
//! binary at runtime.
//!
//! # Why this exists
//!
//! [`crate::tui::app::MODEL_ALIASES`] is a COMPILE-TIME list, and a compile-time
//! list of an external CLI's vocabulary rots on every `claude` release: a new
//! alias cannot reach the picker until snapback itself ships again, and a
//! withdrawn one keeps being offered. This module removes that coupling. The
//! const is demoted to a cold-start SEED — what the picker draws until this scan
//! answers — and the installed binary becomes the LIVE source.
//!
//! `claude --help` cannot be that source: it names only `fable`/`opus`/`sonnet`
//! and omits `haiku`, `best`, the `[1m]` long-context variants and `opusplan` —
//! the one alias with real leverage and the one a user cannot discover from the
//! CLI at all. The accepted set is a literal array inside the shipped bundle, so
//! that array is what gets read. `docs/agents/CLAUDE_CLI.md` records the same
//! capture as a shell one-liner; this is that one-liner turned into code.
//!
//! # Shape
//!
//! The split mirrors [`crate::agents`] exactly, for the same reasons:
//!
//! * [`parse_model_aliases`] is the PURE extractor and the SINGLE place the byte
//!   format is interpreted — the counterpart of
//!   [`crate::agents::parse_agents_json`]. It never spawns, never touches the
//!   filesystem, and is pinned against byte windows captured from two real
//!   binaries.
//! * [`installed_model_aliases`] is the IMPURE driver — the counterpart of
//!   `agents::run_agents` — and it FAILS SOFT to an EMPTY result on every branch:
//!   no `PATH`, no `claude` on it, a canonicalize that fails, an unreadable file,
//!   a read error mid-scan, or simply no match. Empty means "keep the seed"; it
//!   never panics and never hands a caller an error to deal with.
//!
//! It is a plain blocking function, so a caller puts it on its own thread the way
//! [`crate::agents::reported_agents`] is polled off-thread (AGENTS.md:
//! OFF-UI-THREAD). Nothing in this module knows about threads or events.
//!
//! # Locating the binary — why `canonicalize` is not optional
//!
//! snapback invokes bare `"claude"` and lets `$PATH` resolve it (see
//! `agents::AGENTS_ARGV`), so this walks `$PATH` for the same reason: scanning
//! some other install would answer a question nobody asked. The `PATH` hit is
//! then CANONICALIZED, and that step is load-bearing rather than tidy-up. A
//! managed install puts a SYMLINK on `PATH` pointing into a versioned store —
//! observed on the development machine as
//! `~/.local/bin/claude -> ~/.local/share/claude/versions/2.1.233`, beside three
//! older versions kept on disk. Without canonicalizing, the scan reads "whatever
//! that link happens to point at" and there is no way to say WHICH version was
//! read; with it, the resolved path names the version, which is what makes a
//! surprising alias set diagnosable instead of merely wrong.
//!
//! # Reading the file — why it is chunked
//!
//! The binary is a bundled JavaScript runtime around 290 MB and is NOT valid
//! UTF-8, so `read_to_string` is out and reading it whole is a 290 MB allocation
//! for one 86-byte answer. It is read in [`SCAN_CHUNK_BYTES`] chunks with a
//! [`SCAN_OVERLAP_BYTES`] window carried across each boundary, so an array that
//! straddles two chunks is still seen intact. That overlap is provably big enough
//! BY CONSTRUCTION rather than by hope: it is defined as
//! [`MAX_ALIAS_ARRAY_BYTES`], and the extractor itself abandons any candidate
//! array longer than that — so there is no match longer than the overlap for a
//! boundary to lose.
//!
//! # Why the byte search is hand-written std — a resolved rule conflict
//!
//! AGENTS.md's NUCLEO ISOLATION rule confines every `nucleo` AND `memchr` call to
//! `src/search.rs`. This module therefore uses NEITHER: the scan is a plain std
//! byte walk, deliberately unoptimized, because obvious correctness is the goal
//! here and speed is not.
//!
//! The decisive reason is NOT "a std scan is fast enough to disappear behind the
//! I/O". That claim is simply false, and it was MEASURED against the installed
//! 2.1.233 binary rather than assumed: the whole scan takes ~125 ms in a release
//! build and ~2.2 s in a debug one, so the walk plainly dominates the read and a
//! dev build pays two seconds for it.
//!
//! The decisive reason is that **nothing waits on this scan**. It is a one-shot
//! probe on its own thread, run once per launch, and the picker renders the seed
//! until it delivers — two seconds later is still long before anyone opens the
//! picker, and nothing on the render path can block on it. Scan latency is
//! therefore not a product concern at all, and amending a critical rule to buy
//! latency nobody can perceive is a bad trade.
//!
//! A THIRD option was considered and REJECTED: exporting a byte-search helper
//! from `src/search.rs` so the call would live inside the isolated module. That
//! satisfies the rule's letter while defeating its purpose. `search.rs` has one
//! documented job — session search — and moving binary-scanning utilities into it
//! widens exactly the surface NUCLEO ISOLATION exists to keep narrow. Compliance
//! that damages what the rule protects is not compliance. A future contributor
//! reading only the rule text would see nothing to warn them, since the rule is
//! phrased as being about search; this paragraph is that warning.
//!
//! # Why the cache is in memory — a second resolved rule conflict
//!
//! The result is cached in a process-lifetime [`OnceLock`] and NOTHING is written
//! to disk. `src/config.rs` reserves a home for "any future cache", but that is an
//! adjacent artifact and cannot override a critical rule. AGENTS.md's THE PARSE
//! CACHE rule sits directly above SNAPBACK-OWNED STATE and settles this category
//! outright — *"Keep the cache IN MEMORY (derived state, not the owned state
//! below)."* The alias list is DERIVED state, derived from the claude binary, and
//! is the same category as the parse cache. SNAPBACK-OWNED STATE keeps the
//! hidden-session id set as the ONLY persistent state snapback writes, and this
//! module leaves that true. One background scan per launch costs nothing, for the
//! same reason the scan speed does not matter: nothing waits on it.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The program name snapback spawns and therefore the one to look for on `$PATH`.
///
/// Spelled here to match `agents::AGENTS_ARGV`'s bare `"claude"`: the binary this
/// module reads must be the binary the hand-off will run, or the aliases offered
/// describe some other install.
const CLAUDE_PROGRAM: &str = "claude";

/// The array element that IDENTIFIES the `--model` alias array among the many
/// string arrays in the bundle.
///
/// Anchoring on ARRAY CONTENT is deliberate, and it is the third of three traps
/// this extractor exists to survive. The array is assigned to a MINIFIED
/// identifier that is regenerated every build — observed as `h9e`, `bze`, `SWe`
/// and `qWe` across four consecutive releases, with the neighbouring base-four
/// array called `_an`, `jcn`, `agn` and `yyn` in the same four. An identifier
/// anchor would break on literally every release.
///
/// `opusplan` specifically, because it is the discriminator: no other array in
/// the bundle carries it, and it is precisely the alias `claude --help` hides.
/// Two sibling arrays sit close enough to be mistaken for the real one and are
/// excluded by this anchor alone — the 17-element FULL MODEL ID list
/// (`["claude-3-5-haiku",…,"claude-sonnet-5"]`) that immediately precedes it, and
/// the base-four `["sonnet","opus","haiku","fable"]` that immediately follows it.
/// The full-id list being NEARLY TWICE as long as the alias array is why the
/// anchor cannot be replaced by "take the longest array".
///
/// If a future release drops the alias, the scan finds nothing and the caller
/// keeps its seed — a fail-soft miss, which is strictly safer than confidently
/// offering the wrong array.
const ANCHOR_ALIAS: &str = "opusplan";

/// The longest byte run the extractor will interpret as ONE alias array; a
/// candidate that runs past it is abandoned.
///
/// The real array is 86 bytes today
/// (`["sonnet","opus","haiku","fable","best","sonnet[1m]","opus[1m]","fable[1m]","opusplan"]`),
/// so 4 KiB leaves roughly 47x headroom — the alias set would have to grow
/// forty-fold before this cap could hide it. The cap is not primarily a size
/// guard, though: it is what makes [`SCAN_OVERLAP_BYTES`] PROVABLY sufficient. A
/// bound the extractor enforces is a bound the reader can rely on, whereas a
/// bound merely hoped for is not a bound at all.
const MAX_ALIAS_ARRAY_BYTES: usize = 4096;

/// How many trailing bytes of each chunk are carried into the next one so a match
/// spanning a chunk boundary is not sliced in half.
///
/// Equal to [`MAX_ALIAS_ARRAY_BYTES`] BY DEFINITION, not by coincidence: the
/// extractor refuses to consider any array longer than that, so no match can
/// exceed the overlap and no boundary can lose one. Retuning either value alone
/// would break that proof, which is why this is derived from the other rather
/// than spelled again.
const SCAN_OVERLAP_BYTES: usize = MAX_ALIAS_ARRAY_BYTES;

/// How much of the binary is read per `read` call — the buffer IS the chunk, so
/// no `BufReader` sits in front of it adding a second copy.
///
/// 1 MiB over a ~290 MB binary is ~290 reads, and the [`SCAN_OVERLAP_BYTES`]
/// re-scan costs 0.4% of each chunk in duplicated work. Smaller chunks would pay
/// that overlap proportionally more often; larger ones would hold more memory for
/// a scan whose latency nobody is waiting on.
const SCAN_CHUNK_BYTES: usize = 1 << 20;

/// The four structural bytes of the minified array literal being read.
///
/// Named rather than spelled inline (NO MAGIC VALUES) because they are the format
/// itself: the grammar accepted is exactly `[` `"…"` (`,` `"…"`)* `]`, with no
/// whitespace, no nesting and no non-string element — the bundle is minified, so
/// anything looser would admit arrays this module has no business reading.
const ARRAY_OPEN: u8 = b'[';
/// See [`ARRAY_OPEN`].
const ARRAY_CLOSE: u8 = b']';
/// See [`ARRAY_OPEN`].
const ELEMENT_QUOTE: u8 = b'"';
/// See [`ARRAY_OPEN`].
const ELEMENT_SEPARATOR: u8 = b',';
/// See [`ARRAY_OPEN`]. An escape inside an element REJECTS the whole candidate
/// rather than being decoded: no alias contains one, so unescaping would mean
/// inventing a decoder for a case that does not exist, and guessing wrong there
/// would put a corrupted string in front of the user.
const ELEMENT_ESCAPE: u8 = b'\\';

/// Extract the `--model` alias array from a window of raw binary bytes.
///
/// PURE, and the ONLY place the byte format is interpreted — the counterpart of
/// [`crate::agents::parse_agents_json`], and held to the same rule: schema drift
/// gets handled in one spot or it gets handled inconsistently. It never spawns
/// and never reads the filesystem, which is what lets it be pinned against
/// captured bytes instead of against a 290 MB binary the test suite cannot ship.
///
/// Every `[` in `bytes` starts a candidate. A candidate must parse as a FLAT
/// array of double-quoted UTF-8 strings within [`MAX_ALIAS_ARRAY_BYTES`], and
/// must contain [`ANCHOR_ALIAS`]; everything else is skipped. Among the
/// survivors the LONGEST wins.
///
/// The two selection rules cover different failures and neither replaces the
/// other:
///
/// * The ANCHOR rejects the sibling arrays — most importantly the 17-element full
///   model-id list sitting immediately before the alias array, which a
///   longest-wins rule on its own would happily return instead.
/// * LONGEST-WINS is the deterministic tie-break among anchored candidates. It
///   matters for two reasons. A future build could emit the anchored array twice,
///   or emit a superset beside it, and the fuller one is the flag's accepted set.
///   And because the caller re-scans an overlapping window each chunk, the SAME
///   array is routinely seen twice — folding by length makes that idempotent with
///   no deduplication step.
///
/// Returns an EMPTY vector when nothing matches. Values come back RAW and
/// unvalidated, `sonnet[1m]` and all: what `--model` accepts is claude's to
/// decide, never snapback's.
#[must_use]
pub fn parse_model_aliases(bytes: &[u8]) -> Vec<String> {
    let mut best: Vec<String> = Vec::new();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != ARRAY_OPEN {
            continue;
        }
        let Some(candidate) = string_array_at(bytes, index) else {
            continue; // Not a flat string array -> not the thing being looked for.
        };
        if !candidate.iter().any(|element| element == ANCHOR_ALIAS) {
            continue; // A string array, but not THIS one (see `ANCHOR_ALIAS`).
        }
        if candidate.len() > best.len() {
            best = candidate;
        }
    }
    best
}

/// Read `bytes[start..]` as a flat array of double-quoted strings, or `None` if it
/// is not exactly that.
///
/// Private, and the only helper [`parse_model_aliases`] delegates to, so the
/// format still has one interpreter. Strict on purpose — the bundle is minified,
/// so whitespace, nesting, a non-string element, an escape, a non-UTF-8 element
/// or a run past [`MAX_ALIAS_ARRAY_BYTES`] all mean "this is not the array" and
/// bail immediately.
///
/// Two consequences are worth stating because they are what the traps turn on:
///
/// * A `[` or `]` INSIDE a quoted element cannot terminate anything, because the
///   scan is inside a string when it meets them. That is the whole reason a
///   `[^]]*`-style terminator fails here: it truncates the real array at
///   `"sonnet[1m]` and silently drops the four aliases after it.
/// * A `[` that opens something else — `sonnet[1m]`'s own bracket, a regex
///   literal such as `/\[1m\]$/i` — is still tried as a candidate and simply
///   fails within a byte or two, so no pre-filtering of candidate positions is
///   needed to stay correct.
///
/// Non-UTF-8 element bytes reject the WHOLE candidate rather than being repaired
/// lossily, mirroring [`crate::worktrees`]' rule for the same reason: a
/// replacement character quietly turns garbage into a plausible-looking value,
/// and a plausible wrong answer is worse than none.
fn string_array_at(bytes: &[u8], start: usize) -> Option<Vec<String>> {
    let limit = bytes.len().min(start.saturating_add(MAX_ALIAS_ARRAY_BYTES));
    let mut cursor = start.saturating_add(1);
    let mut elements: Vec<String> = Vec::new();
    if cursor >= limit {
        return None;
    }
    if bytes[cursor] == ARRAY_CLOSE {
        return Some(elements); // `[]` — well formed, just carries no anchor.
    }
    loop {
        if cursor >= limit || bytes[cursor] != ELEMENT_QUOTE {
            return None; // Not a quoted element -> not a flat string array.
        }
        cursor += 1;
        let element_start = cursor;
        loop {
            if cursor >= limit {
                return None; // Ran off the window or past the cap mid-element.
            }
            match bytes[cursor] {
                ELEMENT_ESCAPE => return None, // See `ELEMENT_ESCAPE`.
                ELEMENT_QUOTE => break,
                _ => cursor += 1,
            }
        }
        let element = std::str::from_utf8(&bytes[element_start..cursor]).ok()?;
        elements.push(element.to_owned());
        cursor += 1;
        if cursor >= limit {
            return None; // No terminator before the cap -> abandon the candidate.
        }
        match bytes[cursor] {
            ELEMENT_SEPARATOR => cursor += 1,
            ARRAY_CLOSE => return Some(elements),
            _ => return None, // Anything else is not this grammar.
        }
    }
}

/// The `$PATH` entries to look for [`CLAUDE_PROGRAM`] in, in order.
///
/// PURE (it is string work over an already-read variable), so the resolution
/// ORDER and the empty-entry rule are testable without an environment. An EMPTY
/// entry is dropped rather than read as the process cwd: POSIX says an empty
/// `PATH` element means "here", but a `./claude` in whatever directory snapback
/// was launched from is not the install the hand-off will spawn, and scanning it
/// would be both surprising and slow.
#[must_use]
fn claude_path_candidates(path_var: &OsStr) -> Vec<PathBuf> {
    std::env::split_paths(path_var)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(CLAUDE_PROGRAM))
        .collect()
}

/// The user/group/other execute bits; any one of them makes it runnable by
/// somebody, and which one applies to this process is the OS's call.
///
/// At module scope rather than inside [`is_executable_file`] so the test that
/// pins the directory case can assert against the SAME number the check uses,
/// instead of spelling `0o111` a second time beside it.
#[cfg(unix)]
const EXECUTABLE_BITS: u32 = 0o111;

/// Whether `path` is a file the OS would actually execute.
///
/// Follows symlinks deliberately (`metadata`, not `symlink_metadata`): the `PATH`
/// hit is EXPECTED to be a link into a versioned store, and the question here is
/// about the target. Fail-soft — an unreadable or absent path is simply not a
/// candidate.
#[cfg(unix)]
#[must_use]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & EXECUTABLE_BITS != 0)
}

/// Whether `path` is a file the OS would actually execute — the non-unix arm,
/// where there is no execute bit to consult, so being a regular file is the whole
/// test. snapback ships macOS and Linux builds only; this exists so the crate
/// still compiles elsewhere rather than to serve a supported target.
#[cfg(not(unix))]
#[must_use]
fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
}

/// Resolve the `claude` binary the hand-off would spawn, fully canonicalized.
///
/// Impure and fail-soft to `None`: no `PATH` in the environment, no executable
/// `claude` on it, or a hit that will not canonicalize all yield "no binary",
/// never an error.
///
/// Reads the environment and nothing else — the WALK is
/// [`first_canonical_executable`]'s, so the only thing untestable here is the one
/// `var_os` call.
///
/// See the module docs for why canonicalizing is not optional.
#[must_use]
fn locate_claude() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    first_canonical_executable(claude_path_candidates(&path_var), |candidate| {
        std::fs::canonicalize(candidate).ok()
    })
}

/// The first of `candidates`, in order, that is BOTH executable and
/// canonicalizable.
///
/// Candidates are tried in `PATH` order and NEITHER disqualifier ends the walk —
/// the next entry still gets its turn.
///
/// The canonicalize refusal looks dead and is not. [`is_executable_file`]
/// follows symlinks, so a dangling link fails `metadata` and never reaches
/// `realpath` — but those are two syscalls with a window between them, and the
/// managed layout the module docs describe (a `PATH` symlink into a versioned
/// store) swaps that store out from under the window every time claude upgrades
/// itself. A candidate that passed `metadata` against the outgoing version can
/// fail `realpath` a moment later.
///
/// That the branch is reachable only through a race is why `canonicalize` is a
/// PARAMETER: [`crate::worktrees::parse_porcelain`]'s reason (hermetic under
/// test), plus one this walk alone has — no test can SCHEDULE a race, so
/// injecting the refusal is the only way to state the rule as an assertion
/// rather than a hope. Production passes `std::fs::canonicalize`; nothing else
/// ever passes anything else. Do not collapse the walk to `.next()`: a
/// mid-upgrade race would then hide a working `claude` later on `$PATH`.
#[must_use]
fn first_canonical_executable(
    candidates: Vec<PathBuf>,
    canonicalize: impl Fn(&Path) -> Option<PathBuf>,
) -> Option<PathBuf> {
    candidates
        .into_iter()
        .filter(|candidate| is_executable_file(candidate))
        .find_map(|candidate| canonicalize(&candidate))
}

/// Scan one file for the alias array, chunk by chunk.
///
/// Impure, and the only thing it decides is WHERE to look — what the bytes mean is
/// [`parse_model_aliases`]', and how they are folded across windows is
/// [`scan_reader`]'s. A thin wrapper over that: it opens the file, fails soft to
/// EMPTY when it cannot, and delegates.
#[must_use]
fn scan_file(path: &Path) -> Vec<String> {
    let Ok(file) = File::open(path) else {
        return Vec::new(); // Unreadable or absent -> no signal.
    };
    scan_reader(file)
}

/// Scan an already-opened byte source for the alias array, chunk by chunk.
///
/// Reads [`SCAN_CHUNK_BYTES`] at a time and keeps the last [`SCAN_OVERLAP_BYTES`]
/// of the window in front of the next chunk, so an array landing on a boundary is
/// still seen whole; the LONGEST result across all windows wins, which also
/// collapses the duplicate sighting the overlap guarantees.
///
/// That cross-window fold is the load-bearing half and the reason this takes a
/// reader rather than a path: ~20 MB of bundle follows the alias array in the real
/// binary, so every window after the hit finds NOTHING, and an implementation that
/// merely kept the latest window's answer would return empty against the very file
/// this module exists to read while every path-level fixture stayed green. Taking
/// the source as a PARAMETER — the seam [`crate::watch`]'s threads use one level up
/// — is what lets a test hand over a window sequence and pin the fold, along with
/// the two read-error branches below, which no temp file can produce on demand.
///
/// A read ERROR discards whatever was found and returns EMPTY, matching
/// `agents::agents_from_output`'s rule that a failed run is not a reading: a
/// partial scan cannot say whether the array it happened to see was the longest
/// one. `ErrorKind::Interrupted` is the one exception and is retried, since it
/// carries no information about the file at all.
#[must_use]
fn scan_reader(mut source: impl Read) -> Vec<String> {
    let mut chunk = vec![0_u8; SCAN_CHUNK_BYTES];
    let mut window: Vec<u8> = Vec::with_capacity(SCAN_OVERLAP_BYTES + SCAN_CHUNK_BYTES);
    let mut best: Vec<String> = Vec::new();
    loop {
        let read = match source.read(&mut chunk) {
            Ok(0) => break, // EOF.
            Ok(read) => read,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return Vec::new(), // A failed read is not a reading.
        };
        window.extend_from_slice(&chunk[..read]);
        let found = parse_model_aliases(&window);
        if found.len() > best.len() {
            best = found;
        }
        let keep = window.len().min(SCAN_OVERLAP_BYTES);
        window.drain(..window.len() - keep);
    }
    best
}

/// The `--model` aliases the installed `claude` accepts, or an EMPTY vector if
/// they could not be read.
///
/// The module's single public entry point, and the counterpart of
/// [`crate::agents::reported_agents`]: a plain BLOCKING function with no
/// arguments, so a caller runs it on its own thread and delivers the result as an
/// event. It must NOT be called on the render path — it opens and walks a ~290 MB
/// file, measured at ~125 ms release / ~2.2 s debug against the installed
/// `claude 2.1.233`.
///
/// Answers EMPTY on every failure path, which callers read as "keep the seed"
/// rather than as an error: the picker's compile-time
/// [`crate::tui::app::MODEL_ALIASES`] is what the board shows until — and unless —
/// this returns something.
///
/// The scan runs at most ONCE per process; the result is memoized in a
/// [`OnceLock`], so a second caller either gets the cached answer or blocks until
/// the first initializer finishes, and the binary is never read twice. Nothing is
/// written to disk — see the module docs for why that is settled rather than a
/// preference.
///
/// Its one runtime caller is [`crate::watch::EventLoop::spawn_model_alias_probe`],
/// which runs it on a thread of its own and delivers the answer as
/// [`AppEvent::ModelAliases`](crate::watch::AppEvent::ModelAliases). Because that
/// call site blocks nothing, the `OnceLock`'s "a second caller waits for the first"
/// behaviour costs nobody a frame.
#[must_use]
pub fn installed_model_aliases() -> Vec<String> {
    static CACHE: OnceLock<Vec<String>> = OnceLock::new();
    CACHE
        .get_or_init(|| locate_claude().map_or_else(Vec::new, |path| scan_file(&path)))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A byte window captured from the shipped `claude 2.1.228` bundle at absolute
    /// offset 256370957, 534 bytes long.
    ///
    /// This and its sibling below are REAL bytes read straight out of a shipped
    /// binary, never composed by hand, and both are cut to the SAME span — wide
    /// enough to carry every trap this extractor exists to survive:
    ///
    /// * the 17-element FULL MODEL ID array immediately before the alias array
    ///   (trap 1, and the hard half: it is nearly twice as long, so longest-wins
    ///   alone would return it);
    /// * the base-four `["sonnet","opus","haiku","fable"]` immediately after it
    ///   (trap 1, the short half);
    /// * `"sonnet[1m]"` and friends, whose embedded `]` truncates a `[^]]*`-style
    ///   terminator (trap 2);
    /// * the minified identifiers the array is bound to and read through
    ///   (`h9e` / `_an` here), which differ in EVERY captured version (trap 3);
    /// * a `/\[1m\]$/i` regex literal, i.e. a `[` that opens nothing at all.
    const CAPTURED_2_1_228: &[u8] = br#"["claude-3-5-haiku","claude-3-5-sonnet","claude-3-7-sonnet","claude-fable-5","claude-haiku-4-5","claude-mythos-5","claude-opus-4-0","claude-opus-4-1","claude-opus-4-5","claude-opus-4-6","claude-opus-4-7","claude-opus-4-8","claude-opus-5","claude-sonnet-4-0","claude-sonnet-4-5","claude-sonnet-4-6","claude-sonnet-5"],h9e=["sonnet","opus","haiku","fable","best","sonnet[1m]","opus[1m]","fable[1m]","opusplan"],_an=["sonnet","opus","haiku","fable"]});function sM(e){return h9e.includes(e)}function Ma(e){return e.replace(/\[1m\]$/i,"")}"#;

    /// The same window from the INSTALLED `claude 2.1.235` (absolute offset
    /// 275913857, the same 534 bytes wide), identifiers `UKe` / `G0n`. This is the
    /// version the alias array below was re-derived from.
    ///
    /// Byte-for-byte identical to [`CAPTURED_2_1_228`] apart from the minified
    /// identifiers — `h9e`→`UKe` at both its binding and its `includes` call,
    /// `_an`→`G0n` on the base-four sibling, and the two enclosing function names
    /// `sM`→`GH` and `Ma`→`bl`. That is the point: SEVEN releases apart, the two
    /// fixtures differ in exactly the field under test and in nothing else, so the
    /// churn assertion below is about the identifier and not about drift the alias
    /// array happened to pick up along the way.
    const CAPTURED_2_1_235: &[u8] = br#"["claude-3-5-haiku","claude-3-5-sonnet","claude-3-7-sonnet","claude-fable-5","claude-haiku-4-5","claude-mythos-5","claude-opus-4-0","claude-opus-4-1","claude-opus-4-5","claude-opus-4-6","claude-opus-4-7","claude-opus-4-8","claude-opus-5","claude-sonnet-4-0","claude-sonnet-4-5","claude-sonnet-4-6","claude-sonnet-5"],UKe=["sonnet","opus","haiku","fable","best","sonnet[1m]","opus[1m]","fable[1m]","opusplan"],G0n=["sonnet","opus","haiku","fable"]});function GH(e){return UKe.includes(e)}function bl(e){return e.replace(/\[1m\]$/i,"")}"#;

    /// Every captured window, tagged, so the cross-version assertions below name
    /// the version that failed rather than an index.
    ///
    /// # The corpus is CAPPED at two — the OLDEST and the CURRENT
    ///
    /// This cap is a rule, not an accident of what happened to be on disk. Every
    /// window here is byte-identical bar the minified identifiers, so an nth
    /// capture is an nth SPELLING of one claim, never an nth claim: it costs ~534
    /// bytes of source and buys nothing the pair does not already prove. Holding
    /// the pair far apart in time is what keeps the churn it pins non-vacuous;
    /// holding it to a PAIR is what stops this module accreting a fixture per
    /// `claude` release forever.
    ///
    /// **On a refresh, REPLACE the current fixture — never append one.** Re-cut
    /// the window from the newly installed binary, overwrite
    /// [`CAPTURED_2_1_235`] (renaming the const and its recorded offset to the new
    /// version), move its entry here and in [`CAPTURED_IDENTIFIERS`], and leave
    /// [`CAPTURED_2_1_228`] exactly where it is — it is the fixed baseline the
    /// distance is measured FROM, and replacing it would collapse that distance.
    ///
    /// A THIRD fixture is justified by exactly one thing: a STRUCTURAL difference
    /// no window here covers — a new sibling shape, a new separator, a new trap
    /// the extractor has to survive. Never by a new version number. "2.1.236
    /// shipped" is not a reason to add one; "2.1.236 puts a non-string element in
    /// the array" is.
    ///
    /// `docs/agents/CLAUDE_CLI.md` records FIVE observed identifiers
    /// (`h9e` → `bze` → `SWe` → `qWe` → `UKe`) where this corpus holds two. That
    /// mismatch is INTENTIONAL and must not be "corrected" by re-adding fixtures.
    /// That list is a HUMAN historical record of how fast the identifier churns —
    /// prose, read once, costing nothing to carry. A record of five sightings
    /// needs no five fixtures to stand behind it; the two here already exhibit the
    /// churn the extractor must not anchor on.
    const CAPTURED_VERSIONS: [(&str, &[u8]); 2] =
        [("2.1.228", CAPTURED_2_1_228), ("2.1.235", CAPTURED_2_1_235)];

    /// The minified identifier each captured window binds the alias array to.
    ///
    /// The SINGLE source of truth for BOTH halves of the churn assertion — "this
    /// version binds the array to X" and "no OTHER version shares X". An earlier
    /// form kept the second half in a hand-maintained list of its own, which had
    /// silently fallen one version short, so one captured identifier was never
    /// excluded from anything and the cross-check quietly covered less than it
    /// read as covering. Driving both loops from this one table makes the check
    /// pairwise-complete BY CONSTRUCTION rather than by upkeep, and the test walks
    /// [`CAPTURED_VERSIONS`] to reach it — so a window with no entry here is a
    /// panic, not a silent skip.
    const CAPTURED_IDENTIFIERS: [(&str, &str); 2] = [("2.1.228", "h9e"), ("2.1.235", "UKe")];

    /// The alias array as it stands in the installed binary, re-derived from
    /// `2.1.235` rather than taken on trust. It is the answer the extractor owes
    /// on every captured window.
    const EXPECTED_ALIASES: [&str; 9] = [
        "sonnet",
        "opus",
        "haiku",
        "fable",
        "best",
        "sonnet[1m]",
        "opus[1m]",
        "fable[1m]",
        "opusplan",
    ];

    /// The 17-element full model-id array that sits immediately BEFORE the alias
    /// array. Spelled out so the trap-1 test can prove the fixture really contains
    /// a longer competing array rather than merely asserting the happy answer.
    const FULL_MODEL_IDS: [&str; 17] = [
        "claude-3-5-haiku",
        "claude-3-5-sonnet",
        "claude-3-7-sonnet",
        "claude-fable-5",
        "claude-haiku-4-5",
        "claude-mythos-5",
        "claude-opus-4-0",
        "claude-opus-4-1",
        "claude-opus-4-5",
        "claude-opus-4-6",
        "claude-opus-4-7",
        "claude-opus-4-8",
        "claude-opus-5",
        "claude-sonnet-4-0",
        "claude-sonnet-4-5",
        "claude-sonnet-4-6",
        "claude-sonnet-5",
    ];

    /// The headline claim: the installed binary's alias set comes back verbatim
    /// and in order, `[1m]` variants and all.
    ///
    /// This is the whole reason the module exists — the compile-time seed lists
    /// six aliases and is missing `best`, `sonnet[1m]`, `opus[1m]` and
    /// `fable[1m]`, and this scan is what closes that gap without a release.
    #[test]
    fn the_installed_binarys_alias_array_is_read_verbatim() {
        assert_eq!(
            parse_model_aliases(CAPTURED_2_1_235),
            EXPECTED_ALIASES,
            "the aliases must come back raw and in wire order; nothing here \
             validates or reorders them"
        );
    }

    /// TRAP 1, the hard half: a 17-element array of full model ids sits
    /// IMMEDIATELY before the alias array, so a "longest string array wins" rule
    /// would return it — nearly twice as long, and completely wrong.
    ///
    /// Asserted in three parts so the test cannot pass vacuously: the fixture
    /// really does contain that array, it really is longer, and the extractor
    /// still returns the aliases.
    #[test]
    fn the_longer_full_model_id_array_beside_it_never_wins() {
        let window = String::from_utf8(CAPTURED_2_1_235.to_vec()).expect("fixture is ASCII");
        for id in FULL_MODEL_IDS {
            assert!(
                window.contains(&format!("\"{id}\"")),
                "the fixture must actually contain the competing array, else this \
                 test proves nothing: {id} is missing"
            );
        }
        assert!(
            FULL_MODEL_IDS.len() > EXPECTED_ALIASES.len(),
            "the competing array must be LONGER, else longest-wins would pick \
             correctly by accident"
        );

        let found = parse_model_aliases(CAPTURED_2_1_235);
        assert_eq!(found, EXPECTED_ALIASES);
        for id in FULL_MODEL_IDS {
            assert!(
                !found.iter().any(|alias| alias == id),
                "a full model id leaked into the alias set: {id}"
            );
        }
    }

    /// TRAP 1, the short half: the base-four `["sonnet","opus","haiku","fable"]`
    /// sits IMMEDIATELY after the alias array and shares its first four elements,
    /// so a leading-element anchor matches both. Returning it would silently lose
    /// `opusplan` — the single alias this whole feature exists to surface.
    #[test]
    fn the_adjacent_base_four_sibling_never_wins() {
        let window = String::from_utf8(CAPTURED_2_1_235.to_vec()).expect("fixture is ASCII");
        assert!(
            window.contains(r#"["sonnet","opus","haiku","fable"]"#),
            "the fixture must actually contain the base-four sibling"
        );

        let found = parse_model_aliases(CAPTURED_2_1_235);
        assert_eq!(found.len(), EXPECTED_ALIASES.len());
        assert!(
            found.iter().any(|alias| alias == ANCHOR_ALIAS),
            "losing {ANCHOR_ALIAS} to the base-four sibling is the exact failure \
             this anchor exists to prevent"
        );
    }

    /// TRAP 2: `sonnet[1m]` carries a `]` INSIDE a quoted element, which truncates
    /// any `[^]]*`-style terminator right there and drops the four aliases after
    /// it. All three `[1m]` variants must survive intact.
    #[test]
    fn an_embedded_bracket_inside_an_element_does_not_truncate_the_array() {
        let found = parse_model_aliases(CAPTURED_2_1_235);
        for variant in ["sonnet[1m]", "opus[1m]", "fable[1m]"] {
            assert!(
                found.iter().any(|alias| alias == variant),
                "{variant} was lost — the terminator stopped at its embedded ']'"
            );
        }
        assert_eq!(
            found.last().map(String::as_str),
            Some(ANCHOR_ALIAS),
            "a truncation at the first embedded ']' takes everything after it, so \
             the LAST element is what proves the array survived whole"
        );
    }

    /// TRAP 3: the array is bound to a minified identifier that is regenerated on
    /// every build, so nothing may anchor on it. The two captured releases are
    /// SEVEN versions apart and carry two different names for the SAME array.
    ///
    /// The fixtures are asserted to genuinely differ first — a pair of identical
    /// windows would make the cross-version claim vacuous. Both halves of that
    /// check read [`CAPTURED_IDENTIFIERS`], and the walk starts from
    /// [`CAPTURED_VERSIONS`], so EVERY captured window is examined and every
    /// OTHER version's identifier is excluded from it. That structure is the fix
    /// for a real defect: the exclusion half used to be a second hand-written
    /// list, and it had drifted a version short, so one captured identifier was
    /// never excluded from anything.
    #[test]
    fn the_alias_array_is_stable_across_versions_despite_churning_identifiers() {
        assert_eq!(
            CAPTURED_IDENTIFIERS.len(),
            CAPTURED_VERSIONS.len(),
            "the corpus and the identifier table must stay a bijection, else a \
             fixture can be added without ever being cross-checked"
        );

        for (version, bytes) in CAPTURED_VERSIONS {
            let window = String::from_utf8(bytes.to_vec()).expect("fixture is ASCII");
            let (_, identifier) = CAPTURED_IDENTIFIERS
                .iter()
                .find(|(tag, _)| *tag == version)
                .expect("every captured window names its identifier");
            assert!(
                window.contains(&format!("{identifier}=[")),
                "{version} should bind the array to {identifier}"
            );
            for (other_version, other_identifier) in CAPTURED_IDENTIFIERS {
                if other_version == version {
                    continue;
                }
                assert!(
                    !window.contains(&format!("{other_identifier}=[")),
                    "{version} must NOT share {other_version}'s identifier, else \
                     the churn this test pins is not in the fixtures"
                );
            }
        }

        for (version, window) in CAPTURED_VERSIONS {
            assert_eq!(
                parse_model_aliases(window),
                EXPECTED_ALIASES,
                "{version} must yield the same aliases as every other version"
            );
        }
    }

    /// A newly-appearing sibling array must not displace the anchored one.
    ///
    /// The sibling below is SYNTHESIZED, and saying so plainly is half the point
    /// of this comment. The hazard it models is real and was observed in the
    /// shipped bundle — `ANTHROPIC_TIER_NAMES`, the five-element
    /// `["sonnet","opus","haiku","fable","mythos"]`, is present in 2.1.232,
    /// 2.1.233 and 2.1.235 alike — but it sits roughly 24 MB PAST the alias array
    /// (offset 299795873 against 275913857 in 2.1.235), so no 534-byte capture
    /// window comes anywhere near it and NEITHER fixture above contains it.
    ///
    /// Two things in those fixtures look like it and are not. The four-element
    /// `["sonnet","opus","haiku","fable"]` they carry is the base-four sibling,
    /// one element short of this one. And the `claude-mythos-5` they both carry is
    /// a full MODEL ID inside the 17-element list — a different string in a
    /// different array. Do not read either as the capture of this hazard.
    ///
    /// So this test MODELS the hazard rather than replaying a capture: it appends
    /// a hand-written five-element sibling to a REAL window and asserts the
    /// selection rule is unmoved. Widening a fixture across 24 MB of bundle to
    /// turn it into a replay would cost far more than it proves; stating which
    /// half is captured and which is composed costs nothing.
    #[test]
    fn a_new_sibling_array_appearing_between_releases_changes_nothing() {
        const MYTHOS_SIBLING: &[u8] = br#"zz=["sonnet","opus","haiku","fable","mythos"],"#;

        let mut drifted = CAPTURED_2_1_235.to_vec();
        drifted.extend_from_slice(MYTHOS_SIBLING);
        assert_eq!(
            parse_model_aliases(&drifted),
            EXPECTED_ALIASES,
            "a newly-shipped sibling must not displace the anchored array"
        );

        // ...and the sibling on its own is correctly no answer at all.
        assert!(
            parse_model_aliases(MYTHOS_SIBLING).is_empty(),
            "an unanchored sibling is not the alias array"
        );
    }

    /// LONGEST-WINS, pinned on its own. The captured windows cannot pin it — the
    /// anchor leaves exactly one candidate there — so this states the rule
    /// directly, in both orders, so a "return the first match" implementation
    /// fails one of them whichever way it scans.
    #[test]
    fn the_longest_anchored_array_wins_in_either_order() {
        const SHORT_FIRST: &[u8] = br#"a=["opusplan"],b=["sonnet","opus","opusplan"];"#;
        const LONG_FIRST: &[u8] = br#"a=["sonnet","opus","opusplan"],b=["opusplan"];"#;

        assert_eq!(
            parse_model_aliases(SHORT_FIRST),
            ["sonnet", "opus", "opusplan"],
            "a later, fuller anchored array must displace an earlier short one"
        );
        assert_eq!(
            parse_model_aliases(LONG_FIRST),
            ["sonnet", "opus", "opusplan"],
            "and a later short one must not displace the fuller earlier one"
        );
    }

    /// FAIL-SOFT: every shape that is not the array yields an EMPTY vector and
    /// never panics. Includes the near-misses that matter most — a truncated
    /// array, and the REAL base-four array with no anchor in it.
    #[test]
    fn garbage_empty_and_truncated_input_yield_no_aliases() {
        let cases: [(&str, &[u8]); 13] = [
            ("empty", b""),
            ("whitespace", b"   "),
            ("no bracket anywhere", b"not an array at all"),
            ("a bare open bracket", b"["),
            ("an empty array", b"[]"),
            ("truncated mid-element", br#"["sonnet","opus"#),
            ("truncated before the terminator", br#"["sonnet","opus""#),
            ("truncated after the separator", br#"["opusplan","#),
            (
                "unanchored real sibling",
                br#"["sonnet","opus","haiku","fable"]"#,
            ),
            ("non-string element", br#"["opusplan",42]"#),
            ("whitespace inside the array", br#"[ "opusplan" ]"#),
            // A closed element followed by neither `,` nor `]`. This is the one
            // shape that disqualifies a candidate AFTER it has already collected
            // usable elements, so it is the only case that can distinguish
            // "abandon the candidate" from "return what was gathered so far" —
            // and returning the partial set would hand back an array that was
            // never written.
            (
                "element with no separator after it",
                br#"["opusplan" "sonnet"]"#,
            ),
            ("element followed by an object brace", br#"["opusplan"}"#),
        ];
        for (label, raw) in cases {
            assert!(
                parse_model_aliases(raw).is_empty(),
                "expected no aliases for {label}"
            );
        }
    }

    /// A deliberate NON-obvious consequence of judging by content alone: what
    /// ENCLOSES the array is never consulted, so a flat anchored array nested
    /// inside another array or inside a JSON object is read exactly as a top-level
    /// one is.
    ///
    /// That is the right answer rather than a leak. Every `[` is tried, and the
    /// one that opens a non-flat structure simply fails a byte later while the
    /// flat one inside it succeeds — which is the same property that lets the
    /// scanner start mid-file at an arbitrary chunk boundary with no context at
    /// all. Pinned so nobody "fixes" it by reaching for the preceding byte, which
    /// would make the extractor position-dependent and break exactly that.
    #[test]
    fn a_flat_anchored_array_is_read_whatever_encloses_it() {
        assert_eq!(
            parse_model_aliases(br#"[["opusplan","sonnet"]]"#),
            ["opusplan", "sonnet"],
            "the outer `[` fails at the inner `[`; the inner one is a real array"
        );
        assert_eq!(
            parse_model_aliases(br#"{"aliases":["opusplan","sonnet"]}"#),
            ["opusplan", "sonnet"],
            "an object wrapper is not consulted either"
        );
    }

    /// A non-UTF-8 element rejects the WHOLE candidate rather than being repaired
    /// into a replacement character. The binary is not UTF-8, so this is the
    /// normal neighbourhood, and a lossily-repaired byte would read as a
    /// plausible alias the CLI has never heard of.
    #[test]
    fn a_non_utf8_element_rejects_the_whole_array() {
        let mut raw: Vec<u8> = br#"["opusplan","#.to_vec();
        raw.extend_from_slice(b"\"so");
        raw.push(0xFF); // Not valid UTF-8 in any position.
        raw.extend_from_slice(br#"nnet"]"#);
        assert!(
            parse_model_aliases(&raw).is_empty(),
            "a lossy repair would invent an alias out of binary noise"
        );

        // The same bytes with the invalid byte removed DO parse, so the emptiness
        // above is the UTF-8 check's doing and not a malformed fixture.
        let repaired = br#"["opusplan","sonnet"]"#;
        assert_eq!(parse_model_aliases(repaired), ["opusplan", "sonnet"]);
    }

    /// An escape inside an element rejects the candidate rather than being
    /// decoded — see `ELEMENT_ESCAPE`. Paired with the escape-free control so the
    /// emptiness cannot come from an unrelated defect in the fixture.
    ///
    /// The fixture is `["opusplan","x\"]` because the OBVIOUS one —
    /// `["opusplan","son\"net"]` — cannot fail. Read without the escape rule, that
    /// one ends its second element at the escaped quote and then meets `n`, which
    /// the SEPARATOR arm rejects anyway; the array comes back empty either way and
    /// the test certifies a rule it never exercised. Here the escape is the LAST
    /// byte of the element, so dropping the rule yields a well-formed
    /// `["opusplan","x\\"]` — a backslash handed to `--model` as if it were an
    /// alias, which is exactly the corrupted value `ELEMENT_ESCAPE` refuses to
    /// invent.
    #[test]
    fn an_escaped_element_rejects_the_whole_array() {
        assert!(
            parse_model_aliases(br#"["opusplan","x\"]"#).is_empty(),
            "an escape means a decoder this module deliberately does not have"
        );
        assert_eq!(
            parse_model_aliases(br#"["opusplan","x"]"#),
            ["opusplan", "x"],
            "the same bytes without the escape DO parse, so the emptiness above is \
             the escape rule's doing and not a malformed fixture"
        );
    }

    /// The [`MAX_ALIAS_ARRAY_BYTES`] cap is LIVE, and it is what makes the
    /// reader's overlap provably sufficient. Asserted from both sides: an
    /// anchored array just under the cap is found, one just over it is abandoned.
    #[test]
    fn a_candidate_longer_than_the_cap_is_abandoned() {
        /// One padding element, sized so the arithmetic below stays readable.
        const PADDING_ELEMENT: &str = "\"aaaaaaaaaa\",";

        let build = |element_count: usize| -> Vec<u8> {
            let mut raw = String::from("[\"opusplan\",");
            for _ in 0..element_count {
                raw.push_str(PADDING_ELEMENT);
            }
            raw.push_str("\"tail\"]");
            raw.into_bytes()
        };

        let under = build(100);
        assert!(
            under.len() < MAX_ALIAS_ARRAY_BYTES,
            "the under-cap fixture must actually be under the cap"
        );
        assert_eq!(
            parse_model_aliases(&under).len(),
            102,
            "an anchored array within the cap is read whole"
        );

        let over = build(500);
        assert!(
            over.len() > MAX_ALIAS_ARRAY_BYTES,
            "the over-cap fixture must actually exceed the cap"
        );
        assert!(
            parse_model_aliases(&over).is_empty(),
            "a candidate that runs past the cap is abandoned, which is what makes \
             the reader's overlap window provably big enough"
        );
    }

    /// The cap is exact TO THE BYTE: an anchored array of exactly
    /// [`MAX_ALIAS_ARRAY_BYTES`] is read, and the same array one byte longer is
    /// abandoned.
    ///
    /// The test above brackets the cap from ~1.2 KB and ~6.5 KB, which any
    /// off-by-one survives. This one matters because that exact byte is what the
    /// overlap-sufficiency proof rests on: [`SCAN_OVERLAP_BYTES`] is defined AS
    /// this cap, so "no match can exceed the overlap" holds only while the longest
    /// ADMITTED array is this many bytes and not one more.
    #[test]
    fn the_cap_admits_exactly_its_own_byte_count_and_no_more() {
        /// Head of every fixture below: the anchor, so the candidate qualifies.
        const HEAD: &str = "[\"opusplan\",";
        /// One padding element, including its trailing separator.
        const PADDING_ELEMENT: &str = "\"aaaaaaaaaa\",";
        /// The tail element's own bytes beside its `a`s: two quotes and the `]`.
        const TAIL_CHROME: usize = 3;

        /// An anchored, well-formed array of EXACTLY `total` bytes, with the
        /// element count it should parse to.
        fn anchored_array_of_exactly(total: usize) -> (Vec<u8>, usize) {
            let body = total - HEAD.len() - TAIL_CHROME;
            let mut padding = body / PADDING_ELEMENT.len();
            let mut tail_width = body % PADDING_ELEMENT.len();
            if tail_width == 0 {
                // A zero-width tail element would be `""`, which parses but reads
                // as an empty alias; borrow a whole padding element instead.
                padding -= 1;
                tail_width = PADDING_ELEMENT.len();
            }
            let mut raw = String::from(HEAD);
            for _ in 0..padding {
                raw.push_str(PADDING_ELEMENT);
            }
            raw.push('"');
            for _ in 0..tail_width {
                raw.push('a');
            }
            raw.push_str("\"]");
            let raw = raw.into_bytes();
            assert_eq!(
                raw.len(),
                total,
                "the fixture must be exactly {total} bytes"
            );
            (raw, padding + 2) // The anchor, the padding, and the tail.
        }

        let (at_cap, elements) = anchored_array_of_exactly(MAX_ALIAS_ARRAY_BYTES);
        assert_eq!(
            parse_model_aliases(&at_cap).len(),
            elements,
            "an array of exactly {MAX_ALIAS_ARRAY_BYTES} bytes is INSIDE the cap \
             and must be read whole"
        );

        let (over_cap, _) = anchored_array_of_exactly(MAX_ALIAS_ARRAY_BYTES + 1);
        assert!(
            parse_model_aliases(&over_cap).is_empty(),
            "one byte more is outside it, and admitting that byte would put a \
             match beyond the overlap the reader carries"
        );
    }

    /// The overlap is defined AS the cap, and the real array is far inside it.
    /// This is the arithmetic behind "a match can never be lost at a chunk
    /// boundary", kept as an assertion so a retune of either const has to face it.
    #[test]
    fn the_overlap_window_is_the_extractors_own_cap() {
        assert_eq!(
            SCAN_OVERLAP_BYTES, MAX_ALIAS_ARRAY_BYTES,
            "the boundary proof only holds while these are the same number"
        );

        let real_array = real_alias_array();
        assert!(
            real_array.len() < SCAN_OVERLAP_BYTES,
            "the live array is {} bytes against a {SCAN_OVERLAP_BYTES}-byte \
             overlap; if that ever inverts, boundary matches start disappearing",
            real_array.len()
        );
    }

    /// `$PATH` resolution ORDER and the empty-entry rule, pinned without touching
    /// the environment.
    #[test]
    fn path_candidates_append_the_program_in_path_order_and_skip_empty_entries() {
        let candidates = claude_path_candidates(OsStr::new("/first/bin::/second/bin"));
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/first/bin/claude"),
                PathBuf::from("/second/bin/claude"),
            ],
            "each entry gets the program appended, in order, and the EMPTY entry \
             is dropped rather than read as the process cwd"
        );

        assert!(
            claude_path_candidates(OsStr::new("")).is_empty(),
            "an empty PATH offers nowhere to look"
        );
    }

    /// The live alias array exactly as it appears in the binary, rebuilt from
    /// [`EXPECTED_ALIASES`] so a drift in the expectation can never leave a stale
    /// copy of it behind in a fixture.
    fn real_alias_array() -> String {
        format!(
            "[{}]",
            EXPECTED_ALIASES
                .iter()
                .map(|alias| format!("\"{alias}\""))
                .collect::<Vec<_>>()
                .join(",")
        )
    }

    /// A unique temp dir for the file-level scan tests, per the repo's isolated
    /// temp-dir convention. Never touches a real install.
    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "snapback-{tag}-{pid}-{nanos}",
            pid = std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// The reason the overlap window exists: an array that lands ACROSS a chunk
    /// boundary must still be found whole.
    ///
    /// The fixture places the array so the boundary falls in its middle — without
    /// the carried-over window each chunk would see only a fragment and the scan
    /// would return nothing.
    #[test]
    fn an_array_straddling_a_chunk_boundary_is_still_found() {
        let dir = temp_dir("model-aliases-straddle");
        let path = dir.join("claude");

        // Position the ALIAS array on the boundary, not the captured window that
        // contains it: the array sits in that window's tail, so centring the
        // WINDOW leaves the array wholly inside the second chunk and the test
        // passes with no overlap at all. Centring the array is the difference
        // between pinning the overlap and arranging its failure away.
        let window = String::from_utf8(CAPTURED_2_1_235.to_vec()).expect("fixture is ASCII");
        let array = real_alias_array();
        let array_in_window = window.find(&array).expect("the window carries the array");
        let padding = SCAN_CHUNK_BYTES - array_in_window - array.len() / 2;

        let mut blob = vec![b'x'; padding];
        blob.extend_from_slice(window.as_bytes());
        blob.extend(std::iter::repeat_n(b'x', SCAN_CHUNK_BYTES / 2));
        std::fs::write(&path, &blob).expect("write the fixture binary");

        let array_start = padding + array_in_window;
        let array_end = array_start + array.len();
        assert!(
            array_start < SCAN_CHUNK_BYTES && SCAN_CHUNK_BYTES < array_end,
            "the fixture must genuinely straddle: the array occupies \
             {array_start}..{array_end} and the boundary is at {SCAN_CHUNK_BYTES}"
        );

        assert_eq!(
            scan_file(&path),
            EXPECTED_ALIASES,
            "the array spans the {SCAN_CHUNK_BYTES}-byte boundary, so the carried \
             overlap window is the only thing that can keep it whole"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The scanner's fail-soft floor: a file that is not there, and a file with
    /// nothing to find, both answer EMPTY rather than erroring.
    #[test]
    fn an_absent_or_aliasless_file_scans_to_nothing() {
        let dir = temp_dir("model-aliases-missing");

        assert!(
            scan_file(&dir.join("not-here")).is_empty(),
            "an absent binary is no signal, never an error"
        );

        let path = dir.join("claude");
        std::fs::write(&path, vec![0_u8; SCAN_CHUNK_BYTES + 7]).expect("write the fixture binary");
        assert!(
            scan_file(&path).is_empty(),
            "a readable file with no array in it is also no signal"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// One turn of [`ScriptedReader`]: the window to hand over, or the error to
    /// raise instead of one.
    enum ReadStep {
        Chunk(Vec<u8>),
        Fail(ErrorKind),
    }

    /// A [`Read`] that hands over a STATED sequence of windows and then EOF.
    ///
    /// This is what the reader seam buys. Every rule below is a decision about a
    /// SEQUENCE of reads, and a temp file can only ever produce one such sequence:
    /// bytes, then EOF. It cannot put an array in an early window and nothing in a
    /// later one without being ~20 MB wide, and it cannot raise `Interrupted` or a
    /// mid-scan failure on demand at all.
    struct ScriptedReader {
        steps: std::collections::VecDeque<ReadStep>,
    }

    impl ScriptedReader {
        fn new(steps: Vec<ReadStep>) -> Self {
            Self {
                steps: steps.into(),
            }
        }
    }

    impl Read for ScriptedReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.steps.pop_front() {
                None => Ok(0), // EOF once the script runs out.
                Some(ReadStep::Chunk(bytes)) => {
                    assert!(!bytes.is_empty(), "an empty chunk would read as EOF");
                    assert!(bytes.len() <= buf.len(), "a window must fit one chunk");
                    buf[..bytes.len()].copy_from_slice(&bytes);
                    Ok(bytes.len())
                }
                Some(ReadStep::Fail(kind)) => Err(std::io::Error::from(kind)),
            }
        }
    }

    /// The CROSS-WINDOW fold: a find in an early window survives every later
    /// window that finds nothing.
    ///
    /// This is the shape of the real binary and nothing else — roughly 20 MB of
    /// bundle follows the alias array, so ~20 of the ~290 windows come after the
    /// hit and every one of them is empty. Keeping only the latest window's answer
    /// therefore returns EMPTY against the one file this module exists to read,
    /// while a fixture that puts the array in the LAST window (which is what a
    /// small temp file does) stays green throughout.
    #[test]
    fn an_early_windows_find_survives_a_later_empty_window() {
        /// The trailing read, sized only so it is plainly a second window.
        const LATER_WINDOW_BYTES: usize = 64;

        let array = real_alias_array();
        // Filler wide enough that the array falls OUT of the overlap carried into
        // the next read — that is what makes the later window empty.
        let filler = vec![b'x'; SCAN_OVERLAP_BYTES + 1];
        let later = vec![b'x'; LATER_WINDOW_BYTES];

        let mut second_window = filler[filler.len() - SCAN_OVERLAP_BYTES..].to_vec();
        second_window.extend_from_slice(&later);
        assert!(
            parse_model_aliases(&second_window).is_empty(),
            "premise: the second window — the carried overlap plus the next read — \
             must genuinely find nothing, else the fold is never asked to remember \
             anything and this test pins nothing at all"
        );

        let mut first = array.into_bytes();
        first.extend_from_slice(&filler);

        assert_eq!(
            scan_reader(ScriptedReader::new(vec![
                ReadStep::Chunk(first),
                ReadStep::Chunk(later),
            ])),
            EXPECTED_ALIASES,
            "the array was seen in the FIRST window and nowhere after it, so only \
             folding by length can still answer with it"
        );
    }

    /// A read error DISCARDS whatever was already found: a partial scan cannot say
    /// the array it happened to see was the longest one.
    ///
    /// Paired with the same first window followed by a clean EOF, so the emptiness
    /// is the discard rule's doing rather than a window that never carried the
    /// array.
    #[test]
    fn a_read_error_discards_what_an_earlier_window_already_found() {
        assert!(
            scan_reader(ScriptedReader::new(vec![
                ReadStep::Chunk(real_alias_array().into_bytes()),
                ReadStep::Fail(ErrorKind::PermissionDenied),
            ]))
            .is_empty(),
            "the scan stopped short, so what it saw is not an answer"
        );

        assert_eq!(
            scan_reader(ScriptedReader::new(vec![ReadStep::Chunk(
                real_alias_array().into_bytes()
            )])),
            EXPECTED_ALIASES,
            "premise: that very window DOES answer when the scan finishes, so the \
             emptiness above is the discard and not a bad fixture"
        );
    }

    /// `Interrupted` is the one error that is not a verdict about the file, so it
    /// is RETRIED — before a find and after one alike.
    #[test]
    fn an_interrupted_read_is_retried_rather_than_ending_the_scan() {
        assert_eq!(
            scan_reader(ScriptedReader::new(vec![
                ReadStep::Fail(ErrorKind::Interrupted),
                ReadStep::Chunk(real_alias_array().into_bytes()),
            ])),
            EXPECTED_ALIASES,
            "an interrupted first read says nothing about the file, so the array \
             arriving on the retry is still the answer"
        );

        assert_eq!(
            scan_reader(ScriptedReader::new(vec![
                ReadStep::Chunk(real_alias_array().into_bytes()),
                ReadStep::Fail(ErrorKind::Interrupted),
            ])),
            EXPECTED_ALIASES,
            "and unlike every other error it does not discard what was already \
             found"
        );
    }

    /// The `PATH` walk skips BOTH kinds of unusable hit and keeps going: one that
    /// is not executable, and one that will not canonicalize.
    ///
    /// Unix-only because there is no execute bit to set elsewhere, and the
    /// non-unix arm of [`is_executable_file`] exists to keep the crate compiling
    /// rather than to serve a shipped target.
    #[cfg(unix)]
    #[test]
    fn the_walk_skips_a_non_executable_hit_and_one_that_will_not_canonicalize() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("model-aliases-walk");
        let runnable = |name: &str| -> PathBuf {
            let path = dir.join(name);
            std::fs::write(&path, b"#!/bin/sh\n").expect("write the fixture binary");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("make the fixture executable");
            path
        };

        let not_executable = dir.join("first-claude");
        std::fs::write(&not_executable, b"#!/bin/sh\n").expect("write the fixture binary");
        std::fs::set_permissions(&not_executable, std::fs::Permissions::from_mode(0o644))
            .expect("keep the fixture non-executable");
        let unresolvable = runnable("second-claude");
        let good = runnable("third-claude");

        assert!(
            !is_executable_file(&not_executable),
            "premise: the FIRST candidate is disqualified by the executable filter"
        );
        assert!(
            is_executable_file(&unresolvable),
            "premise: the SECOND candidate passes that filter, so only the \
             canonicalize refusal can be what skips it"
        );

        let found = first_canonical_executable(
            vec![not_executable.clone(), unresolvable.clone(), good.clone()],
            |candidate| (candidate != unresolvable.as_path()).then(|| candidate.to_path_buf()),
        );
        assert_eq!(
            found,
            Some(good),
            "neither disqualifier may end the walk — a broken install early on \
             PATH must not hide a working one later"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A candidate must be a REGULAR FILE the OS would run: the execute bit alone
    /// is not enough (a directory carries it too, where it means "searchable"),
    /// and being a file alone is not enough either.
    #[cfg(unix)]
    #[test]
    fn only_a_regular_file_carrying_an_execute_bit_is_a_candidate() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("model-aliases-executable");

        let plain = dir.join("plain");
        std::fs::write(&plain, b"#!/bin/sh\n").expect("write the fixture binary");
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644))
            .expect("keep the fixture non-executable");
        assert!(
            !is_executable_file(&plain),
            "a readable file with no execute bit is not something the OS would run"
        );

        let executable = dir.join("executable");
        std::fs::write(&executable, b"#!/bin/sh\n").expect("write the fixture binary");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
            .expect("make the fixture executable");
        assert!(
            is_executable_file(&executable),
            "and the same file with the bit set IS, or nothing would ever be found"
        );

        assert!(
            std::fs::metadata(&dir)
                .expect("the temp dir exists")
                .permissions()
                .mode()
                & EXECUTABLE_BITS
                != 0,
            "premise: the directory carries an execute bit of its own, which is \
             why being a file has to be asked separately"
        );
        assert!(
            !is_executable_file(&dir),
            "a directory is searchable, never runnable"
        );

        assert!(
            !is_executable_file(&dir.join("absent")),
            "an absent path is simply not a candidate, never an error"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
