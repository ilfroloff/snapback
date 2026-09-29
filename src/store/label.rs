//! Session label derivation, and the store's shared per-record reads.
//!
//! Label preference:
//! 1. latest `type:"summary"` string, else
//! 2. the first prompt the USER wrote ([`FirstPrompt`]): a "real" typed prompt
//!    (skip `isSidechain` turns, [`is_injected`] context and `<...>`-wrapped
//!    command/system prompts; handle both string and typed-block
//!    `message.content`), or a PROMPT COMMAND the user typed, as `/name args`,
//!    else
//! 3. the `session_id`.
//!
//! Result is truncated to a display cap.
//!
//! NOTE: an inline `type:"ai-title"` `aiTitle` tier is deliberately NOT
//! considered — the summary and first real user prompt are the only sources.
//!
//! This module is also the SHARED HOME of the record reads more than one surface
//! must agree on: [`is_sidechain`] (whose turn is it), [`is_meta`] and
//! [`peer_origin`] (who wrote it), and [`is_injected`], the one "Claude Code
//! injected this" check the preview's fold, the content index and the label all
//! call — so no two of them can disagree about which records are instructions
//! nobody typed.

use serde_json::Value;

use super::command;

/// Truncate labels to keep list lines short/atomic.
pub const LABEL_MAX: usize = 180;

/// Extract a `type:"summary"` line's title, if this record is one.
///
/// Empty / whitespace-only summaries are treated as absent so they never win
/// over a real user prompt.
pub fn summary_text(record: &Value) -> Option<String> {
    if record.get("type").and_then(Value::as_str) != Some("summary") {
        return None;
    }
    let s = record.get("summary").and_then(Value::as_str)?;
    if s.trim().is_empty() {
        return None;
    }
    Some(s.to_string())
}

/// Whether this record is a SUB-AGENT's turn (`isSidechain: true`) rather than
/// the session's own.
///
/// The store's one "not from a subagent" rule for a record, shared by the label
/// pick below and by `parse`'s failed-background-task pass, so the two cannot
/// disagree about whose turn a record is. It is the rule's SHARED home, not its
/// ONLY one: `store::preview` still holds an inline copy of the same read for its
/// user turns, and a change here must be mirrored there.
///
/// FAIL-SOFT: an absent, null, or non-bool `isSidechain` reads as `false` — the
/// session's own turn, which is what every depth-2 `user` record observed in a
/// real store carries (subagent turns live in their own files, which discovery
/// never reaches).
pub fn is_sidechain(record: &Value) -> bool {
    record
        .get("isSidechain")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Whether `record` carries `isMeta` — Claude Code's own mark for a record it
/// wrote on the user's behalf (skill and command bodies, caveats, hand-backs),
/// which nobody typed.
///
/// FAIL-SOFT toward "meta": only an ABSENT `isMeta` or a literal `false` reads
/// as an ordinary record, so an unrecognised value can never pass an injection
/// off as the user writing. Shared by `parse`'s failed-task flag (an injection
/// never clears it) and by [`is_injected`].
pub fn is_meta(record: &Value) -> bool {
    !matches!(record.get("isMeta"), None | Some(Value::Bool(false)))
}

/// The `origin.kind` value that marks a message from another session. One of
/// THREE kinds observed in the live store — the other two (`human`, the user's
/// own typed turns, and `task-notification`) are bare `{"kind":…}` objects
/// carrying neither `from` nor `body`. Named because it is an undocumented wire
/// token, exactly like `agents::KIND_*`.
pub const ORIGIN_KIND_PEER: &str = "peer";

/// A qualifying peer message, borrowed out of the record: the two structural
/// facts the preview's peer node renders from.
///
/// Its fields are PRIVATE to this module and it has no other constructor, so
/// outside `store::label` one is produced ONLY by [`peer_origin`] — the gate
/// cannot be bypassed by constructing one elsewhere. Read it through
/// [`from`](Self::from) and [`body`](Self::body).
pub struct PeerOrigin<'a> {
    /// See [`from`](Self::from).
    from: Option<&'a str>,
    /// See [`body`](Self::body).
    body: &'a str,
}

