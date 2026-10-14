//! Snapback-owned persistent state: the per-session state map, whose only field
//! today is `hidden`.
//!
//! This is the ONLY persistent state snapback writes, and it lives in a SEPARATE,
//! snapback-owned directory ([`crate::config::state_dir`]) — NEVER inside the
//! read-only Claude store under `~/.claude/projects/`. A hidden session is a
//! VISIBILITY preference (hide the row from the board), not a status claim about
//! the agent: a hidden live session still reports live in the show-hidden view.
//!
//! On disk it is `sessions.json`, a JSON object keyed by session id whose values
//! are objects of per-session fields (`{"<id>": {"hidden": true}}`). It is read as
//! a `serde_json::Value`, never a typed struct, so an entry or a field this
//! version does not know — a newer snapback may have written it — survives every
//! save untouched.
//!
//! The split follows the pure-core / thin-driver rule (PATTERNS §3): parsing,
//! serializing and applying a [`HideDelta`] are pure and unit-tested;
//! [`load_hidden`] / [`save_hidden`] are the thin FS drivers and spawn no process.
//! Their two postures differ on purpose:
//!
//! - The display READ fails SOFT (PATTERNS §1): a missing, unreadable or
//!   malformed file is an empty set — nothing hidden — never a panic or an error,
//!   so the board always comes up. It takes no lock: every write lands by an
//!   atomic rename, so a reader only ever sees a whole file.
//! - The WRITE is a read-modify-write of ONE keystroke's delta under an exclusive
//!   lock, and it FAILS rather than overwrite a file it could not read. Writing
//!   the caller's whole set instead is the lost update this replaced: two windows
//!   each hold a snapshot, and the last save drops the other's hides. The atomic
//!   rename alone rules out a torn file, not that.
//!
//! The newline-delimited `hidden_sessions` file of older versions is MIGRATED,
//! never written: a load reads it while `sessions.json` is absent, and the first
//! save seeds the map from it under the lock, then removes it. Once
//! `sessions.json` exists the legacy file is never read again.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind};
use std::path::Path;

use serde_json::{Map, Value};

/// File name of the per-session state map inside snapback's state dir
/// ([`crate::config::state_dir`], `~/.config/snapback/state` by default). A plain,
/// greppable name so a user can inspect, hand-edit or delete it.
const STATE_FILE_NAME: &str = "sessions.json";

/// Sidecar every save takes an exclusive advisory lock on. Locking a sidecar
/// rather than `sessions.json` itself is what lets the save replace that file by
/// rename while the lock is held. NEVER deleted: a save that recreated it would
/// lock a different file than a save still holding the old one, and the two
/// would race again.
const LOCK_FILE_NAME: &str = "sessions.json.lock";

/// File name of the legacy newline-delimited hidden-id set that older versions
/// wrote. Read only to migrate it; see the module doc.
const LEGACY_FILE_NAME: &str = "hidden_sessions";

/// Suffix for the sibling temp file the atomic write renames over the target. It
/// sits in the SAME dir as the target so the rename stays within one filesystem
/// (a cross-device rename is not atomic).
const TMP_SUFFIX: &str = ".tmp";

/// The per-session field that marks a session hidden. Only the JSON boolean
/// `true` hides; any other value is one this version does not understand.
const HIDDEN_FIELD: &str = "hidden";

/// The parsed state file: session id -> object of per-session fields.
///
/// serde_json is built WITHOUT its `preserve_order` feature, so this is a
/// `BTreeMap` and serializes its keys sorted at every level; that is what keeps
/// an unchanged map byte-stable (pinned by a test, which goes red if the feature
/// is ever enabled).
pub type StateMap = Map<String, Value>;

/// One hide keystroke's change: every id in `ids` goes to `hidden`.
///
/// A save applies THIS to what disk holds at that moment, never the caller's
/// whole set, so it cannot undo a change another window saved meanwhile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HideDelta {
    /// The session ids the keystroke flips.
    pub ids: Vec<String>,
    /// The state they all go to: `true` hides, `false` un-hides.
    pub hidden: bool,
}

