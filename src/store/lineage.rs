//! Background-fork lineage identity: which rows are copies of one conversation.
//!
//! Claude Code FORKS a transcript when a prompt is handed to a background job.
//! It copies the foreground file's records verbatim — identical record `uuid`s —
//! into a NEW `sessionId` file and appends there, while the foreground file stops
//! growing. Both files therefore share a tree root, a `cwd`, a `gitBranch` and a
//! first prompt, so `label::finalize_label` derives the SAME label for both and
//! the board draws visually identical "double" rows.
//!
//! This module derives the lineage such rows belong to and collapses each one to
//! a single visible head. It is presentation-only: [`fold`] hides indices from a
//! display list, and nothing here can drop a session.
//!
//! A fork snapback makes — from the `Ctrl-F` box or the Attach/Fork/Cancel
//! choice — joins its parent's lineage the same way (it copies the root record
//! too), but it is NEW work hanging off that parent rather than the parent's
//! continuation. snapback names every such fork
//! with [`FORK_NAME_PREFIX`], and [`head_of`] reads that name so the parent keeps
//! heading the forks made from it.
//!
//! Because it is the one place that knows which rows are copies of ONE
//! conversation, it also answers the question that needs a member compared against
//! its own origin: [`lost_agent_bindings`] flags a background fork that lost the
//! agent binding its lineage ROOT still carries (anthropics/claude-code#80811).
//! That is presentation-only too — a badge, nothing more.
//!
//! Pure and framework-free — no I/O, no `ratatui`. [`fold`] is the single entry
//! point the TUI calls for display, [`lost_agent_bindings`] the one it calls per
//! reload for the badge.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use time::OffsetDateTime;

use super::{label, Session};

/// The prefix the `Ctrl-F` fork box puts at the front of every fork's `--name`
/// (`resume::fork_name`), and the ONE marker [`head_of`] reads to tell such a fork
/// from the conversation it was forked from ([`is_fork_name`]).
///
/// The text IS the marker, so the builder that writes it and the rule that reads
/// it share this one constant and cannot spell it differently. Two costs follow
/// from a text marker, both accepted: a session the user names `fork: …` by hand
/// folds as a fork, and a fork renamed without the prefix becomes an ordinary
/// member that can take the head. Claude's own background copies carry other names
/// (`pr #152 isolated review (2)` in a real store), so they never match.
pub const FORK_NAME_PREFIX: &str = "fork: ";

/// Whether `name` marks a fork snapback made: it starts with
/// [`FORK_NAME_PREFIX`] exactly, case and trailing space included. Pure.
#[must_use]
pub fn is_fork_name(name: &str) -> bool {
    name.starts_with(FORK_NAME_PREFIX)
}

/// Whether `session` is a fork snapback named ([`is_fork_name`] on its
/// [`Session::custom_title`]). A session with no name is never one.
fn is_named_fork(session: &Session) -> bool {
    session.custom_title.as_deref().is_some_and(is_fork_name)
}

/// What a named fork's CHILD row draws after its `⑂`, in the slot an unnamed
/// member's id takes: the name without [`FORK_NAME_PREFIX`], trimmed and made
/// row-safe the way a label is (tabs and line breaks become spaces, capped at
/// [`label::LABEL_MAX`]).
///
/// `None` for a session that is not a named fork, and for one whose name is the
/// prefix alone, so such a row keeps the id it always drew.
#[must_use]
pub fn fork_title(session: &Session) -> Option<String> {
    let title = session
        .custom_title
        .as_deref()?
        .strip_prefix(FORK_NAME_PREFIX)?
        .trim();
    (!title.is_empty()).then(|| label::sanitize_and_truncate(title, label::LABEL_MAX))
}

/// Identity of one fork lineage.
///
/// Keyed on `(repo, branch, root)` rather than on the root uuid ALONE: measured
/// over the real store, 8 of 24 lineages span more than one `gitBranch` (zero
/// span more than one `cwd`). Folding on the root alone would gather members
/// across branch group heads, breaking the list's invariant that same-group rows
/// are contiguous with exactly one head per group. Branch-scoping is also the
/// correct semantic — a fork onto another branch is different work, and it keeps
/// its own row under its own branch's head.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LineageKey {
    /// Derived repo grouping label (see [`super::group::repo_of`]).
    pub repo: String,
    /// Branch label as displayed, so `(detached)` groups with itself.
    pub branch: String,
    /// `uuid` of the transcript tree's root record, copied verbatim into every
    /// fork of the conversation.
    pub root: String,
}

/// The outcome of folding a display list: what stays visible, and what each
/// surviving head is standing in for.
#[derive(Debug, Clone, Default)]
pub struct Folded {
    /// The visible session indices, in the incoming display order except that a
    /// lineage is drawn in its newest member's slot, head first, with an expanded
    /// lineage's other members gathered beneath the head (see [`fold`]).
    pub visible: Vec<usize>,
    /// Head index -> how many lineage members that head hides. A head that hides
    /// nothing (a lone session, or an expanded lineage) has NO entry, so a `(+N)`
    /// marker can never claim a fold that did not happen.
    pub hidden: HashMap<usize, usize>,
}

/// The lineage a session belongs to, or `None` when it has none.
///
/// FAIL-SOFT: a file with no derivable root uuid yields `None`, which means "no
/// lineage" — that session is never folded and always keeps its own row. A
/// degraded parse must cost a user a fold, never a session.
pub fn lineage_key(session: &Session) -> Option<LineageKey> {
    session.root_uuid.as_ref().map(|root| LineageKey {
        repo: session.repo.clone(),
        branch: session.branch_display().to_string(),
        root: root.clone(),
    })
}