impl<'a> PeerOrigin<'a> {
    /// `origin.from` — the sending agent's stem (`a03505fe4b1c2d3e0`), or some
    /// other sender identity entirely (an agent TYPE name, a unix socket path).
    ///
    /// OPTIONAL by design: the gate does not require it, so a body-bearing peer
    /// record with no `from` still renders — under the generic label rather than
    /// being dropped. See `store::preview`'s `peer_label`.
    pub fn from(&self) -> Option<&'a str> {
        self.from
    }

    /// `origin.body` — the message text, GUARANTEED non-empty by the gate
    /// ([`peer_origin`] is the only way to build one).
    pub fn body(&self) -> &'a str {
        self.body
    }
}

/// The peer gate: is this record a message ANOTHER SESSION sent — a subagent's
/// hand-back, or a forked sibling's note? `Some` only when ALL THREE of these
/// hold, read FAIL-SOFT off `serde_json::Value` throughout (a malformed or absent
/// `origin` is simply not a peer message — never a panic):
///
/// 1. `type == "user"`
/// 2. `origin.kind == "peer"`
/// 3. `origin.body` is a NON-EMPTY string
///
/// Decided STRUCTURALLY, from the record-level `origin` object alone, and NEVER
/// from the `<agent-message …>` text frame the body carries: that frame appears
/// verbatim in quoted prose and tool payloads, while `origin` cannot be written
/// by content.
///
/// Requiring `body` is LOAD-BEARING, not defensive. Measured across the live
/// store: 227 `peer`, 221 `human` and 278 `task-notification` origins exist, and
/// only `peer` ever carries `from`/`body` (136 of them a non-empty one). So a
/// looser gate of "has an `origin`" would collapse ~221 ordinary human turns and
/// ~278 task notifications into peer nodes — hiding the user's OWN prompts. That
/// is the same class of regression the never-parse-the-frame rule exists to
/// prevent, arriving through the structural path instead.
///
/// Lives here rather than in `store::preview` because [`is_injected`] asks it
/// too: every hand-back ALSO carries `isMeta`, and the two must be the same read
/// or a hand-back could fall between them. PURE — see the unit tests.
pub fn peer_origin(record: &Value) -> Option<PeerOrigin<'_>> {
    // The record type is spelled literally here, as in `store::preview`'s
    // `render_record` match.
    if record.get("type").and_then(Value::as_str) != Some("user") {
        return None;
    }
    let origin = record.get("origin")?;
    if origin.get("kind").and_then(Value::as_str) != Some(ORIGIN_KIND_PEER) {
        return None;
    }
    let body = origin
        .get("body")
        .and_then(Value::as_str)
        .filter(|b| !b.is_empty())?;
    Some(PeerOrigin {
        from: origin.get("from").and_then(Value::as_str),
        body,
    })
}

/// Whether Claude Code INJECTED this record — context it added to the session
/// that nobody typed: a skill or command body, a caveat, a "continue from where
/// you left off" notice.
///
/// Exactly a `type:"user"` record that [`is_meta`] and that is NOT a peer message
/// ([`peer_origin`]). The peer exclusion is load-bearing: every subagent
/// hand-back carries `isMeta` too, and a hand-back is an agent's REPORT, not
/// instructions — it stays a peer node in the preview and stays searchable.
///
/// The ONE answer three surfaces read, so they cannot disagree about which
/// records are instructions: `store::preview` folds such a record to a one-line
/// node, `store::parse` leaves it out of the content index, and the label pick
/// never takes it for the user's prompt ([`user_prompt_text`]) — it is instead
/// what CONFIRMS a prompt command ([`FirstPrompt`]). PURE.
pub fn is_injected(record: &Value) -> bool {
    record.get("type").and_then(Value::as_str) == Some("user")
        && is_meta(record)
        && peer_origin(record).is_none()
}

/// Extract the text of a "real" user prompt from this record, or `None`.
///
/// A record qualifies when it is `type:"user"`, is not an `isSidechain` turn, is
/// not [`is_injected`] context, yields non-empty text, and is not a
/// `<...>`-wrapped command/system prompt. Both string and typed-block
/// (`[{type:"text", text:..}]`) `message.content` shapes are handled.
pub fn user_prompt_text(record: &Value) -> Option<String> {
    if record.get("type").and_then(Value::as_str) != Some("user") {
        return None;
    }
    if is_sidechain(record) || is_injected(record) {
        return None;
    }
    let content = record.get("message").and_then(|m| m.get("content"))?;
    let text = user_content_text(content);
    let head = text.trim_start();
    if head.is_empty() || head.starts_with('<') {
        return None;
    }
    Some(text)
}

