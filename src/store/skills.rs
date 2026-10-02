//! The AGENT listing of ONE session file, with descriptions: a REPLY's fallback
//! for the compose pick list's `@` agents until claude's own list lands.
//!
//! [`Listing`] is the one shape both of the pick list's sources produce — claude's
//! `initialize` handshake (`src/claude_catalog.rs`) and this transcript reader,
//! whose `commands` are always empty.
//!
//! `skill_listing` records are deliberately NOT read: they list what the MODEL may
//! invoke, so they include the `user-invocable: false` skills claude's `/` menu
//! hides and omit the `disable-model-invocation: true` ones it shows, with no
//! per-skill flag to tell them apart. A `/` lists claude's catalog alone
//! (`docs/agents/CLAUDE_CLI.md`, "The `initialize` control handshake").
//!
//! Read on demand from a single session file; deliberately NOT part of
//! `parse_file`/`Session` (the same names would be copied onto every row)
//! and NOT cached in `SessionStore`, so it can never decide which files exist.
//! Pure over the bytes it is given: nothing here shells out (AGENTS.md PURE,
//! GIT-FREE STORE CORE). Parsing is FAIL-SOFT over `serde_json::Value`
//! (AGENTS.md FAIL-SOFT parsing): any shape mismatch contributes nothing, and a
//! missing or unreadable file is an empty listing. The list is a convenience;
//! typing a command is never blocked.
//!
//! Source (`attachment` records; shape seen from claude 2.1.235 to 2.1.284):
//! `{"type":"agent_listing_delta","addedTypes":[...],"addedLines":["- <name>:
//! <description> (Tools: ...)"],"removedTypes":[...],"isInitial":bool}` — a
//! stateful delta stream, folded in file order; an `isInitial: true` record is a
//! full listing and restarts the set (a file can carry more than one).

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde_json::Value;

/// Cheap prefilter marker (and the `attachment.type`) of an agent listing delta:
/// a line without it cannot be one, so it is never JSON-parsed. Plain
/// `str::contains` (`memchr` stays in `search.rs`).
const AGENT_LISTING_MARKER: &str = "agent_listing_delta";

/// The bullet claude opens every listing line with (`- <name>: <description>`).
const LISTING_LINE_PREFIX: &str = "- ";

/// What separates a name from its description on a listing line. A bare
/// `- <name>` line has no description.
const NAME_DESCRIPTION_SEPARATOR: char = ':';

/// The tool list claude appends to every `addedLines` entry (2787/2787 entries
/// in a local store, claude 2.1.235–2.1.284). The pick list shows the
/// description alone, so the LAST occurrence is cut when the line ends with `)`.
const AGENT_TOOLS_SUFFIX: &str = " (Tools: ";

/// One pick-list row's data: a command or agent name, and what claude says it
/// does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListingEntry {
    pub name: String,
    /// Already passed through [`normalize_description`]; `None` when the source
    /// gave none.
    pub description: Option<String>,
}

/// Everything the pick list can offer for one folder: `/` commands (skills
/// included) and `@` agents, each sorted by name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    pub commands: Vec<ListingEntry>,
    pub agents: Vec<ListingEntry>,
}

