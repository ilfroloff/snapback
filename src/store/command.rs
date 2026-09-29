//! Claude Code's control-wrapper tags, parsed — the ONE reading of them that the
//! preview, the content index and the row label share.
//!
//! Claude Code wraps control content (slash-command turns, injected reminders,
//! local command output, task notifications, persisted output) in a fixed set of
//! PAIRED pseudo-tags. This module splits a message body into literal prose and
//! those wrappers ([`collapse_control_wrappers`]) and says what a slash command
//! the user TYPED reads as (`/name args`, [`command_text`]). Three consumers read
//! that one answer, so they cannot drift apart about what a command says:
//!
//! - `store::preview` draws each segment — a command as `▷ /name args`, every
//!   other wrapper as a one-line dim marker;
//! - `store::parse`'s content index keeps a command as `/name args` and every
//!   other byte as written ([`typed_text`]), so a search reaches the command's
//!   name and arguments and never the tag names around them;
//! - `store::label` labels a session its user started with a prompt command
//!   `/name args` ([`sole_command`]).
//!
//! Framework-free on purpose: nothing here knows about ratatui. The marker
//! LABELS below are plain words the preview draws, carried on the segment so the
//! allowlist and what each wrapper collapses to stay one table.
//!
//! SAFETY — allowlist only. Two classes of angle-bracket tokens live in real data
//! and only PAIRED control wrappers may be touched. Open-only template
//! placeholders (`<session-id>`, `<skill-dir>`), generics/JSX (`<String>`,
//! `<br>`, `<T>`), and comparisons (`x < y > z`) are legitimate content —
//! collapsing them would be a data-loss bug. So a token is acted on ONLY when its
//! name is in [`CONTROL_WRAPPERS`] AND it has a matching close tag; a known
//! opener with no close FAILS SOFT to literal (never eats trailing content, never
//! panics). Wrappers can span lines and nest different-named tags as payload
//! (e.g. `<task-notification>` holds `<task-id>`/`<output-file>`), so the pass
//! walks the WHOLE body string.

/// The ONLY paired pseudo-tag names the collapse acts on. One exact allowlist so
/// the pass can never touch a legitimate angle-bracket token (an open-only
/// placeholder, a generic, or a `<`/`>` comparison in prose).
pub const CONTROL_WRAPPERS: &[&str] = &[
    "command-name",
    "command-message",
    "command-args",
    "local-command-stdout",
    "local-command-stderr",
    "local-command-caveat",
    "system-reminder",
    "task-notification",
    "persisted-output",
];

/// The opener [`typed_text`] must find before it walks a body at all: with no
/// `<command-name>` there is no NAMED command group to rewrite, and a group with
/// no name prints nothing ([`command_text`]).
///
/// The quick path this buys is NOT the walk's answer for every body. A body with
/// a `command-args` (or `command-message`) tag group but no `command-name` takes
/// it, and so keeps that group — its tags included — in the content index as
/// written, where the walk would have dropped the nameless group. Claude Code
/// writes the tags as one group, and no user record in the live store carries
/// either without its name today, so the two answers agree on real data.
const COMMAND_NAME_OPENER: &str = "<command-name>";

/// Marker for a collapsed `local-command-stdout` / `local-command-stderr` wrapper.
/// The payload can be huge, so only its presence is surfaced (never inlined).
pub const MARKER_COMMAND_OUTPUT: &str = "[command output]";
/// Marker for a collapsed `local-command-caveat` wrapper. The caveat Claude Code
/// injects alongside `local-command-stdout` is semantically DISTINCT from the
/// command's output, so it gets its own label rather than folding into it.
pub const MARKER_COMMAND_CAVEAT: &str = "[command caveat]";
/// Marker for a collapsed `system-reminder` wrapper — stubbed, so an injected
/// reminder stays discoverable rather than hidden or dumped raw.
pub const MARKER_SYSTEM_REMINDER: &str = "[system-reminder]";
/// Marker for a collapsed `task-notification` wrapper (nested `task-id` /
/// `output-file` are consumed as payload, never shown).
pub const MARKER_TASK_NOTIFICATION: &str = "[task-notification]";
/// Marker for a collapsed `persisted-output` wrapper.
pub const MARKER_PERSISTED_OUTPUT: &str = "[persisted-output]";