/// Partition `indices` into the lineages they belong to: one entry per
/// CONVERSATION, holding that conversation's session indices.
///
/// The counting rule the header is built on, and the same grouping
/// `tui::app::child_indices` marks child rows with — one function, so a row that
/// draws as a child can never be counted as a lineage of its own.
///
/// FAIL-SOFT, exactly as [`fold`] is: a session with no derivable
/// [`lineage_key`] joins no group and comes back as a lineage of ONE. That is
/// the same treatment it gets on screen (never folded, always its own row), so a
/// degraded parse costs a fold, never a miscount.
///
/// Groups arrive in FIRST-APPEARANCE order and every group is non-empty, so a
/// caller may assert on the shape rather than only on the count, despite the
/// `HashMap` inside. Order is otherwise irrelevant to the counter.
///
/// TWO groupings in this module deliberately do NOT come through here, because
/// both need the `LineageKey` ITSELF, which this throws away: [`fold`] must
/// consult `expanded` BY KEY, and [`lost_agent_bindings`] must reach each
/// lineage's own root. Each hand-rolls its own keyed map, so this is the grouping
/// rule's SHARED home, not its ONLY one — THREE loops read [`lineage_key`], and a
/// change to the rule must be mirrored across all three. [`lineage_key`] is where
/// the identity itself lives, and that much they do all share.
#[must_use]
pub fn group_members(sessions: &[Session], indices: &[usize]) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut slot_of: HashMap<LineageKey, usize> = HashMap::new();
    for &i in indices {
        match lineage_key(&sessions[i]) {
            Some(key) => match slot_of.get(&key) {
                Some(&slot) => groups[slot].push(i),
                None => {
                    slot_of.insert(key, groups.len());
                    groups.push(vec![i]);
                }
            },
            None => groups.push(vec![i]),
        }
    }
    groups
}

/// D1's rank for one lineage member: newest FIRST, a timestamp-less member last,
/// exact ties broken by `session_id` ascending.
///
/// `Option`'s `Ord` gives `Some > None` and later-time-greater, so `Reverse` puts
/// the newest member first and a timestamp-less one last. This is the exact key
/// `App::order_filtered` sorts on, so a lineage ranked here lands where the
/// display ordering already puts it.
///
/// The ONE place D1's ordering is written down: [`head_of`] takes its minimum over
/// the members that are not named forks, [`fold`] places each lineage at the slot
/// of its overall minimum ([`newest_of`]) and sorts gathered members by it.
fn member_rank(session: &Session) -> (Reverse<Option<OffsetDateTime>>, &str) {
    (Reverse(session.timestamp), session.session_id.as_str())
}

/// The lineage's NEWEST member by [`member_rank`], whose display slot [`fold`]
/// draws the whole lineage in. `None` only for an empty `members`.
fn newest_of(sessions: &[Session], members: &[usize]) -> Option<usize> {
    members
        .iter()
        .copied()
        .min_by_key(|&i| member_rank(&sessions[i]))
}

/// The lineage's head: its NEWEST member that is NOT a named fork
/// ([`is_fork_name`]), tie-broken by `session_id` ascending — [`member_rank`] over
/// those members. When every member is a named fork, the newest member of all.
///
/// Named forks are skipped because a fork made from the `Ctrl-F` box is new work
/// hanging off its parent, and it is newer than the parent from the moment it
/// exists: newest-as-head would hand the parent's row to whichever fork ran last
/// and bury the conversation they all came from. A background copy claude makes
/// on its own carries no such name and keeps newest-as-head — the copy that kept
/// working IS that conversation's latest state.
///
/// Newest among the rest — NOT most-messages — because the board sorts repo ->
/// branch -> timestamp-desc and ranks groups by their MAX timestamp; a head chosen
/// by bulk could carry a timestamp below its own lineage's max. (The two rules
/// disagree in only 1 of 24 measured lineages, so this is nearly free.) A head
/// that skips a newer named fork does carry an older timestamp than that fork;
/// [`fold`] keeps the lineage where the fork sorts anyway, and a collapsed head
/// showing an older time than the rows below it is the accepted cost.
///
/// # Panics
///
/// Panics if `members` is empty. That is an internal precondition, not hostile
/// input: [`fold`] only ever builds member sets by pushing into them.
pub fn head_of(sessions: &[Session], members: &[usize]) -> usize {
    members
        .iter()
        .copied()
        .filter(|&i| !is_named_fork(&sessions[i]))
        .min_by_key(|&i| member_rank(&sessions[i]))
        .or_else(|| newest_of(sessions, members))
        .expect("head_of requires a non-empty lineage")
}

/// The lineage's ROOT: the OLDEST member that HAS a timestamp — the original
/// foreground transcript every later member was forked from.
///
/// The exact opposite end of the SAME total order [`newest_of`] takes the top of,
/// so there is still only ONE ordering in this module ([`member_rank`]) and a root
/// can never be derived by a rule the fold placement disagrees with.
///
/// # Why the dated filter is load-bearing
///
/// A plain `max_by_key(member_rank)` gets this WRONG. `member_rank` leads with
/// `Reverse(Option<OffsetDateTime>)`, and `Reverse(None)` sorts GREATEST — so the
/// maximum of an undated member and a dated one is the UNDATED one, and a
/// timestamp-less row would be crowned "oldest" purely for lacking the field the
/// question is about. Restricting to members that HAVE a timestamp first is what
/// makes the maximum mean "earliest".
///
/// # Fail-soft
///
/// `None` when no member carries a timestamp: there is then no derivable root, and
/// callers must treat that as "cannot tell" rather than guessing. A degraded parse
/// must cost a badge, never a session — the same trade [`fold`] and
/// [`group_members`] already make for a missing `root_uuid`.
#[must_use]
pub fn root_of(sessions: &[Session], members: &[usize]) -> Option<usize> {
    members
        .iter()
        .copied()
        .filter(|&i| sessions[i].timestamp.is_some())
        .max_by_key(|&i| member_rank(&sessions[i]))
}

/// The BARE #80811 signature, read off ONE session in isolation: a background
/// transcript that NAMES an agent job but carries no agent BINDING.
///
/// Deliberately NOT the shipped rule — it OVER-FLAGS, and that is measurable.
/// `agentName` is free-form (a real store holds `"bugsnag nextjs ssr
/// integration"`, `"mrk-2812 worktree fix"`), so "named an agent but has no
/// binding" is also the ordinary shape of a background job that never had a bound
/// agent to lose. Only [`lost_agent_bindings`]' lineage gate turns this into a
/// claim about a LOST binding, by requiring that the lineage root HAD one.
fn bare_downgrade_signature(session: &Session) -> bool {
    session.background && session.has_agent_name && !session.has_agent_setting
}