/// A slash command the user typed, held until the record after it says what it
/// was: the command record's `uuid` and its `/name args`
/// ([`command::sole_command`]).
///
/// `None` unless `record` is the session's own `type:"user"` turn (not
/// [`is_sidechain`], not [`is_injected`]) carrying a `uuid` and a body that is ONE
/// command and nothing else. PURE.
fn command_prompt(record: &Value) -> Option<(String, String)> {
    if record.get("type").and_then(Value::as_str) != Some("user")
        || is_sidechain(record)
        || is_injected(record)
    {
        return None;
    }
    let uuid = record.get("uuid").and_then(Value::as_str)?;
    let content = record.get("message").and_then(|m| m.get("content"))?;
    let text = command::sole_command(&user_content_text(content))?;
    Some((uuid.to_string(), text))
}

/// The label's first-prompt pick, fed every record of a transcript in FILE ORDER
/// by the one streaming pass that reads it.
///
/// It takes the FIRST of two things the user wrote:
///
/// - a typed prompt ([`user_prompt_text`]), or
/// - a PROMPT COMMAND — a slash command whose body Claude Code expanded into the
///   session (`/cr-review github PR #157 …`), labelled as the `/name args` the
///   user typed.
///
/// Both kinds of slash command are a `<command-name>` wrapper record; what tells
/// them apart is the record AFTER it. A prompt command is followed by its
/// expanded body — an [`is_injected`] record whose `parentUuid` is the wrapper's
/// `uuid`. A LOCAL command (`/model`, `/clear`) is followed by its
/// `<local-command-stdout>` instead, and stays skipped, as it always was. The
/// wrapper is therefore held for exactly ONE record — in a real store every body
/// is the very next record — and confirmed or dropped there, which keeps this a
/// single pass with no look-back.
///
/// Without this, a command-started session was labelled with the FIRST LINE OF
/// THE SKILL BODY (every `/cr-review` read "Your task is to run an independent,
/// unbiased review…"), because that body was the first unwrapped user text.
///
/// FAIL-SOFT: a wrapper with no `uuid`, or a next record that is not an injected
/// body pointing back at it, confirms nothing, and the pick moves on.
#[derive(Default)]
pub struct FirstPrompt {
    /// The pick, once made. Never replaced after that.
    first: Option<String>,
    /// The command record seen LAST, awaiting the record after it:
    /// `(uuid, "/name args")`.
    pending: Option<(String, String)>,
}

impl FirstPrompt {
    /// Feed the next record in file order.
    pub fn observe(&mut self, record: &Value) {
        // Taken unconditionally: a held command lives for exactly one record.
        let pending = self.pending.take();
        if self.first.is_some() {
            return;
        }
        if let Some((uuid, text)) = pending {
            let confirms = is_injected(record)
                && record.get("parentUuid").and_then(Value::as_str) == Some(uuid.as_str());
            if confirms {
                self.first = Some(text);
                return;
            }
        }
        if let Some(prompt) = user_prompt_text(record) {
            self.first = Some(prompt);
            return;
        }
        self.pending = command_prompt(record);
    }

    /// The first prompt the user wrote, if the transcript held one.
    pub fn into_first(self) -> Option<String> {
        self.first
    }
}

/// Join a user record's `message.content` into a single string (text blocks
/// joined with a space).
fn user_content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// Apply the label preference and sanitize/truncate for display.
pub fn finalize_label(summary: Option<&str>, first_user: Option<&str>, session_id: &str) -> String {
    let raw = summary.or(first_user).unwrap_or(session_id);
    sanitize_and_truncate(raw, LABEL_MAX)
}