/// A message body as an ordered sequence of literal prose and collapsed control
/// wrappers.
#[derive(Debug, PartialEq)]
pub enum Segment {
    /// Prose, byte for byte.
    Literal(String),
    /// A slash-command group (`command-name` + optional `command-args`; the
    /// `command-message` echo is dropped). Read it through [`command_text`].
    Command { name: Option<String>, args: String },
    /// Any other allowlisted wrapper: the marker `label` the preview draws for
    /// it, and the wrapper's `raw` text — tags included, exactly as written — for
    /// the consumers that keep it ([`typed_text`]).
    Marker { label: &'static str, raw: String },
}

/// How an allowlisted wrapper collapses. Command-turn tags carry their payload so
/// the trio (`command-name` + optional `command-args`; `command-message` is a mere
/// echo) can merge into one command group; every other wrapper maps to a fixed
/// marker label.
enum WrapperKind {
    CommandName,
    CommandArgs,
    CommandMessage,
    Marker(&'static str),
}

/// Map an allowlisted wrapper name to its kind — the single source that ties
/// [`CONTROL_WRAPPERS`] to behavior. `None` means "not a control wrapper".
fn wrapper_kind(name: &str) -> Option<WrapperKind> {
    Some(match name {
        "command-name" => WrapperKind::CommandName,
        "command-args" => WrapperKind::CommandArgs,
        "command-message" => WrapperKind::CommandMessage,
        "local-command-stdout" | "local-command-stderr" => {
            WrapperKind::Marker(MARKER_COMMAND_OUTPUT)
        }
        "local-command-caveat" => WrapperKind::Marker(MARKER_COMMAND_CAVEAT),
        "system-reminder" => WrapperKind::Marker(MARKER_SYSTEM_REMINDER),
        "task-notification" => WrapperKind::Marker(MARKER_TASK_NOTIFICATION),
        "persisted-output" => WrapperKind::Marker(MARKER_PERSISTED_OUTPUT),
        _ => return None,
    })
}

/// If `rest` opens with a known control-wrapper tag `<name>` (name in
/// [`CONTROL_WRAPPERS`], immediately closed by `>` with no attributes), return the
/// allowlist `name` and the opener's byte length. A closing tag, an unlisted name,
/// or an attribute-bearing tag is not a control opener. Tag names are ASCII, so
/// the returned length always lands on a UTF-8 char boundary.
fn match_control_opener(rest: &str) -> Option<(&'static str, usize)> {
    let after_lt = rest.strip_prefix('<')?;
    CONTROL_WRAPPERS.iter().find_map(|&name| {
        after_lt
            .strip_prefix(name)
            .and_then(|tail| tail.strip_prefix('>'))
            .map(|_| (name, '<'.len_utf8() + name.len() + '>'.len_utf8()))
    })
}

/// Pure pre-pass: split `body` into literal and collapsed [`Segment`]s over the
/// [`CONTROL_WRAPPERS`] allowlist. Operates on the WHOLE body (wrappers span lines
/// and nest different-named tags as payload). FAIL-SOFT: a known opener with no
/// matching close is left literal (trailing content preserved, never panics); an
/// unlisted `<…>` token is left literal byte-for-byte. A body with no wrapper
/// yields exactly `[Literal(body)]`, so ordinary prose is untouched.
pub fn collapse_control_wrappers(body: &str) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    let mut literal = String::new();
    let mut i = 0;
    while i < body.len() {
        let rest = &body[i..];
        if let Some(consumed) = try_collapse_at(rest, &mut segments, &mut literal) {
            i += consumed;
            continue;
        }
        // Ordinary character (includes an unmatched `<` or an unlisted tag's `<`).
        let ch = rest.chars().next().expect("non-empty remainder has a char");
        literal.push(ch);
        i += ch.len_utf8();
    }
    flush_literal_segment(&mut literal, &mut segments);
    segments
}

/// Try to collapse a control wrapper at the START of `rest`. On a full match
/// (known opener + matching close) mutate `segments`/`literal` and return the byte
/// length consumed; return `None` otherwise (the caller then takes one literal
/// char), which also covers the FAIL-SOFT unclosed-opener case.
fn try_collapse_at(rest: &str, segments: &mut Vec<Segment>, literal: &mut String) -> Option<usize> {
    let (name, open_len) = match_control_opener(rest)?;
    let kind = wrapper_kind(name)?;
    let close = format!("</{name}>");
    let rel = rest[open_len..].find(&close)?;
    let payload = &rest[open_len..open_len + rel];
    let consumed = open_len + rel + close.len();
    match kind {
        WrapperKind::Marker(label) => {
            flush_literal_segment(literal, segments);
            segments.push(Segment::Marker {
                label,
                raw: rest[..consumed].to_string(),
            });
        }
        WrapperKind::CommandName => add_command_field(segments, literal, Some(payload), None),
        WrapperKind::CommandArgs => add_command_field(segments, literal, None, Some(payload)),
        // A `command-message` is a mere echo: drop its payload but keep the command
        // group open so an adjacent name/args tag still merges into one group.
        WrapperKind::CommandMessage => add_command_field(segments, literal, None, None),
    }
    Some(consumed)
}

/// Flush accumulated literal text as a `Segment::Literal` (an empty run is dropped).
fn flush_literal_segment(literal: &mut String, segments: &mut Vec<Segment>) {
    if !literal.is_empty() {
        segments.push(Segment::Literal(std::mem::take(literal)));
    }
}

/// Merge one slash-command tag into the current command group. The trio is
/// contiguous in real data but separated only by whitespace, so a blank pending
/// literal is absorbed and the field extends the trailing `Segment::Command`; any
/// non-blank literal (real prose) finalizes the run and starts a fresh group.
fn add_command_field(
    segments: &mut Vec<Segment>,
    literal: &mut String,
    name: Option<&str>,
    args: Option<&str>,
) {
    if literal.trim().is_empty() {
        literal.clear(); // absorb inter-tag / leading whitespace
    } else {
        flush_literal_segment(literal, segments);
    }
    if !matches!(segments.last(), Some(Segment::Command { .. })) {
        segments.push(Segment::Command {
            name: None,
            args: String::new(),
        });
    }
    if let Some(Segment::Command {
        name: cur_name,
        args: cur_args,
    }) = segments.last_mut()
    {
        if let Some(n) = name {
            *cur_name = Some(n.to_string());
        }
        if let Some(a) = args {
            *cur_args = a.to_string();
        }
    }
}

/// What a slash-command group says the user typed: `/name args`, or `/name` when
/// the args are empty. The ONE normalisation of a command, which the preview's
/// `▷` line, the content index and the row label all print.
///
/// The name is trimmed and any leading slashes are stripped before exactly one is
/// written back (`//init` and `init` both read `/init`); the args are trimmed. A
/// group with no usable name — a bare `command-message` echo, or `command-args`
/// with no `command-name` — says nothing and yields `None`.
pub fn command_text(name: Option<&str>, args: &str) -> Option<String> {
    let name = name?.trim().trim_start_matches('/');
    if name.is_empty() {
        return None;
    }
    let args = args.trim();
    Some(if args.is_empty() {
        format!("/{name}")
    } else {
        format!("/{name} {args}")
    })
}

/// `body` as the content index keeps it: every slash-command group rewritten to
/// its [`command_text`], and every other byte — prose, and every other wrapper
/// with its tags — exactly as written.
///
/// That split is the whole rule. A command's TAG names (`command-args`,
/// `command-name`) are harness syntax the user never typed, and indexing them
/// made a search for an ordinary word like `args` match most sessions that ever
/// ran a command; the command's name and arguments ARE what the user typed. The
/// other wrappers' contents stay searchable as they always were (`README.md`
/// promises a reminder or a command's output is found).
///
/// A command is kept on a line of its own: where it would otherwise abut the text
/// beside it (`…</local-command-stdout><command-name>…`), a newline separates the
/// two so neither word runs into the other.
///
/// Hands `body` straight back when it holds no `<command-name>` opener. The index
/// is built over every transcript at load, so the common case costs one
/// substring scan and no copy. For such a body the walk would rebuild the same
/// bytes, with ONE exception: a nameless `command-args` / `command-message`
/// group, which the quick path keeps as written and the walk would drop (see
/// [`COMMAND_NAME_OPENER`]). Pure.
pub fn typed_text(body: String) -> String {
    if !body.contains(COMMAND_NAME_OPENER) {
        return body;
    }
    let mut out = String::with_capacity(body.len());
    // Set right after a command is written, so the NEXT piece starts on a line
    // of its own when it does not already begin with whitespace.
    let mut after_command = false;
    for segment in collapse_control_wrappers(&body) {
        let piece = match segment {
            Segment::Literal(text) => text,
            Segment::Marker { raw, .. } => raw,
            Segment::Command { name, args } => {
                let Some(text) = command_text(name.as_deref(), &args) else {
                    continue;
                };
                if out.chars().last().is_some_and(|c| !c.is_whitespace()) {
                    out.push('\n');
                }
                out.push_str(&text);
                after_command = true;
                continue;
            }
        };
        if after_command && piece.chars().next().is_some_and(|c| !c.is_whitespace()) {
            out.push('\n');
        }
        after_command = false;
        out.push_str(&piece);
    }
    out
}

/// The [`command_text`] of a body that is ONE slash command and nothing else, or
/// `None`.
///
/// "Nothing else" is exact: one command group with a usable name, and at most
/// whitespace around it. A body that mixes a command with prose, a marker, or a
/// second command is not a command line — it is not what Claude Code writes for
/// a command the user typed, so it is not read as one. Pure.
pub fn sole_command(body: &str) -> Option<String> {
    let mut found: Option<String> = None;
    for segment in collapse_control_wrappers(body) {
        match segment {
            Segment::Literal(text) if text.trim().is_empty() => {}
            Segment::Command { name, args } if found.is_none() => {
                found = Some(command_text(name.as_deref(), &args)?);
            }
            _ => return None,
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape Claude Code writes for a prompt command the user typed, as it
    /// sits in a real transcript: the `command-message` echo first, then the
    /// name, then the args, one per line.
    const PROMPT_COMMAND: &str = "<command-message>review-branch</command-message>\n\
         <command-name>/review-branch</command-name>\n\
         <command-args>PR #42 in a separate worktree</command-args>";

    #[test]
    fn every_allowlisted_wrapper_has_a_kind() {
        // Drift guard: the allowlist and the kind mapping stay in lockstep, so
        // matching a listed opener can never fall through to an unhandled kind.
        for &name in CONTROL_WRAPPERS {
            assert!(
                wrapper_kind(name).is_some(),
                "allowlist name {name} has no kind"
            );
        }
    }

    #[test]
    fn command_text_normalizes_the_name_and_omits_empty_args() {
        assert_eq!(
            command_text(Some("/foo"), " bar baz ").as_deref(),
            Some("/foo bar baz")
        );
        // Any leading slashes are stripped then exactly one written.
        assert_eq!(command_text(Some("//init"), "  ").as_deref(), Some("/init"));
        assert_eq!(command_text(Some("init"), "").as_deref(), Some("/init"));
        // A group with no usable name says nothing.
        assert_eq!(command_text(None, "anything"), None);
        assert_eq!(command_text(Some("   "), "x"), None);
        assert_eq!(command_text(Some("/"), "x"), None);
    }

    #[test]
    fn a_marker_keeps_its_raw_wrapper_text() {
        let body = "before <local-command-stdout>Set model\nto opus</local-command-stdout> after";
        assert_eq!(
            collapse_control_wrappers(body),
            vec![
                Segment::Literal("before ".to_string()),
                Segment::Marker {
                    label: MARKER_COMMAND_OUTPUT,
                    raw: "<local-command-stdout>Set model\nto opus</local-command-stdout>"
                        .to_string(),
                },
                Segment::Literal(" after".to_string()),
            ]
        );
    }

    /// The index's rule, on the real command shape: the command becomes the
    /// words the user typed, and NO tag name survives to be searched.
    #[test]
    fn typed_text_keeps_a_command_as_what_the_user_typed() {
        let text = typed_text(PROMPT_COMMAND.to_string());
        assert_eq!(text, "/review-branch PR #42 in a separate worktree");
        for tag in ["command-name", "command-args", "command-message", "<", ">"] {
            assert!(!text.contains(tag), "{tag:?} must not be indexed: {text:?}");
        }
    }

    /// Every OTHER wrapper stays exactly as written — the README promises a
    /// reminder or a command's output is searchable — and so does all prose,
    /// whether the fast path or the walk answers it. The last two bodies carry a
    /// `<command-name>` opener, so they go through the walk.
    #[test]
    fn typed_text_leaves_every_other_wrapper_and_all_prose_verbatim() {
        for body in [
            "plain prose with Vec<String> and x < y > z",
            "<local-command-stdout>Set model to opus</local-command-stdout>",
            "prose\n<system-reminder>be brief</system-reminder>\nmore prose",
            "<task-notification><status>failed</status></task-notification>",
            "an unclosed <command-name>/x opener stays literal",
            "<system-reminder>quoted <command-name>/x</command-name></system-reminder>",
        ] {
            assert_eq!(typed_text(body.to_string()), body, "must be kept verbatim");
        }
    }

    /// A command next to other text lands on a line of its own rather than
    /// running into its neighbour, and the neighbour's own bytes survive.
    #[test]
    fn typed_text_puts_a_command_on_its_own_line() {
        let body = "<local-command-stdout>out</local-command-stdout>\
                    <command-name>/model</command-name><command-args>opus</command-args>trailing";
        assert_eq!(
            typed_text(body.to_string()),
            "<local-command-stdout>out</local-command-stdout>\n/model opus\ntrailing"
        );
        // Existing whitespace is not doubled.
        assert_eq!(
            typed_text("see:\n<command-name>/init</command-name>\n".to_string()),
            "see:\n/init\n"
        );
        // A nameless group contributes nothing, and the rest is kept.
        assert_eq!(
            typed_text("<command-name>   </command-name>kept".to_string()),
            "kept"
        );
    }

    #[test]
    fn sole_command_reads_only_a_body_that_is_one_command() {
        assert_eq!(
            sole_command(PROMPT_COMMAND).as_deref(),
            Some("/review-branch PR #42 in a separate worktree")
        );
        // A local command reads the same way; telling the two apart is the
        // LABEL's job (it needs the record that follows), not the parser's.
        assert_eq!(
            sole_command(
                "<command-name>/model</command-name>\n\
                 <command-message>model</command-message>\n<command-args></command-args>\n"
            )
            .as_deref(),
            Some("/model")
        );
        for not_one in [
            "",
            "plain prose",
            "<command-message>echo only</command-message>",
            "<command-name>/a</command-name> and some prose",
            "<command-name>/a</command-name>\n<local-command-stdout>x</local-command-stdout>",
            "<command-name>/a</command-name>\nprose\n<command-name>/b</command-name>",
        ] {
            assert_eq!(
                sole_command(not_one),
                None,
                "{not_one:?} is not one command"
            );
        }
    }

    #[test]
    fn legitimate_angle_bracket_tokens_are_left_literal() {
        // Regression / data-loss guard: open-only placeholders, generics, and
        // comparisons are real content — the pre-pass returns them byte-for-byte
        // as one literal segment (nothing stripped or restyled).
        let body = "Use <session-id> and Vec<String>; also x < y > z here.";
        assert_eq!(
            collapse_control_wrappers(body),
            vec![Segment::Literal(body.to_string())],
            "no legitimate angle-bracket token may be collapsed"
        );
    }

    #[test]
    fn unclosed_known_opener_is_left_literal_and_keeps_trailing_content() {
        // A known opener with no closing tag must FAIL SOFT: treated as literal,
        // with everything after it preserved (never eaten, never a panic).
        let body = "before <system-reminder> tail content after";
        assert_eq!(
            collapse_control_wrappers(body),
            vec![Segment::Literal(body.to_string())],
            "an unclosed opener stays literal"
        );
    }
}