/// Every session that shows the anthropics/claude-code#80811 downgrade: a
/// BACKGROUND fork that carries an agent NAME but lost the agent BINDING its own
/// lineage ROOT still carries.
///
/// Returns the flagged `session_id`s, derived ONCE per reload. Keyed by id rather
/// than index so the set cannot be silently mis-read if the session vector is ever
/// re-ordered (the STABLE-ID rule the selection already follows), and so a row
/// renderer does a single `contains` with the id already in its hand.
///
/// # The rule
///
/// A session is flagged when it is `sessionKind: bg`, carries `agent-name`, has NO
/// `agent-setting`, and is NOT its own lineage root while that root DOES carry
/// `agent-setting`. Everything else — including every shape the parse could not
/// read — is not flagged.
///
/// # Why the gate, and what it costs
///
/// The bare signature alone ([`bare_downgrade_signature`]) reads one row in
/// isolation and over-flags: `agentName` is free-form, so a background job that
/// never had a binding matches it while having lost nothing. Gating on the ROOT
/// is what makes the badge assert a LOSS: the root carried a binding, this
/// member does not, and the two are the same conversation by construction.
/// Measured over a real store this flags 3 sessions where the bare signature
/// flags 4, and all 3 are genuine — each is a fork whose root bound `lead` /
/// `technical-brainstormer` and whose own name is the root's name plus a fork
/// marker.
///
/// # Cost
///
/// O(n): one grouping pass over the store, then one root per lineage. It is
/// deliberately NOT a per-row question — asking "does my lineage's root carry a
/// binding?" while drawing each row would rescan the store per row and make the
/// board O(n²).
///
/// # Fail-soft
///
/// Every way of not knowing yields NO flag: no derivable `lineage_key` (no
/// `root_uuid`), no member with a timestamp (no derivable root), a lineage of ONE
/// (the session IS its own root, so it cannot both carry and lack the binding), and
/// any unreadable `sessionKind` / `agentName` / `agentSetting` shape, which the
/// parser has already turned into `false`.
///
/// NOT a closed set, though: the gate BOUNDS over-flagging, it does not eliminate
/// it. `Session::timestamp` is the LAST non-null timestamp in the file, so
/// [`root_of`] means "earliest LAST activity" — never "created first". A FOREGROUND
/// original that gains its `agent-setting` AFTER a background fork was taken
/// (resumed, then bound) yet whose activity ends BEFORE that fork's is still crowned
/// root WITH a binding, so the fork is flagged although nothing was ever taken from
/// it. What bounds that is the badge being INERT — no key, no gate, nothing else
/// reads it — so the worst case is one cosmetic marker, never a wrong action.
#[must_use]
pub fn lost_agent_bindings(sessions: &[Session]) -> HashSet<String> {
    // Group ONCE — the same `(repo, branch, root)` identity `fold` uses, so a
    // root on another branch is a different lineage and cannot gate a flag here.
    let mut members: HashMap<LineageKey, Vec<usize>> = HashMap::new();
    for (i, session) in sessions.iter().enumerate() {
        if let Some(key) = lineage_key(session) {
            members.entry(key).or_default().push(i);
        }
    }

    let mut flagged = HashSet::new();
    for group in members.values() {
        // No dated member => no derivable root => nothing to compare against.
        let Some(root) = root_of(sessions, group) else {
            continue;
        };
        // The gate: only a root that HELD a binding can have one taken from it.
        if !sessions[root].has_agent_setting {
            continue;
        }
        for &i in group {
            // `i != root` also disposes of a lineage of ONE, whose single member
            // is its own root: it would have to carry and lack `agent-setting`
            // simultaneously, so the guard is structural rather than a special case.
            if i != root && bare_downgrade_signature(&sessions[i]) {
                flagged.insert(sessions[i].session_id.clone());
            }
        }
    }
    flagged
}