/// Replace tab/newline/carriage-return with spaces and truncate to `max`
/// characters (codepoint-indexed).
fn sanitize_and_truncate(s: &str, max: usize) -> String {
    s.chars()
        .map(|c| match c {
            '\t' | '\n' | '\r' => ' ',
            other => other,
        })
        .take(max)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summary_wins_over_user_prompt() {
        let label = finalize_label(Some("A title"), Some("a prompt"), "sid");
        assert_eq!(label, "A title");
    }

    #[test]
    fn falls_back_to_user_then_session_id() {
        assert_eq!(finalize_label(None, Some("a prompt"), "sid"), "a prompt");
        assert_eq!(finalize_label(None, None, "sid"), "sid");
    }

    #[test]
    fn empty_summary_is_ignored() {
        let record = json!({"type": "summary", "summary": "   "});
        assert_eq!(summary_text(&record), None);
    }

    #[test]
    fn user_prompt_skips_sidechain_and_wrapped() {
        let sidechain = json!({
            "type": "user",
            "isSidechain": true,
            "message": {"content": "hidden tool turn"}
        });
        assert_eq!(user_prompt_text(&sidechain), None);

        let wrapped = json!({
            "type": "user",
            "message": {"content": "<command-name>/clear</command-name>"}
        });
        assert_eq!(user_prompt_text(&wrapped), None);
    }

    #[test]
    fn is_sidechain_reads_only_a_true_bool_as_a_subagent_turn() {
        assert!(is_sidechain(&json!({"type": "user", "isSidechain": true})));
        // Everything else is the session's own turn: false, absent, and the
        // fail-soft shapes a drifted schema could carry.
        for own in [
            json!({"type": "user", "isSidechain": false}),
            json!({"type": "user"}),
            json!({"type": "user", "isSidechain": null}),
            json!({"type": "user", "isSidechain": "true"}),
            json!({"type": "user", "isSidechain": 1}),
        ] {
            assert!(!is_sidechain(&own), "not a subagent turn: {own}");
        }
    }

    #[test]
    fn user_prompt_handles_string_and_typed_blocks() {
        let string_turn = json!({
            "type": "user",
            "message": {"content": "plain question"}
        });
        assert_eq!(
            user_prompt_text(&string_turn).as_deref(),
            Some("plain question")
        );

        let typed_turn = json!({
            "type": "user",
            "message": {"content": [
                {"type": "text", "text": "first part"},
                {"type": "tool_result", "content": "ignored"},
                {"type": "text", "text": "second part"}
            ]}
        });
        assert_eq!(
            user_prompt_text(&typed_turn).as_deref(),
            Some("first part second part")
        );
    }

    #[test]
    fn tabs_and_newlines_are_flattened_and_truncated() {
        let raw = format!("line one\nline two\t{}", "x".repeat(300));
        let label = finalize_label(Some(&raw), None, "sid");
        assert_eq!(label.chars().count(), LABEL_MAX);
        assert!(!label.contains('\n'));
        assert!(!label.contains('\t'));
        assert!(label.starts_with("line one line two "));
    }

    // --- the peer gate ------------------------------------------------------

    /// A real agent stem, the shape `origin.from` carries for a hand-back.
    const PEER_STEM: &str = "a03505fe4b1c2d3e0";

    /// A `type:"user"` record carrying `origin` and an `<agent-message …>` text
    /// frame, so the gate can be exercised on each origin shape. `origin` is
    /// spelled out per case rather than templated, because the gate's whole job
    /// is to tell these shapes apart — and every record built here carries the
    /// frame, so a gate that read the text instead of `origin` would admit them
    /// all.
    fn peer_record(origin: Value) -> Value {
        json!({
            "type": "user",
            "timestamp": "2026-08-20T14:15:00.000Z",
            "origin": origin,
            "message": {"content": "Another Claude session sent a message:\n<agent-message from=\"x\">\nbody\n</agent-message>"}
        })
    }

    /// The gate is all THREE conditions — `type:"user"`, `origin.kind:"peer"`,
    /// and a NON-EMPTY string `origin.body` — and every other shape falls
    /// through to the preview's ordinary rendering. The body requirement is the
    /// load-bearing one: the live store's `human` and `task-notification` origins
    /// are bare `{"kind":…}` objects, so a gate of "has an origin" would swallow
    /// the user's own prompts. Malformed origins fail soft to `None`, never a
    /// panic.
    #[test]
    fn peer_origin_gate_requires_type_kind_and_a_non_empty_body() {
        let good = peer_record(json!({
            "kind": "peer", "from": PEER_STEM, "body": "hello"
        }));
        let origin = peer_origin(&good).expect("a peer record with a body qualifies");
        assert_eq!(origin.from(), Some(PEER_STEM));
        assert_eq!(origin.body(), "hello");

        // The two bare kinds that dominate the store: no body, no node.
        for kind in ["human", "task-notification"] {
            let bare = peer_record(json!({ "kind": kind }));
            assert!(
                peer_origin(&bare).is_none(),
                "a bare `{kind}` origin must not collapse"
            );
        }
        // Same kind, but the body is missing / empty / not a string.
        for body in [json!(null), json!(""), json!(42)] {
            let record = peer_record(json!({
                "kind": "peer", "from": PEER_STEM, "body": body
            }));
            assert!(
                peer_origin(&record).is_none(),
                "a peer origin with body {body} must not collapse"
            );
        }
        // Wrong record type, wrong kind, absent / non-object origin: all fail soft.
        let mut assistant = peer_record(json!({
            "kind": "peer", "from": PEER_STEM, "body": "hello"
        }));
        assistant["type"] = json!("assistant");
        assert!(
            peer_origin(&assistant).is_none(),
            "only user records collapse"
        );
        assert!(peer_origin(&peer_record(json!("peer"))).is_none());
        assert!(peer_origin(&json!({"type": "user"})).is_none());
    }

    // --- injected context and the prompt-command label ---------------------

    /// A subagent hand-back as the store holds it: a peer `origin` with a body,
    /// AND `isMeta` — the pairing that makes the peer exclusion load-bearing.
    fn peer_handback() -> Value {
        json!({
            "type": "user", "uuid": "peer-1", "isMeta": true,
            "origin": {"kind": "peer", "from": "a03505fe4b1c2d3e0", "body": "the report"},
            "message": {"content": "Another Claude session sent a message: the report"}
        })
    }

    /// The prompt-command wrapper Claude Code writes when the user types
    /// `/review-branch PR #42` — the exact shape of a real one.
    fn command_wrapper(uuid: &str) -> Value {
        json!({
            "type": "user", "uuid": uuid, "parentUuid": "before", "turnOrigin": "sdk",
            "message": {"role": "user", "content":
                "<command-message>review-branch</command-message>\n\
                 <command-name>/review-branch</command-name>\n\
                 <command-args>PR #42</command-args>"}
        })
    }

    /// The command's expanded body: `isMeta`, typed blocks, and a `parentUuid`
    /// pointing at whatever record it expands.
    fn injected_body(parent: &str) -> Value {
        json!({
            "type": "user", "uuid": "body-1", "parentUuid": parent, "isMeta": true,
            "message": {"role": "user", "content": [
                {"type": "text", "text": "Your task is to review the current branch."}
            ]}
        })
    }

    /// Run the pick over `records` in order, as the streaming pass does.
    fn first_prompt(records: &[Value]) -> Option<String> {
        let mut pick = FirstPrompt::default();
        for record in records {
            pick.observe(record);
        }
        pick.into_first()
    }

    #[test]
    fn is_injected_is_meta_user_context_and_never_a_peer_handback() {
        assert!(is_injected(&injected_body("x")));
        // FAIL-SOFT toward meta: an unrecognised `isMeta` is still an injection.
        for meta in [json!("yes"), json!(null), json!(1)] {
            assert!(
                is_injected(&json!({"type": "user", "isMeta": meta})),
                "isMeta {meta} reads as injected"
            );
        }
        // The load-bearing exclusion: a hand-back carries isMeta and is NOT
        // instructions — it is an agent's report, and stays a peer message.
        let peer = peer_handback();
        assert!(is_meta(&peer), "the premise: a hand-back carries isMeta");
        assert!(peer_origin(&peer).is_some(), "and passes the peer gate");
        assert!(!is_injected(&peer), "so it is never injected context");
        // Not meta, or not a user record, is not injected.
        for not in [
            json!({"type": "user", "message": {"content": "typed"}}),
            json!({"type": "user", "isMeta": false}),
            json!({"type": "assistant", "isMeta": true}),
            json!({"isMeta": true}),
        ] {
            assert!(!is_injected(&not), "{not} is not injected");
        }
    }

    /// The bug this fixes, at the record level: a skill body is `isMeta` and
    /// carries no wrapper tag, so it used to pass for the user's first prompt.
    #[test]
    fn an_injected_body_is_never_the_users_prompt() {
        assert_eq!(user_prompt_text(&injected_body("x")), None);
        assert_eq!(first_prompt(&[injected_body("x")]), None);
    }

    /// A prompt command is confirmed by the body that expands it and labels the
    /// session with what the user typed — not with the body's first line.
    #[test]
    fn a_confirmed_prompt_command_is_the_first_prompt_as_name_and_args() {
        assert_eq!(
            first_prompt(&[command_wrapper("cmd-1"), injected_body("cmd-1")]).as_deref(),
            Some("/review-branch PR #42")
        );
    }

    /// A LOCAL command is followed by its output, not by an injected body, so it
    /// is never confirmed — and the pick carries on to the next real prompt.
    #[test]
    fn a_local_command_is_skipped_and_the_next_prompt_wins() {
        let local = json!({
            "type": "user", "uuid": "cmd-model",
            "message": {"content": "<command-name>/model</command-name>\n\
                <command-message>model</command-message>\n<command-args></command-args>"}
        });
        let stdout = json!({
            "type": "user", "uuid": "out-1", "parentUuid": "cmd-model",
            "message": {"content": "<local-command-stdout>Set model to opus</local-command-stdout>"}
        });
        let typed = json!({"type": "user", "message": {"content": "now fix the flaky test"}});
        assert_eq!(
            first_prompt(&[local, stdout, typed]).as_deref(),
            Some("now fix the flaky test")
        );
    }

    /// Confirmation is exact: the body must be the VERY NEXT record, be injected,
    /// and point back at THIS wrapper. Anything else confirms nothing.
    #[test]
    fn a_command_is_confirmed_only_by_the_next_record_pointing_back_at_it() {
        let unconfirmed = [
            // The body points at some other record.
            vec![command_wrapper("cmd-1"), injected_body("elsewhere")],
            // Something sits between the wrapper and its body.
            vec![
                command_wrapper("cmd-1"),
                json!({"type": "attachment", "uuid": "att", "parentUuid": "cmd-1"}),
                injected_body("cmd-1"),
            ],
            // The record pointing back is not injected.
            vec![
                command_wrapper("cmd-1"),
                json!({"type": "user", "parentUuid": "cmd-1",
                       "message": {"content": "<local-command-stdout>x</local-command-stdout>"}}),
            ],
            // A peer hand-back pointing back is a report, not the command's body.
            vec![command_wrapper("cmd-1"), {
                let mut peer = peer_handback();
                peer["parentUuid"] = json!("cmd-1");
                peer["message"]["content"] = json!("<agent-message>x</agent-message>");
                peer
            }],
        ];
        for records in unconfirmed {
            assert_eq!(
                first_prompt(&records),
                None,
                "must not confirm: {records:?}"
            );
        }

        // FAIL-SOFT: a wrapper with no uuid has nothing to point back at.
        let mut keyless = command_wrapper("cmd-1");
        keyless.as_object_mut().expect("an object").remove("uuid");
        assert_eq!(first_prompt(&[keyless, injected_body("cmd-1")]), None);
    }

    /// The FIRST prompt wins, whichever kind it is: a typed prompt ahead of a
    /// confirmed command keeps the label, and a confirmed command ahead of a
    /// typed prompt keeps it too.
    #[test]
    fn the_first_prompt_of_either_kind_wins() {
        let typed = json!({"type": "user", "message": {"content": "start here"}});
        assert_eq!(
            first_prompt(&[
                typed.clone(),
                command_wrapper("cmd-1"),
                injected_body("cmd-1")
            ])
            .as_deref(),
            Some("start here")
        );
        assert_eq!(
            first_prompt(&[command_wrapper("cmd-1"), injected_body("cmd-1"), typed]).as_deref(),
            Some("/review-branch PR #42")
        );
    }
}