impl HideDelta {
    /// Apply this delta to an in-memory hidden-id set.
    pub fn apply_to(&self, set: &mut HashSet<String>) {
        for id in &self.ids {
            if self.hidden {
                set.insert(id.clone());
            } else {
                set.remove(id);
            }
        }
    }
}

/// Parse a `sessions.json` body. `None` when the body is not JSON or its top
/// level is not an object: the caller decides whether that reads as "nothing
/// hidden" (display) or as a reason to refuse the write (save).
pub fn parse_state(body: &str) -> Option<StateMap> {
    match serde_json::from_str(body) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// The ids `map` marks hidden. Fail-soft: an entry that is not an object, or
/// whose `hidden` is anything but the JSON boolean `true`, is not hidden.
pub fn hidden_ids(map: &StateMap) -> HashSet<String> {
    map.iter()
        .filter(|(_, entry)| entry.get(HIDDEN_FIELD) == Some(&Value::Bool(true)))
        .map(|(id, _)| id.clone())
        .collect()
}

/// Apply `delta` to `map`, keeping every other entry and field as it is.
///
/// A hide sets `hidden: true`, keeping the entry's other fields. An entry that
/// is not an object has no field to keep, so a hide replaces it. An un-hide
/// removes the `hidden` field and drops the entry only when that leaves it
/// empty; it leaves a non-object entry alone, since that entry was never read
/// as hidden.
pub fn apply_delta(map: &mut StateMap, delta: &HideDelta) {
    for id in &delta.ids {
        if delta.hidden {
            let entry = map
                .entry(id.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(Map::new());
            }
            if let Value::Object(fields) = entry {
                fields.insert(HIDDEN_FIELD.to_string(), Value::Bool(true));
            }
        } else if let Some(Value::Object(fields)) = map.get_mut(id) {
            fields.remove(HIDDEN_FIELD);
            if fields.is_empty() {
                map.remove(id);
            }
        }
    }
}

/// Serialize `map` as pretty-printed JSON with a trailing newline, keys sorted
/// (see [`StateMap`]), so re-saving an unchanged map writes identical bytes.
pub fn serialize_state(map: &StateMap) -> serde_json::Result<String> {
    let mut body = serde_json::to_string_pretty(map)?;
    body.push('\n');
    Ok(body)
}

/// Parse a legacy newline-delimited `hidden_sessions` body into a set. Fail-soft:
/// each line is trimmed, blank lines are skipped, and duplicates collapse into
/// the set — a whitespace-noisy or partially-corrupt file can never panic, it
/// just yields whatever ids it could recover.
pub fn parse_hidden(body: &str) -> HashSet<String> {
    body.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Load the hidden-id set from `dir`. Fail-soft: a missing, unreadable or
/// malformed `sessions.json` yields an EMPTY set, never an error — hidden state
/// is a convenience, so a read failure must never block the board. While
/// `sessions.json` does not exist, the legacy file answers instead (same
/// fail-soft rules). Never writes, and takes no lock.
pub fn load_hidden(dir: &Path) -> HashSet<String> {
    match fs::read_to_string(dir.join(STATE_FILE_NAME)) {
        Ok(body) => parse_state(&body).map_or_else(HashSet::new, |map| hidden_ids(&map)),
        Err(err) if err.kind() == ErrorKind::NotFound => {
            fs::read_to_string(dir.join(LEGACY_FILE_NAME))
                .map_or_else(|_| HashSet::new(), |body| parse_hidden(&body))
        }
        Err(_) => HashSet::new(),
    }
}

/// Save one keystroke's `delta` to `dir` and return the hidden-id set disk holds
/// afterwards — this window's change merged with every other window's.
///
/// Under an exclusive lock on the sidecar lock file: re-read `sessions.json`,
/// apply `delta`, write a sibling temp file and rename it over the target. A
/// missing `sessions.json` starts from the legacy file's ids (or empty), and the
/// legacy file is removed once the new file is in place. Creates `dir` if absent;
/// spawns no process.
///
/// Errors — and writes NOTHING — when it cannot read what it would replace: an
/// unreadable `sessions.json`, one that is not a JSON object, or, while
/// migrating, an unreadable legacy file. Overwriting any of them would destroy
/// hides it never saw.
///
/// The lock blocks only while another snapback process runs this same short
/// read-modify-write; the OS releases it when that process's handle closes,
/// including when it dies.
pub fn save_hidden(dir: &Path, delta: &HideDelta) -> io::Result<HashSet<String>> {
    fs::create_dir_all(dir)?;
    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(dir.join(LOCK_FILE_NAME))?;
    lock.lock()?;

    let target = dir.join(STATE_FILE_NAME);
    let (mut map, migrating) = match fs::read_to_string(&target) {
        Ok(body) => match parse_state(&body) {
            Some(map) => (map, false),
            None => {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    format!("{STATE_FILE_NAME} is not a JSON object"),
                ))
            }
        },
        Err(err) if err.kind() == ErrorKind::NotFound => (legacy_seed(dir)?, true),
        Err(err) => return Err(in_file(STATE_FILE_NAME, err)),
    };
    apply_delta(&mut map, delta);

    let tmp = dir.join(format!(
        "{STATE_FILE_NAME}{TMP_SUFFIX}.{}",
        std::process::id()
    ));
    fs::write(&tmp, serialize_state(&map)?)?;
    fs::rename(&tmp, &target)?;
    if migrating {
        // Best-effort: `sessions.json` now exists, so a legacy file left behind
        // is never read again, and failing a save that already landed would
        // report a hide as lost when it is on disk.
        let _ = fs::remove_file(dir.join(LEGACY_FILE_NAME));
    }
    Ok(hidden_ids(&map))
}