/// Fold every collapsed lineage in `filtered` down to its head, and gather every
/// expanded one's members beneath theirs.
///
/// Folding happens at the `filtered` (display list) level, so selection, scroll
/// clamping and the wheel keep operating on visible rows alone and need no
/// knowledge of lineages. A lineage whose key is in `expanded` keeps every
/// member; so does a lineage with only one member, which has nothing to hide.
///
/// # Ordering
///
/// - **Each lineage is drawn in the slot of its NEWEST member** ([`newest_of`]);
///   every other member's own slot is dropped. Rows outside any lineage, and
///   lineages of one, keep their incoming order. The visible set is never
///   re-sorted, so folding and expanding can never re-rank one lineage against
///   another.
/// - **The run drawn in that slot starts with the head** ([`head_of`]); an
///   expanded lineage's other members are GATHERED immediately beneath it, in the
///   lineage's own [`member_rank`] order.
///
/// The newest member's slot, not the head's: when the head is a parent whose
/// named forks are newer ([`head_of`]), the head's own slot would sink a lineage
/// whose forks are being worked on right now to wherever the parent last moved.
/// The cost, accepted: a collapsed head can show an older timestamp than the rows
/// below it. With no named fork the head IS the newest member, and nothing moves.
///
/// Gathering is deliberate, and filtering alone does NOT produce it: time
/// scatters a lineage (measured over the real store, 18 of 27 head->child pairs
/// have unrelated rows between them), so a merely-unhidden child lands at its own
/// timestamp slot, detached from the head that explains it. Since every member
/// shares its head's label, such a row reads as an orphan.
///
/// It is safe by construction: D4 scopes a lineage to one `(repo, branch)`, so a
/// gathered member stays INSIDE its own group and same-group rows stay contiguous
/// with exactly one head — `tui::app::build_rows`' invariant. The incoming order
/// sorts a group newest-first (`App::order_filtered`, the same key as
/// [`member_rank`]), so the newest member is the lineage's FIRST row in it and
/// every other member only ever moves UP into its slot.
pub fn fold(sessions: &[Session], filtered: &[usize], expanded: &HashSet<LineageKey>) -> Folded {
    // Collect each lineage's members. A session with no derivable key has no
    // lineage and joins none, so it can never be folded or moved (FAIL-SOFT).
    let mut members: HashMap<LineageKey, Vec<usize>> = HashMap::new();
    for &i in filtered {
        if let Some(key) = lineage_key(&sessions[i]) {
            members.entry(key).or_default().push(i);
        }
    }

    let mut hidden: HashMap<usize, usize> = HashMap::new();
    // Newest member -> the rows drawn in its slot: the head, then (expanded only)
    // every other member.
    let mut runs: HashMap<usize, Vec<usize>> = HashMap::new();
    // Every member of a multi-member lineage but the newest: dropped where it sits,
    // then either left out (collapsed) or re-emitted in the newest's slot.
    let mut displaced: HashSet<usize> = HashSet::new();
    for (key, group) in &members {
        // A lineage of one has nothing to hide and nothing to gather.
        if group.len() < 2 {
            continue;
        }
        let head = head_of(sessions, group);
        let slot = newest_of(sessions, group).expect("a lineage of two is not empty");
        displaced.extend(group.iter().copied().filter(|&i| i != slot));
        let mut rest: Vec<usize> = group.iter().copied().filter(|&i| i != head).collect();
        let mut run = vec![head];
        if expanded.contains(key) {
            rest.sort_by_key(|&i| member_rank(&sessions[i]));
            run.extend(rest);
        } else {
            hidden.insert(head, rest.len());
        }
        runs.insert(slot, run);
    }

    // One pass over the incoming order: a lineage's run lands in its newest
    // member's slot, everything else stays put. Deterministic despite `HashMap`
    // iteration — `filtered` drives the order and each lineage is independent.
    let mut visible = Vec::with_capacity(filtered.len());
    for &i in filtered {
        if let Some(run) = runs.get(&i) {
            visible.extend(run.iter().copied());
        } else if !displaced.contains(&i) {
            visible.push(i);
        }
    }

    Folded { visible, hidden }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use time::OffsetDateTime;

    /// A synthetic session carrying only what the lineage code reads: repo,
    /// branch, `root_uuid`, timestamp and `session_id`. Every member of a real
    /// lineage shares a label by construction, so the helper gives them one.
    fn session(id: &str, branch: &str, root: Option<&str>, ts: Option<i64>) -> Session {
        Session {
            file: PathBuf::from(format!("/tmp/{id}.jsonl")),
            session_id: id.to_string(),
            cwd: PathBuf::from("/Volumes/Development/ilfroloff/snapback"),
            git_branch: Some(branch.to_string()),
            timestamp: ts.map(|s| OffsetDateTime::from_unix_timestamp(s).unwrap()),
            repo: "snapback".to_string(),
            label: "I see kinda double-sessions in the sessions list".to_string(),
            root_uuid: root.map(str::to_string),
            msg_count: 0,
            content_index: String::new(),
            background: false,
            has_agent_name: false,
            has_agent_setting: false,
            failed_task: None,
            custom_title: None,
        }
    }

    /// [`session`] carrying the name claude recorded for it (`custom-title`).
    fn named(id: &str, name: &str, ts: i64) -> Session {
        Session {
            custom_title: Some(name.to_string()),
            ..session(id, BRANCH, Some(FORK_ROOT), Some(ts))
        }
    }

    /// The branch a fixture lineage's members share — D4 scopes a lineage to one
    /// repo+branch, so a gathering fixture has to hold this constant to be one
    /// lineage at all.
    const BRANCH: &str = "feature/fold-fork-lineages";

    fn expanded(keys: &[LineageKey]) -> HashSet<LineageKey> {
        keys.iter().cloned().collect()
    }

    /// Read a display list back as `session_id`s: an ordering assertion has to
    /// say WHICH rows moved, and bare indices do not.
    fn ids(sessions: &[Session], visible: &[usize]) -> Vec<String> {
        visible
            .iter()
            .map(|&i| sessions[i].session_id.clone())
            .collect()
    }

    /// The grouping the header counts in: members of one lineage collapse to one
    /// entry, and the entries arrive in first-appearance order so the shape is
    /// assertable and not just the count.
    #[test]
    fn group_members_gathers_one_lineage_into_one_entry() {
        let sessions = vec![
            session("bg", BRANCH, Some("fork-root"), Some(300)),
            session("interloper", BRANCH, Some("other-root"), Some(200)),
            session("fg", BRANCH, Some("fork-root"), Some(100)),
        ];

        let groups = group_members(&sessions, &[0, 1, 2]);

        assert_eq!(
            groups,
            vec![vec![0, 2], vec![1]],
            "the fork pair is ONE conversation and the interloper another, in the \
             order they were handed in"
        );
    }

    /// FAIL-SOFT, and it is what keeps the counter honest about a degraded parse:
    /// a session with no derivable root is a lineage of ONE, exactly as it is on
    /// screen. Gathering the rootless ones together would count two unrelated
    /// conversations as one.
    #[test]
    fn group_members_gives_a_rootless_session_its_own_lineage() {
        let sessions = vec![
            session("no-root-a", "main", None, Some(100)),
            session("no-root-b", "main", None, Some(200)),
        ];

        let groups = group_members(&sessions, &[0, 1]);

        assert_eq!(groups, vec![vec![0], vec![1]]);
    }

    /// The lineage identity is `(repo, branch, root)` (D4), and the grouping must
    /// read it whole: a shared root across branches is two lineages, matching the
    /// two rows [`fold`] leaves on the board.
    #[test]
    fn group_members_splits_a_shared_root_across_branches() {
        let sessions = vec![
            session("bde-on-master", "master", Some("bde050d4"), Some(100)),
            session(
                "bde-on-feature",
                "feature/fold-fork-lineages",
                Some("bde050d4"),
                Some(200),
            ),
        ];

        let groups = group_members(&sessions, &[0, 1]);

        assert_eq!(groups.len(), 2, "two branches, two conversations");
    }

    #[test]
    fn a_lone_session_is_never_folded() {
        let sessions = vec![session("solo", "main", Some("root-1"), Some(100))];

        let folded = fold(&sessions, &[0], &HashSet::new());

        assert_eq!(folded.visible, vec![0]);
        assert!(
            folded.hidden.is_empty(),
            "a lineage of one hides nothing, so it must never claim a (+N)"
        );
    }

    #[test]
    fn a_fork_pair_collapses_to_the_newest_head() {
        // The real shape: a background hand-off copies the foreground file's
        // records verbatim, so both files carry one root under one repo+branch.
        // The bg copy kept growing after the fork, so it is the newer member.
        let sessions = vec![
            session(
                "fg",
                "feature/live-status-preview",
                Some("fork-root"),
                Some(100),
            ),
            session(
                "bg",
                "feature/live-status-preview",
                Some("fork-root"),
                Some(200),
            ),
        ];

        let folded = fold(&sessions, &[1, 0], &HashSet::new());

        assert_eq!(
            folded.visible,
            vec![1],
            "only the newest member keeps a row"
        );
        assert_eq!(
            folded.hidden.get(&1).copied(),
            Some(1),
            "the head must report the one ancestor it stands for"
        );
    }

    #[test]
    fn an_expanded_lineage_shows_every_member() {
        let sessions = vec![
            session(
                "fg",
                "feature/live-status-preview",
                Some("fork-root"),
                Some(100),
            ),
            session(
                "bg",
                "feature/live-status-preview",
                Some("fork-root"),
                Some(200),
            ),
        ];
        let open = expanded(&[lineage_key(&sessions[1]).unwrap()]);

        let folded = fold(&sessions, &[1, 0], &open);

        // The ancestor comes back, beneath its head. NOTE: this 2-session fixture
        // cannot tell gathered order from merely-unhidden order — both are the
        // same list here. `an_expanded_lineage_gathers_children_under_their_head`
        // is the one with an interloper, and it is what pins the gathering.
        assert_eq!(folded.visible, vec![1, 0]);
        assert!(
            folded.hidden.is_empty(),
            "an expanded head hides nothing, so it reports nothing hidden"
        );
    }

    #[test]
    fn an_expanded_lineage_gathers_children_under_their_head() {
        // Time SCATTERS a lineage: the bg head keeps working while its stalled
        // ancestor is stranded hours back, with unrelated sessions of the same
        // repo+branch in between (measured: 18 of 27 real pairs look like this).
        // Expanding must GATHER the ancestor under its head, not merely unhide it
        // at its own timestamp slot where nothing explains it.
        // The real (+2) shape: this plan's own conversation forked TWICE
        // (e4a59d02 -> c6ce9d37 -> 2265afd8), so a lineage can hide more than one.
        let sessions = vec![
            session("bg", BRANCH, Some("fork-root"), Some(300)),
            // The interloper: same repo+branch, its OWN root, so it is a lineage
            // of one — it must keep its place among the heads.
            session("interloper", BRANCH, Some("other-root"), Some(200)),
            session("mid", BRANCH, Some("fork-root"), Some(150)),
            session("fg", BRANCH, Some("fork-root"), Some(100)),
        ];
        // Display order is timestamp-desc, so the board hands the interloper over
        // BETWEEN the head and its children. Without that the fixture proves
        // nothing: unhidden order and gathered order would be the same list.
        let incoming = vec![0, 1, 2, 3];
        assert_eq!(
            head_of(&sessions, &[0, 2, 3]),
            0,
            "the fixture's head must be the bg copy"
        );
        let open = expanded(&[lineage_key(&sessions[0]).unwrap()]);

        let folded = fold(&sessions, &incoming, &open);

        assert_eq!(
            ids(&sessions, &folded.visible),
            vec!["bg", "mid", "fg", "interloper"],
            "both children must sit immediately beneath their head; leaving them at \
             their own timestamp slots would read ['bg', 'interloper', 'mid', 'fg']"
        );
        assert!(folded.hidden.is_empty());
    }

    #[test]
    fn gathered_children_are_ordered_by_the_lineage_rule_not_by_arrival() {
        // Task 3.4(b) fixes the gathered order to the lineage's OWN rank, and
        // `head_of` already picks the head that way rather than taking whichever
        // member arrived first — the same call `child_indices` makes, so D1 lives
        // in one place.
        //
        // ONLY a scrambled incoming list can falsify that: `App::order_filtered`
        // sorts every scope by `member_rank` within a group, and D4 confines a
        // lineage to ONE group, so in production the members always arrive already
        // ranked and the sort is a no-op. Measured: with the sort deleted, all 302
        // other tests stay green. Hence this fixture hands the children over
        // BACKWARDS — otherwise the rule is unfalsifiable and the ordering could
        // silently become "whatever the caller did".
        let sessions = vec![
            session("bg", BRANCH, Some("fork-root"), Some(300)),
            session("mid", BRANCH, Some("fork-root"), Some(150)),
            session("fg", BRANCH, Some("fork-root"), Some(100)),
        ];
        let open = expanded(&[lineage_key(&sessions[0]).unwrap()]);

        let folded = fold(&sessions, &[0, 2, 1], &open);

        assert_eq!(
            ids(&sessions, &folded.visible),
            vec!["bg", "mid", "fg"],
            "gathered children are ranked newest-first by the lineage rule, not by \
             the order they were handed in"
        );
    }

    #[test]
    fn folding_and_expanding_never_reranks_heads() {
        // (a) of the ordering rule: gathering moves MEMBERS, never heads. Two fork
        // lineages with an unrelated session between them, all one repo+branch —
        // so lineage B's head sits between lineage A's head and A's child, and a
        // gather that re-sorted the visible set would drag the heads around it.
        let sessions = vec![
            session("a-head", "master", Some("root-a"), Some(300)),
            session("interloper", "master", Some("root-solo"), Some(200)),
            session("b-head", "master", Some("root-b"), Some(150)),
            session("a-child", "master", Some("root-a"), Some(100)),
            session("b-child", "master", Some("root-b"), Some(50)),
        ];
        let incoming = vec![0, 1, 2, 3, 4];
        let open = expanded(&[
            lineage_key(&sessions[0]).unwrap(),
            lineage_key(&sessions[2]).unwrap(),
        ]);
        // The heads, in the order the board ranked them by timestamp.
        let heads = ["a-head", "interloper", "b-head"];
        let head_order = |visible: &[usize]| -> Vec<String> {
            ids(&sessions, visible)
                .into_iter()
                .filter(|id| heads.contains(&id.as_str()))
                .collect()
        };

        let all_folded = fold(&sessions, &incoming, &HashSet::new());
        let all_open = fold(&sessions, &incoming, &open);
        let refolded = fold(&sessions, &incoming, &HashSet::new());

        assert_eq!(head_order(&all_folded.visible), heads);
        assert_eq!(
            head_order(&all_open.visible),
            heads,
            "expanding must never re-rank one head against another"
        );
        assert_eq!(head_order(&refolded.visible), heads);
        assert_eq!(
            ids(&sessions, &all_open.visible),
            vec!["a-head", "a-child", "interloper", "b-head", "b-child"],
            "each child gathers under its OWN head, and the heads stay put around them"
        );
        assert_eq!(
            ids(&sessions, &all_folded.visible),
            ids(&sessions, &refolded.visible),
            "a fold -> expand -> fold cycle must land back on the same board"
        );
    }

    #[test]
    fn a_lineage_spanning_branches_does_not_fold() {
        // The measured `bde050d4` shape: one root uuid whose members sit on
        // DIFFERENT branches (8 of 24 lineages do). D4 scopes the key to
        // repo+branch, so this is two lineages of one — not one lineage of two.
        // Each member keeps its row under its own branch's group head, which is
        // both the correct semantic and what keeps `build_rows` coherent.
        let sessions = vec![
            session("bde-on-master", "master", Some("bde050d4"), Some(100)),
            session(
                "bde-on-feature",
                "feature/fold-fork-lineages",
                Some("bde050d4"),
                Some(200),
            ),
        ];

        let folded = fold(&sessions, &[1, 0], &HashSet::new());

        assert_eq!(
            folded.visible,
            vec![1, 0],
            "a shared root across branches must not fold either row away"
        );
        assert!(folded.hidden.is_empty());
        assert_ne!(
            lineage_key(&sessions[0]),
            lineage_key(&sessions[1]),
            "the branch is part of the lineage identity"
        );
    }

    #[test]
    fn a_session_without_a_root_uuid_is_never_folded() {
        // FAIL-SOFT: no derivable root => no lineage => always its own row, even
        // against a twin matching on every other field.
        let sessions = vec![
            session("no-root-a", "main", None, Some(100)),
            session("no-root-b", "main", None, Some(200)),
        ];

        let folded = fold(&sessions, &[1, 0], &HashSet::new());

        assert_eq!(lineage_key(&sessions[0]), None);
        assert_eq!(
            folded.visible,
            vec![1, 0],
            "a rootless session keeps its row"
        );
        assert!(folded.hidden.is_empty());
    }

    #[test]
    fn head_is_newest_not_largest() {
        // D1's one measured disagreement (1 of 24): the NEWEST member is not the
        // one with the MOST MESSAGES — the stalled ancestor holds the longer
        // conversation while the fork that took over is merely newer. Newest
        // must still win, or a folded row's timestamp could fall below its own
        // lineage's max and the board's timestamp-desc ordering would go
        // incoherent.
        //
        // The rejected rule is stated LITERALLY here (`msg_count`), not by a
        // transcript-bulk proxy as it once had to be: `Session` now carries a
        // real turn count, so the fixture can disagree on the actual quantity
        // D1 rejected rather than on something correlated with it.
        let mut ancestor = session("ancestor", "main", Some("root-1"), Some(100));
        ancestor.msg_count = 171;
        let mut newest = session("newest-fork", "main", Some("root-1"), Some(200));
        newest.msg_count = 6;
        assert!(
            newest.msg_count < ancestor.msg_count,
            "the fixture must make the two rules disagree, or it proves nothing"
        );

        let sessions = vec![ancestor, newest];

        assert_eq!(
            head_of(&sessions, &[0, 1]),
            1,
            "the head is the newest member, not the one with the most messages"
        );
        let folded = fold(&sessions, &[1, 0], &HashSet::new());
        assert_eq!(
            folded.visible,
            vec![1],
            "the fold keeps the newest member, not the one holding the most work"
        );
    }

    #[test]
    fn head_tie_breaks_on_session_id_and_sorts_a_missing_timestamp_last() {
        // The rest of D1: `None` last (never the head while any member has a
        // time), and an exact timestamp tie resolved by `session_id` ascending —
        // the same tie-break `App::order_filtered` uses, so the head lands where
        // the display ordering already puts it.
        let sessions = vec![
            session("zz-tied", "main", Some("root-1"), Some(200)),
            session("aa-tied", "main", Some("root-1"), Some(200)),
            session("timeless", "main", Some("root-1"), None),
        ];

        assert_eq!(
            head_of(&sessions, &[0, 1, 2]),
            1,
            "lowest id wins an exact tie"
        );
        assert_eq!(
            head_of(&sessions, &[2]),
            2,
            "a timestamp-less lone member still heads its own lineage"
        );
    }

    // --- forks named by the `Ctrl-F` box ------------------------------------

    /// The marker is the prefix, exactly. Claude's own background copies are named
    /// otherwise and must never read as a fork.
    #[test]
    fn only_the_exact_prefix_marks_a_fork_name() {
        assert!(is_fork_name(FORK_NAME_PREFIX));
        assert!(is_fork_name("fork: Review PR1"));
        for not_a_fork in [
            "pr #152 isolated review (2)",
            "Fork: Review PR1",
            "fork:Review PR1",
            " fork: Review PR1",
            "my fork: Review PR1",
            "",
        ] {
            assert!(!is_fork_name(not_a_fork), "{not_a_fork:?}");
        }
    }

    /// Decision 2: one parent and three forks made from the box. The parent heads
    /// them although every fork is newer, the three fold beneath it as `(+3)`, and
    /// expanding gathers them under it. The lineage sits where its newest fork
    /// sorts, above an interloper that is newer than the parent.
    #[test]
    fn a_parent_heads_its_three_named_forks_and_they_fold_beneath_it() {
        let sessions = vec![
            named("pr3", "fork: Review PR3", 400),
            named("pr2", "fork: Review PR2", 300),
            session("interloper", BRANCH, Some("other-root"), Some(250)),
            named("pr1", "fork: Review PR1", 200),
            session("parent", BRANCH, Some(FORK_ROOT), Some(100)),
        ];
        let incoming = vec![0, 1, 2, 3, 4];

        assert_eq!(head_of(&sessions, &[0, 1, 3, 4]), 4, "the parent is head");

        let folded = fold(&sessions, &incoming, &HashSet::new());
        assert_eq!(
            ids(&sessions, &folded.visible),
            vec!["parent", "interloper"]
        );
        assert_eq!(folded.hidden.get(&4).copied(), Some(3));

        let open = expanded(&[lineage_key(&sessions[4]).unwrap()]);
        let folded = fold(&sessions, &incoming, &open);
        assert_eq!(
            ids(&sessions, &folded.visible),
            vec!["parent", "pr3", "pr2", "pr1", "interloper"],
            "the three forks are gathered beneath the parent they came from"
        );
        assert!(folded.hidden.is_empty());
    }

    /// Decision 3: the fork is the NEWEST member, and the unnamed original is
    /// still crowned head — newest-as-head would have picked the fork.
    #[test]
    fn the_unnamed_original_heads_even_when_a_fork_is_the_newest() {
        let sessions = vec![
            session("original", BRANCH, Some(FORK_ROOT), Some(100)),
            named("fork", "fork: try it", 200),
        ];

        assert_eq!(head_of(&sessions, &[0, 1]), 0);
        let folded = fold(&sessions, &[1, 0], &HashSet::new());
        assert_eq!(ids(&sessions, &folded.visible), vec!["original"]);
        assert_eq!(folded.hidden.get(&0).copied(), Some(1));
    }

    /// A background copy claude made on its own carries a name WITHOUT the prefix,
    /// so it keeps today's rule: it is the newest non-fork, and it heads.
    #[test]
    fn a_background_copy_named_by_claude_keeps_newest_as_head() {
        let sessions = vec![
            session("original", BRANCH, Some(FORK_ROOT), Some(100)),
            named("snap-fork", "fork: try it", 200),
            named("bg-copy", "pr #152 isolated review (2)", 300),
        ];

        assert_eq!(head_of(&sessions, &[0, 1, 2]), 2);
    }

    /// When every member is a named fork there is no parent to crown, and the
    /// newest member heads, as before.
    #[test]
    fn when_every_member_is_a_named_fork_the_newest_heads() {
        let sessions = vec![
            named("older", "fork: one", 100),
            named("newer", "fork: two", 200),
        ];

        assert_eq!(head_of(&sessions, &[0, 1]), 1);
    }

    /// The lineage keeps its newest member's slot, collapsed or expanded, and a
    /// row outside it keeps its own. Drawn at the parent's slot it would sink
    /// below the interloper.
    #[test]
    fn a_lineage_is_drawn_where_its_newest_member_sorts() {
        let sessions = vec![
            named("fork", "fork: try it", 300),
            session("interloper", BRANCH, Some("other-root"), Some(200)),
            session("parent", BRANCH, Some(FORK_ROOT), Some(100)),
        ];
        let incoming = vec![0, 1, 2];

        let folded = fold(&sessions, &incoming, &HashSet::new());
        assert_eq!(
            ids(&sessions, &folded.visible),
            vec!["parent", "interloper"]
        );

        let open = expanded(&[lineage_key(&sessions[2]).unwrap()]);
        let folded = fold(&sessions, &incoming, &open);
        assert_eq!(
            ids(&sessions, &folded.visible),
            vec!["parent", "fork", "interloper"]
        );
    }

    /// The child-row title: the name without the prefix, trimmed and row-safe;
    /// nothing for a session that is not a named fork or names only the prefix.
    #[test]
    fn fork_title_strips_the_prefix_and_names_only_forks() {
        let title = |name: Option<&str>| {
            fork_title(&Session {
                custom_title: name.map(str::to_string),
                ..session("s", BRANCH, Some(FORK_ROOT), Some(1))
            })
        };
        assert_eq!(
            title(Some("fork: Review PR1")).as_deref(),
            Some("Review PR1")
        );
        assert_eq!(title(Some("fork:  a\tb\nc ")).as_deref(), Some("a b c"));
        assert_eq!(title(Some("fork:   ")), None);
        assert_eq!(title(Some("pr #152 isolated review (2)")), None);
        assert_eq!(title(None), None);
    }

    // --- the #80811 downgrade badge ---------------------------------------

    /// The root uuid a fixture lineage's members share. A lineage is
    /// `(repo, branch, root)`, so members must hold this AND [`BRANCH`] constant
    /// to be one conversation at all.
    const FORK_ROOT: &str = "fork-root";

    /// The lineage ROOT of the #80811 shape: the foreground original, which both
    /// named the job and BOUND an agent to it.
    ///
    /// `background: false` is the REAL shape of that original, not a don't-care: a
    /// foreground session OMITS `sessionKind` entirely (DOMAIN.md, `sessionKind`),
    /// so a root fixture claiming `bg` would model a file the store never holds.
    /// The predicate reads the root's binding and never its kind, so this is
    /// fidelity rather than behaviour — it keeps the positive tests exercising the
    /// shape the badge was measured against.
    fn bound_root(id: &str, ts: Option<i64>) -> Session {
        Session {
            background: false,
            has_agent_name: true,
            has_agent_setting: true,
            ..session(id, BRANCH, Some(FORK_ROOT), ts)
        }
    }

    /// The #80811 member: a background fork that still carries the job NAME but
    /// has lost the `agent-setting` BINDING its root has.
    fn downgraded_fork(id: &str, ts: Option<i64>) -> Session {
        Session {
            background: true,
            has_agent_name: true,
            has_agent_setting: false,
            ..session(id, BRANCH, Some(FORK_ROOT), ts)
        }
    }

    /// Read the flag set back as a sorted id list, so a failure says WHICH rows
    /// were flagged rather than only how many.
    fn flagged(sessions: &[Session]) -> Vec<String> {
        let mut ids: Vec<String> = lost_agent_bindings(sessions).into_iter().collect();
        ids.sort();
        ids
    }

    /// The signature the badge exists for, exactly as it appears on disk: a
    /// `lead`-bound foreground root at 13:39 and its background fork at 12:52 the
    /// next day, carrying the job name with no binding.
    #[test]
    fn flags_a_background_fork_that_lost_its_roots_binding() {
        let sessions = vec![
            downgraded_fork("ca45ce75", Some(300)),
            bound_root("75ae8db4", Some(100)),
        ];

        assert_eq!(
            flagged(&sessions),
            vec!["ca45ce75".to_string()],
            "the fork lost a binding its own root still carries"
        );
    }

    /// The OVER-FLAG decision 4 rejected, and the reason the lineage gate exists.
    /// `agentName` is FREE-FORM — a job title like `"bugsnag nextjs ssr
    /// integration"`, not an agent handle — so a background job whose lineage
    /// never had a binding matches the BARE signature while having lost nothing.
    ///
    /// This is the test that proves the gate does work: it is RED against
    /// [`bare_downgrade_signature`] alone.
    #[test]
    fn does_not_flag_when_the_lineage_root_never_had_a_binding() {
        let sessions = vec![
            downgraded_fork("titled-fork", Some(300)),
            // Same lineage, older, but it never bound an agent either.
            Session {
                has_agent_setting: false,
                ..bound_root("titled-root", Some(100))
            },
        ];

        assert!(
            flagged(&sessions).is_empty(),
            "a free-form job title is not a lost binding: nothing was taken away"
        );
    }

    /// A lineage of ONE is its own root, so it would have to carry and lack
    /// `agent-setting` at the same moment. Structurally impossible — pinned
    /// because it is a load-bearing over-flag guard, and because a naive
    /// implementation that compares a session against "some root" rather than
    /// against a root that is NOT itself flags every lone background job.
    ///
    /// # This test is guarded TWICE, and no SINGLE mutation reddens it
    ///
    /// Stated plainly because the execution checklist asks for every test to have
    /// been OBSERVED failing, and this one cannot be by breaking one thing. Two
    /// independent clauses in [`lost_agent_bindings`] each suffice on their own:
    /// the root gate (`!sessions[root].has_agent_setting` -> `continue`) and
    /// `i != root`. Removing the gate alone leaves this green (it reddens
    /// `does_not_flag_when_the_lineage_root_never_had_a_binding` instead, which is
    /// the test that isolates the gate); removing `i != root` alone leaves the
    /// WHOLE suite green. Only removing BOTH turns this red.
    ///
    /// `i != root` cannot be isolated by any fixture, either — given the gate it is
    /// never a discriminator. Reaching the comparison at all requires
    /// `sessions[root].has_agent_setting`, while flagging the root would require
    /// [`bare_downgrade_signature`], hence `!has_agent_setting`, on that same
    /// session. So it is redundant BY CONSTRUCTION (as the guard's own comment
    /// says), kept as structural defence against a future refactor that drops or
    /// loosens the gate.
    ///
    /// This test therefore pins the over-flag guard AS A WHOLE. It does not
    /// isolate either clause, and it should not be read as evidence that both are
    /// independently load-bearing.
    #[test]
    fn does_not_flag_a_lineage_of_one() {
        let sessions = vec![downgraded_fork("lonely", Some(100))];

        assert!(
            flagged(&sessions).is_empty(),
            "a session cannot have lost a binding to itself"
        );
    }

    /// FAIL-SOFT, the same treatment `fold` and `group_members` give it: no
    /// derivable `root_uuid` means no lineage, so there is no root to compare
    /// against and therefore nothing to assert.
    #[test]
    fn does_not_flag_a_session_with_no_root_uuid() {
        let sessions = vec![
            Session {
                root_uuid: None,
                ..downgraded_fork("rootless", Some(300))
            },
            bound_root("has-a-root", Some(100)),
        ];

        assert!(
            flagged(&sessions).is_empty(),
            "no lineage key, no lineage, no claim"
        );
    }

    /// A lineage is `(repo, branch, root)` — a shared root across branches is TWO
    /// lineages (D4). A bound root on ANOTHER branch must not gate a flag on this
    /// one: that is different work, not this conversation's origin.
    #[test]
    fn does_not_flag_across_a_branch_boundary() {
        let sessions = vec![
            downgraded_fork("fork-on-feature", Some(300)),
            Session {
                git_branch: Some("master".to_string()),
                ..bound_root("root-on-master", Some(100))
            },
        ];

        assert!(
            flagged(&sessions).is_empty(),
            "a root on another branch heads a different lineage"
        );
    }

    /// Nothing was lost if the binding is still there. The most direct
    /// false-positive guard there is.
    #[test]
    fn does_not_flag_a_member_that_still_carries_its_binding() {
        let sessions = vec![
            Session {
                has_agent_setting: true,
                ..downgraded_fork("still-bound", Some(300))
            },
            bound_root("root", Some(100)),
        ];

        assert!(
            flagged(&sessions).is_empty(),
            "a member holding agent-setting has lost nothing"
        );
    }

    /// The badge asserts something about a BACKGROUND job specifically. An
    /// interactive session matching every other clause is not the #80811 shape —
    /// it is a foreground transcript, and `sessionKind` is what says so.
    #[test]
    fn does_not_flag_a_foreground_session() {
        let sessions = vec![
            Session {
                background: false,
                ..downgraded_fork("interactive", Some(300))
            },
            bound_root("root", Some(100)),
        ];

        assert!(
            flagged(&sessions).is_empty(),
            "no sessionKind:bg, no background downgrade to report"
        );
    }

    /// A member naming no agent at all has no binding to have lost — it is an
    /// ordinary background session, and the overwhelming majority of the store.
    #[test]
    fn does_not_flag_a_member_that_never_named_an_agent() {
        let sessions = vec![
            Session {
                has_agent_name: false,
                ..downgraded_fork("anonymous", Some(300))
            },
            bound_root("root", Some(100)),
        ];

        assert!(flagged(&sessions).is_empty());
    }

    // --- root_of ----------------------------------------------------------

    /// The edge a naive `max_by_key(member_rank)` gets WRONG: `member_rank` leads
    /// with `Reverse(Option<_>)` and `Reverse(None)` sorts GREATEST, so a plain
    /// maximum crowns the TIMESTAMP-LESS member "oldest" — and the badge would
    /// then compare every member against a row that has no position in time.
    #[test]
    fn a_timestamp_less_member_never_becomes_the_root() {
        let sessions = vec![
            session("undated", BRANCH, Some(FORK_ROOT), None),
            session("oldest", BRANCH, Some(FORK_ROOT), Some(100)),
            session("newest", BRANCH, Some(FORK_ROOT), Some(300)),
        ];

        assert_eq!(
            root_of(&sessions, &[0, 1, 2]),
            Some(1),
            "the root is the oldest DATED member, never the undated one"
        );
        assert_eq!(
            head_of(&sessions, &[0, 1, 2]),
            2,
            "and the head is still the newest, from the same ordering"
        );
    }

    /// FAIL-SOFT: with no dated member there is no derivable root, so the answer
    /// is "cannot tell" rather than an arbitrary pick — and the badge stays off.
    #[test]
    fn root_of_is_none_when_no_member_is_dated() {
        let sessions = vec![
            session("a", BRANCH, Some(FORK_ROOT), None),
            session("b", BRANCH, Some(FORK_ROOT), None),
        ];

        assert_eq!(root_of(&sessions, &[0, 1]), None);
    }

    /// The whole reason an undated member must not be crowned root, stated as the
    /// badge behaviour rather than as an ordering detail: the undated row would
    /// be a rootless "root" carrying no binding, and the gate would go silent.
    #[test]
    fn an_undated_member_does_not_suppress_a_real_flag() {
        let sessions = vec![
            downgraded_fork("ca45ce75", Some(300)),
            bound_root("75ae8db4", Some(100)),
            // A stalled stub with no parseable timestamp, in the same lineage.
            session("undated-stub", BRANCH, Some(FORK_ROOT), None),
        ];

        assert_eq!(
            flagged(&sessions),
            vec!["ca45ce75".to_string()],
            "an undated sibling must not become the root and hide the downgrade"
        );
    }
}