/// A description safe to draw in one table cell: every control character is
/// dropped (ESC included, so no raw escape can reach a cell — AGENTS.md
/// TERMINAL-SAFE STYLING), whitespace runs — the control whitespace `\n`, `\t`
/// and `\r` among them — fold to one space, and the ends are trimmed. Nothing
/// left is `None`.
#[must_use]
pub fn normalize_description(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut gap = false;
    for c in raw.chars() {
        if c.is_whitespace() {
            gap = !out.is_empty();
        } else if !c.is_control() {
            if gap {
                out.push(' ');
                gap = false;
            }
            out.push(c);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The agents listed by the `agent_listing_delta` records among `lines`, sorted
/// by name; `commands` is always empty (see the module doc).
///
/// Agents are the delta fold described in the module doc: per record, an
/// `isInitial: true` clears the set, then `addedTypes` are added (described by
/// that record's `addedLines`), then `removedTypes` are removed.
pub fn listing_from_lines<'a>(lines: impl IntoIterator<Item = &'a str>) -> Listing {
    let mut agents: BTreeMap<String, Option<String>> = BTreeMap::new();
    for line in lines {
        if !line.contains(AGENT_LISTING_MARKER) {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("attachment") {
            continue;
        }
        let Some(attachment) = value.get("attachment") else {
            continue;
        };
        if attachment.get("type").and_then(Value::as_str) == Some(AGENT_LISTING_MARKER) {
            fold_agent_delta(attachment, &mut agents);
        }
    }
    Listing {
        commands: Vec::new(),
        agents: into_entries(agents),
    }
}

/// Reads `file` line by line (a non-UTF-8 line is skipped, as in `parse_file`)
/// and returns its listing; an unreadable or missing file gives an empty one.
pub fn read_listing(file: &Path) -> Listing {
    let Ok(f) = File::open(file) else {
        return Listing::default();
    };
    let mut lines = Vec::new();
    for line in BufReader::new(f).lines() {
        match line {
            Ok(l) => lines.push(l),
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => continue,
            // A read failing part-way: stop rather than spin on a persistent error.
            Err(_) => break,
        }
    }
    listing_from_lines(lines.iter().map(String::as_str))
}

fn fold_agent_delta(attachment: &Value, agents: &mut BTreeMap<String, Option<String>>) {
    if attachment.get("isInitial").and_then(Value::as_bool) == Some(true) {
        agents.clear();
    }
    if let Some(added) = attachment.get("addedTypes").and_then(Value::as_array) {
        let names = string_items(added);
        // An array in every record observed; one `\n`-joined string is tolerated.
        let lines: Vec<&str> = match attachment.get("addedLines") {
            Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
            Some(Value::String(joined)) => joined.split('\n').collect(),
            _ => Vec::new(),
        };
        let described = descriptions(lines, &names, strip_tools_suffix);
        for name in names {
            agents.insert(name.to_owned(), described.get(name).cloned());
        }
    }
    if let Some(removed) = attachment.get("removedTypes").and_then(Value::as_array) {
        for name in string_items(removed) {
            agents.remove(name);
        }
    }
}

/// The trimmed, non-empty strings of a JSON array; anything else is skipped.
fn string_items(list: &[Value]) -> Vec<&str> {
    list.iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect()
}

/// Descriptions for `names` from listing lines shaped `- <name>: <description>`.
///
/// Each line is matched against `names` LONGEST FIRST, so a line
/// `- plugin-x:deploy: …` is never read as name `plugin-x` with a description
/// `deploy: …`. A line equal to a name has no description; `clean` trims what
/// the source appends before [`normalize_description`] runs; within one record
/// the first non-empty description per name wins. Lines naming none of `names`
/// are ignored.
fn descriptions<'n, 'l>(
    lines: impl IntoIterator<Item = &'l str>,
    names: &[&'n str],
    clean: fn(&str) -> &str,
) -> HashMap<&'n str, String> {
    let mut by_length = names.to_vec();
    by_length.sort_by_key(|name| std::cmp::Reverse(name.len()));
    let mut out = HashMap::new();
    for line in lines {
        let line = line.trim();
        let line = line.strip_prefix(LISTING_LINE_PREFIX).unwrap_or(line);
        for &name in &by_length {
            let Some(rest) = line.strip_prefix(name) else {
                continue;
            };
            if rest.is_empty() {
                break;
            }
            let Some(raw) = rest.strip_prefix(NAME_DESCRIPTION_SEPARATOR) else {
                continue;
            };
            if let Some(description) = normalize_description(clean(raw)) {
                out.entry(name).or_insert(description);
            }
            break;
        }
    }
    out
}

/// `raw` without its trailing ` (Tools: …)` list: cut at the LAST
/// [`AGENT_TOOLS_SUFFIX`], and only when the text ends with `)`.
fn strip_tools_suffix(raw: &str) -> &str {
    let trimmed = raw.trim_end();
    if !trimmed.ends_with(')') {
        return raw;
    }
    trimmed
        .rfind(AGENT_TOOLS_SUFFIX)
        .map_or(raw, |cut| &trimmed[..cut])
}

fn into_entries(map: BTreeMap<String, Option<String>>) -> Vec<ListingEntry> {
    map.into_iter()
        .map(|(name, description)| ListingEntry { name, description })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn names(entries: &[ListingEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    fn entry(name: &str, description: Option<&str>) -> ListingEntry {
        ListingEntry {
            name: name.to_owned(),
            description: description.map(str::to_owned),
        }
    }

    /// A well-formed `skill_listing` attachment is the MODEL's skill list, which
    /// offers skills claude's own `/` menu hides, so it is never read.
    #[test]
    fn a_skill_listing_record_contributes_nothing() {
        let record = skill_record(
            r#"["sbping","keybindings-help"]"#,
            r#""- sbping: Answers pong\n- keybindings-help: Customize keyboard shortcuts""#,
        );
        assert_eq!(listing_from_lines([record.as_str()]), Listing::default());
    }

    #[test]
    fn no_records_gives_empty() {
        assert_eq!(
            listing_from_lines(std::iter::empty::<&str>()),
            Listing::default()
        );
    }

    #[test]
    fn missing_file_gives_empty() {
        let p = PathBuf::from("/nonexistent/snapback-skills/none.jsonl");
        assert_eq!(read_listing(&p), Listing::default());
    }

    fn skill_record(names: &str, content: &str) -> String {
        format!(
            r#"{{"type":"attachment","attachment":{{"type":"skill_listing","names":{names},"content":{content}}}}}"#
        )
    }

    fn agent_record(added: &str, lines: &str, removed: &str, initial: bool) -> String {
        format!(
            r#"{{"type":"attachment","attachment":{{"type":"agent_listing_delta","addedTypes":{added},"addedLines":{lines},"removedTypes":{removed},"isInitial":{initial}}}}}"#
        )
    }

    /// Each listing line is matched against the record's names LONGEST FIRST, so
    /// `- plugin-x:deploy: …` describes `plugin-x:deploy`, never `plugin-x`.
    #[test]
    fn agent_descriptions_match_names_longest_first() {
        let record = agent_record(
            r#"["plugin-x","plugin-x:deploy","bare","silent"]"#,
            r#"["- plugin-x:deploy: Deploy it (Tools: Read)","- plugin-x: The base","- bare"]"#,
            "[]",
            true,
        );
        assert_eq!(
            listing_from_lines([record.as_str()]).agents,
            vec![
                entry("bare", None),
                entry("plugin-x", Some("The base")),
                entry("plugin-x:deploy", Some("Deploy it")),
                entry("silent", None),
            ]
        );
    }

    #[test]
    fn normalize_description_drops_controls_and_folds_whitespace() {
        assert_eq!(
            normalize_description("  red \u{1b}[31mtext\u{1b}[0m\tand\n\nmore\u{7}bell\r\n "),
            Some("red [31mtext[0m and morebell".to_owned())
        );
        assert_eq!(normalize_description("\u{1b}\u{7} \n\t"), None);
        assert_eq!(normalize_description(""), None);
    }

    #[test]
    fn agent_descriptions_are_normalized() {
        let agent = agent_record(
            r#"["a","c"]"#,
            r#"["- a: first\nsecond \u0007line (Tools: Read)","- c: say \u001b[1mhi\u001b[0m\tloud (Tools: Read)"]"#,
            "[]",
            true,
        );
        assert_eq!(
            listing_from_lines([agent.as_str()]).agents,
            vec![
                entry("a", Some("first second line")),
                entry("c", Some("say [1mhi[0m loud")),
            ]
        );
    }

    #[test]
    fn agents_fold_an_initial_listing_then_a_removal_delta() {
        let initial = agent_record(
            r#"["b-agent","a-agent"]"#,
            r#"["- a-agent: Does A (Tools: Read)","- b-agent: Does B (Tools: All tools)"]"#,
            "[]",
            true,
        );
        let removal = agent_record("[]", "[]", r#"["b-agent"]"#, false);
        assert_eq!(
            listing_from_lines([initial.as_str()]).agents,
            vec![
                entry("a-agent", Some("Does A")),
                entry("b-agent", Some("Does B"))
            ]
        );
        assert_eq!(
            listing_from_lines([initial.as_str(), removal.as_str()]).agents,
            vec![entry("a-agent", Some("Does A"))]
        );
    }

    #[test]
    fn a_later_initial_agent_listing_resets_the_set() {
        let first = agent_record(r#"["old"]"#, r#"["- old: Old one"]"#, "[]", true);
        let delta = agent_record(r#"["extra"]"#, r#"["- extra: Extra"]"#, "[]", false);
        let second = agent_record(r#"["new"]"#, r#"["- new: New one"]"#, "[]", true);
        assert_eq!(
            listing_from_lines([first.as_str(), delta.as_str(), second.as_str()]).agents,
            vec![entry("new", Some("New one"))]
        );
        assert_eq!(
            names(&listing_from_lines([first.as_str(), delta.as_str()]).agents),
            vec!["extra", "old"]
        );
    }

    #[test]
    fn the_last_tools_suffix_is_stripped_only_at_the_end() {
        let record = agent_record(
            r#"["a","b","c","d"]"#,
            r#"["- a: Plain (Tools: Read, Glob)","- b: see (Tools: docs) (Tools: Read)","- c: uses (Tools: X) inline","- d: (Tools: Read)"]"#,
            "[]",
            true,
        );
        assert_eq!(
            listing_from_lines([record.as_str()]).agents,
            vec![
                entry("a", Some("Plain")),
                entry("b", Some("see (Tools: docs)")),
                entry("c", Some("uses (Tools: X) inline")),
                entry("d", None),
            ]
        );
    }

    #[test]
    fn added_lines_given_as_one_string_still_parse() {
        let record = agent_record(
            r#"["a","b"]"#,
            r#""- a: Does A (Tools: Read)\n- b: Does B""#,
            "[]",
            true,
        );
        assert_eq!(
            listing_from_lines([record.as_str()]).agents,
            vec![entry("a", Some("Does A")), entry("b", Some("Does B"))]
        );
    }

    #[test]
    fn non_array_added_or_removed_types_contribute_nothing() {
        let initial = agent_record(r#"["keep"]"#, r#"["- keep: Kept"]"#, "[]", true);
        let bad_added = agent_record(r#""sneaky""#, r#"["- sneaky: No"]"#, "[]", false);
        let bad_removed = agent_record("[]", "[]", r#""keep""#, false);
        assert_eq!(
            listing_from_lines([initial.as_str(), bad_added.as_str(), bad_removed.as_str()]).agents,
            vec![entry("keep", Some("Kept"))]
        );
    }

    /// `string_items` serves both halves of a delta. The removal sits between two
    /// additions, so a blank name that slipped into the set is added back after it
    /// and still shows in the last prefix. A blank in `removedTypes` cannot be
    /// observed on its own, because no blank can enter the set.
    #[test]
    fn added_and_removed_types_skip_non_strings_and_blanks_and_trim_names() {
        let r1 = agent_record(
            r#"[1,null,"  "," padded ",{"a":1},"keep","gone",true,["nested"]]"#,
            r#"["- padded: Padded one","- keep: Kept","- gone: Gone"]"#,
            "[]",
            true,
        );
        let r2 = agent_record(
            "[]",
            "[]",
            r#"[2,null,"","   ",{"name":"keep"},false,["keep"],"  gone  "]"#,
            false,
        );
        let r3 = agent_record(
            r#"[3,null," ",{"name":"obj"},false," late "]"#,
            r#"["- late: Late one"]"#,
            "[]",
            false,
        );
        assert_eq!(
            listing_from_lines([r1.as_str()]).agents,
            vec![
                entry("gone", Some("Gone")),
                entry("keep", Some("Kept")),
                entry("padded", Some("Padded one")),
            ]
        );
        assert_eq!(
            listing_from_lines([r1.as_str(), r2.as_str()]).agents,
            vec![
                entry("keep", Some("Kept")),
                entry("padded", Some("Padded one")),
            ]
        );
        assert_eq!(
            listing_from_lines([r1.as_str(), r2.as_str(), r3.as_str()]).agents,
            vec![
                entry("keep", Some("Kept")),
                entry("late", Some("Late one")),
                entry("padded", Some("Padded one")),
            ]
        );
    }

    #[test]
    fn a_user_record_mentioning_agent_listing_delta_contributes_nothing() {
        let user = r#"{"type":"user","message":{"content":"what is agent_listing_delta?"},"attachment":{"type":"agent_listing_delta","addedTypes":["sneaky"],"addedLines":[],"removedTypes":[],"isInitial":true}}"#;
        assert_eq!(listing_from_lines([user]), Listing::default());
    }

    #[test]
    fn a_malformed_agent_line_is_skipped() {
        let bad = r#"{"agent_listing_delta": broken"#;
        let good = agent_record(r#"["ok"]"#, r#"["- ok: Fine"]"#, "[]", true);
        assert_eq!(
            listing_from_lines([bad, good.as_str()]).agents,
            vec![entry("ok", Some("Fine"))]
        );
    }

    /// The fixture carries a `skill_listing` record too: it lists no command.
    #[test]
    fn fixture_reads_agents_and_never_commands() {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/skill_listing/listing.jsonl");
        let got = read_listing(&p);
        assert_eq!(got.commands, Vec::new());
        assert_eq!(
            got.agents,
            vec![
                entry(
                    "Explore",
                    Some("Fast read-only search agent for locating code.")
                ),
                entry("sby-agent", Some("Probe agent Y for snapback")),
            ]
        );
    }
}