/// The map a first save starts from: the legacy file's ids, each hidden, or
/// empty when there is no legacy file. An unreadable legacy file is an error, not
/// an empty seed, because the migration removes it after the write.
fn legacy_seed(dir: &Path) -> io::Result<StateMap> {
    let mut map = StateMap::new();
    match fs::read_to_string(dir.join(LEGACY_FILE_NAME)) {
        Ok(body) => apply_delta(
            &mut map,
            &HideDelta {
                ids: parse_hidden(&body).into_iter().collect(),
                hidden: true,
            },
        ),
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(in_file(LEGACY_FILE_NAME, err)),
    }
    Ok(map)
}

/// `err`, prefixed with the state file it came from, so the board status names
/// the file to fix rather than a bare OS message.
fn in_file(name: &str, err: io::Error) -> io::Error {
    io::Error::new(err.kind(), format!("{name}: {err}"))
}

/// Save `ids` hidden in `dir` through [`save_hidden`], for a test whose board
/// must START from, or re-read on a reload, a hidden set that is really on disk.
#[cfg(test)]
pub(crate) fn seed_hidden(dir: &Path, ids: &[&str]) {
    let delta = HideDelta {
        ids: ids.iter().map(|id| (*id).to_string()).collect(),
        hidden: true,
    };
    save_hidden(dir, &delta).expect("seed the hidden set");
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    // The file names are the USER-FACING contract (a user greps or deletes them,
    // and the legacy name is what older versions left on disk), so the tests
    // below spell them LITERALLY — never via the consts, which would move in
    // lockstep with a rename and pass vacuously.

    /// A unique, isolated temp dir under `std::env::temp_dir()` — NEVER the real
    /// data dir. Mirrors the `snapback-<tag>-<pid>-<nanos>` convention used across
    /// the crate's tests.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "snapback-hidden-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn set_of(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn delta(ids: &[&str], hidden: bool) -> HideDelta {
        HideDelta {
            ids: ids.iter().map(|s| s.to_string()).collect(),
            hidden,
        }
    }

    fn map_of(body: &str) -> StateMap {
        parse_state(body).expect("a JSON object")
    }

    // --- parse_state / hidden_ids / serialize_state -----------------------

    #[test]
    fn serialize_then_parse_round_trips_a_map() {
        let map = map_of(r#"{"s1": {"hidden": true}, "s2": {"hidden": true, "note": [1, 2]}}"#);
        let body = serialize_state(&map).unwrap();
        assert_eq!(parse_state(&body), Some(map), "a map survives a trip");
    }

    #[test]
    fn serialize_emits_sorted_keys_and_is_byte_stable() {
        let map = map_of(r#"{"c": {"z": 1, "hidden": true}, "a": {"hidden": true}, "b": {}}"#);
        let body = serialize_state(&map).unwrap();
        assert_eq!(
            body,
            "{\n  \"a\": {\n    \"hidden\": true\n  },\n  \"b\": {},\n  \"c\": {\n    \"hidden\": true,\n    \"z\": 1\n  }\n}\n",
            "keys are sorted at every level, pretty-printed, newline-terminated"
        );
        assert_eq!(
            serialize_state(&parse_state(&body).unwrap()).unwrap(),
            body,
            "re-saving an unchanged map writes identical bytes"
        );
    }

    #[test]
    fn parse_state_rejects_a_malformed_body_and_a_non_object_top_level() {
        for body in [
            "", "not json", "{\"a\": ", "[\"s1\"]", "\"s1\"", "42", "null",
        ] {
            assert_eq!(parse_state(body), None, "{body:?} is not a state map");
        }
    }

    #[test]
    fn hidden_ids_skips_non_object_entries_and_non_true_hidden_values() {
        let map = map_of(
            r#"{
                "yes": {"hidden": true},
                "false": {"hidden": false},
                "string": {"hidden": "true"},
                "one": {"hidden": 1},
                "null": {"hidden": null},
                "absent": {"other": true},
                "bare-true": true,
                "array": ["hidden"],
                "text": "hidden"
            }"#,
        );
        assert_eq!(
            hidden_ids(&map),
            set_of(&["yes"]),
            "only an object entry with the JSON boolean `true` is hidden"
        );
    }

    // --- apply_delta -------------------------------------------------------

    #[test]
    fn a_hide_sets_hidden_true_and_keeps_every_other_field_and_entry() {
        let mut map = map_of(
            r#"{"s1": {"pinned": 3}, "s2": {"hidden": false, "x": "y"}, "other": {"future": {"a": 1}}}"#,
        );
        apply_delta(&mut map, &delta(&["s1", "s2", "new"], true));
        assert_eq!(
            Value::Object(map),
            serde_json::json!({
                "s1": {"hidden": true, "pinned": 3},
                "s2": {"hidden": true, "x": "y"},
                "new": {"hidden": true},
                "other": {"future": {"a": 1}},
            }),
            "a hide adds `hidden: true` and touches nothing else"
        );
    }

    #[test]
    fn a_hide_replaces_a_non_object_entry() {
        let mut map = map_of(r#"{"s1": "garbage", "s2": [1]}"#);
        apply_delta(&mut map, &delta(&["s1"], true));
        assert_eq!(
            Value::Object(map),
            serde_json::json!({"s1": {"hidden": true}, "s2": [1]}),
            "the hidden entry becomes an object; the untouched one stays as it was"
        );
    }

    #[test]
    fn an_unhide_removes_hidden_and_drops_only_the_entries_it_empties() {
        let mut map = map_of(
            r#"{
                "bare": {"hidden": true},
                "kept": {"hidden": true, "pinned": 3},
                "odd": {"hidden": "yes"},
                "text": "garbage",
                "other": {"hidden": true}
            }"#,
        );
        apply_delta(
            &mut map,
            &delta(&["bare", "kept", "odd", "text", "absent"], false),
        );
        assert_eq!(
            Value::Object(map),
            serde_json::json!({
                "kept": {"pinned": 3},
                "text": "garbage",
                "other": {"hidden": true},
            }),
            "an emptied entry is dropped, a field-bearing one keeps its fields, \
             and entries outside the delta are untouched"
        );
    }

    #[test]
    fn apply_to_flips_the_ids_in_an_in_memory_set() {
        let mut set = set_of(&["keep", "s2"]);
        delta(&["s1", "s2"], true).apply_to(&mut set);
        assert_eq!(set, set_of(&["keep", "s1", "s2"]));
        delta(&["s1", "s2"], false).apply_to(&mut set);
        assert_eq!(set, set_of(&["keep"]));
    }

    // --- parse_hidden (legacy) ----------------------------------------------

    #[test]
    fn parse_hidden_is_fail_soft_over_blanks_whitespace_and_dupes() {
        // Leading/trailing whitespace, blank lines, and a duplicate id.
        let body = "  a  \n\n b\n\n\na\n   \n";
        assert_eq!(
            parse_hidden(body),
            set_of(&["a", "b"]),
            "blank lines are skipped, ids are trimmed, and duplicates collapse"
        );
    }

    // --- load_hidden / save_hidden ------------------------------------------

    #[test]
    fn save_then_load_round_trips_through_sessions_json() {
        let base = unique_temp_dir("roundtrip");
        // `state/` does not exist yet, so save must create the dir.
        let dir = base.join("state");

        let merged = save_hidden(&dir, &delta(&["s1", "s2"], true)).expect("save");
        assert_eq!(
            merged,
            set_of(&["s1", "s2"]),
            "save returns what disk holds"
        );
        assert_eq!(load_hidden(&dir), set_of(&["s1", "s2"]));
        assert_eq!(
            std::fs::read_to_string(dir.join("sessions.json")).unwrap(),
            "{\n  \"s1\": {\n    \"hidden\": true\n  },\n  \"s2\": {\n    \"hidden\": true\n  }\n}\n",
            "the state lands in `sessions.json` as a per-session map"
        );

        let merged = save_hidden(&dir, &delta(&["s1"], false)).expect("save");
        assert_eq!(merged, set_of(&["s2"]));
        assert_eq!(load_hidden(&dir), set_of(&["s2"]));
        assert!(
            dir.join("sessions.json.lock").exists(),
            "the lock file stays: deleting it would let two saves race again"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Two saves of different hides — two windows, each knowing only its own —
    /// merge on disk: a save carries its delta, never a snapshot to write over
    /// the other's. The two-window board test in `tui::app` is the end-to-end
    /// regression for the reported lost update.
    #[test]
    fn saves_of_different_hides_merge_on_disk() {
        let dir = unique_temp_dir("two-saves");

        let first = save_hidden(&dir, &delta(&["x"], true)).expect("first save");
        let second = save_hidden(&dir, &delta(&["y"], true)).expect("second save");

        assert_eq!(first, set_of(&["x"]));
        assert_eq!(second, set_of(&["x", "y"]), "the later save sees both");
        assert_eq!(load_hidden(&dir), set_of(&["x", "y"]), "both hides persist");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The lock is what serializes concurrent saves: without it, two
    /// read-modify-writes interleave and one overwrites the other's hide.
    #[test]
    fn concurrent_saves_from_many_threads_lose_no_hide() {
        const THREADS: usize = 8;
        const SAVES_PER_THREAD: usize = 40;
        let dir = unique_temp_dir("concurrent");

        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    for s in 0..SAVES_PER_THREAD {
                        save_hidden(&dir, &delta(&[&format!("t{t}-s{s}")], true)).expect("save");
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("saver thread");
        }

        assert_eq!(
            load_hidden(&dir).len(),
            THREADS * SAVES_PER_THREAD,
            "every concurrent hide is on disk"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_save_keeps_entries_and_fields_it_does_not_understand() {
        let dir = unique_temp_dir("unknown");
        std::fs::write(
            dir.join("sessions.json"),
            r#"{"s1": {"pinned": true}, "later": {"color": "red"}, "odd": 7}"#,
        )
        .unwrap();

        save_hidden(&dir, &delta(&["s1"], true)).expect("save");

        let on_disk = map_of(&std::fs::read_to_string(dir.join("sessions.json")).unwrap());
        assert_eq!(
            Value::Object(on_disk),
            serde_json::json!({
                "s1": {"hidden": true, "pinned": true},
                "later": {"color": "red"},
                "odd": 7,
            })
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_save_refuses_to_overwrite_a_file_that_is_not_a_json_object() {
        for body in ["not json", "[\"s1\"]", ""] {
            let dir = unique_temp_dir("refuse");
            std::fs::write(dir.join("sessions.json"), body).unwrap();

            let err = save_hidden(&dir, &delta(&["s1"], true)).expect_err("refused");
            assert_eq!(err.kind(), ErrorKind::InvalidData);
            assert_eq!(
                std::fs::read_to_string(dir.join("sessions.json")).unwrap(),
                body,
                "{body:?} is left exactly as it was"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn a_save_refuses_to_overwrite_a_file_it_cannot_read() {
        let dir = unique_temp_dir("unreadable");
        // Present, but reading it fails with an I/O error other than "not
        // found" (not UTF-8), while a rename over it would still succeed — so
        // only the read check can stop the overwrite.
        let unreadable = [0xff, 0xfe, b'{', b'}'];
        std::fs::write(dir.join("sessions.json"), unreadable).unwrap();
        std::fs::write(dir.join("hidden_sessions"), "legacy\n").unwrap();

        assert!(load_hidden(&dir).is_empty(), "the display read fails soft");
        assert!(save_hidden(&dir, &delta(&["s1"], true)).is_err());
        assert_eq!(
            std::fs::read(dir.join("sessions.json")).unwrap(),
            unreadable,
            "nothing was renamed over it"
        );
        assert!(
            dir.join("hidden_sessions").exists(),
            "and nothing migrated or removed the legacy file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_is_empty_when_the_file_is_missing_or_malformed() {
        let dir = unique_temp_dir("missing");
        assert!(load_hidden(&dir).is_empty(), "no file: nothing hidden");
        for body in ["not json", "[\"s1\"]", "{\"s1\": true}"] {
            std::fs::write(dir.join("sessions.json"), body).unwrap();
            assert!(load_hidden(&dir).is_empty(), "{body:?}: nothing hidden");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- legacy migration ----------------------------------------------------

    #[test]
    fn load_reads_the_legacy_file_while_sessions_json_is_missing() {
        let dir = unique_temp_dir("legacy-load");
        std::fs::write(dir.join("hidden_sessions"), "good\n\n   \nalso-good\n").unwrap();

        assert_eq!(
            load_hidden(&dir),
            set_of(&["good", "also-good"]),
            "the legacy ids load, a blank/whitespace line skipped"
        );
        assert!(
            !dir.join("sessions.json").exists(),
            "a load never writes, so it never migrates"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_first_save_migrates_the_legacy_ids_then_removes_the_legacy_file() {
        let dir = unique_temp_dir("legacy-save");
        std::fs::write(dir.join("hidden_sessions"), "old-1\nold-2\n").unwrap();

        let merged = save_hidden(&dir, &delta(&["new", "old-2"], true)).expect("save");

        assert_eq!(merged, set_of(&["old-1", "old-2", "new"]));
        let on_disk = map_of(&std::fs::read_to_string(dir.join("sessions.json")).unwrap());
        assert_eq!(
            hidden_ids(&on_disk),
            set_of(&["old-1", "old-2", "new"]),
            "`sessions.json` holds the legacy ids plus the delta"
        );
        assert!(
            !dir.join("hidden_sessions").exists(),
            "the legacy file is removed once the new file is in place"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_first_save_can_unhide_a_legacy_id() {
        let dir = unique_temp_dir("legacy-unhide");
        std::fs::write(dir.join("hidden_sessions"), "old-1\nold-2\n").unwrap();

        let merged = save_hidden(&dir, &delta(&["old-1"], false)).expect("save");

        assert_eq!(merged, set_of(&["old-2"]));
        assert_eq!(load_hidden(&dir), set_of(&["old-2"]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_existing_sessions_json_ignores_the_legacy_file() {
        let dir = unique_temp_dir("legacy-ignored");
        std::fs::write(dir.join("sessions.json"), r#"{"json": {"hidden": true}}"#).unwrap();
        std::fs::write(dir.join("hidden_sessions"), "legacy\n").unwrap();

        assert_eq!(load_hidden(&dir), set_of(&["json"]), "load ignores it");
        let merged = save_hidden(&dir, &delta(&["new"], true)).expect("save");
        assert_eq!(
            merged,
            set_of(&["json", "new"]),
            "a save seeds nothing from it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_legacy_file_fails_the_first_save_and_survives() {
        let dir = unique_temp_dir("legacy-unreadable");
        // Present, but not UTF-8: the display read takes it as nothing hidden,
        // and the migration must not take that as licence to remove it.
        std::fs::write(dir.join("hidden_sessions"), [0xff, 0xfe, b'\n']).unwrap();

        assert!(load_hidden(&dir).is_empty(), "the display read fails soft");
        assert!(save_hidden(&dir, &delta(&["s1"], true)).is_err());
        assert!(!dir.join("sessions.json").exists(), "nothing was written");
        assert!(dir.join("hidden_sessions").exists(), "nor removed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
