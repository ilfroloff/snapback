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
//! Pure and framework-free — no I/O, no `ratatui`. [`fold`] is the single entry
//! point the TUI calls.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use time::OffsetDateTime;

use super::Session;

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
    /// The visible session indices. Heads keep the incoming display order; an
    /// expanded lineage's other members are gathered beneath their own head.
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

/// D1's rank for one lineage member: newest FIRST, a timestamp-less member last,
/// exact ties broken by `session_id` ascending.
///
/// `Option`'s `Ord` gives `Some > None` and later-time-greater, so `Reverse` puts
/// the newest member first and a timestamp-less one last. This is the exact key
/// `App::order_filtered` sorts on, so a lineage ranked here lands where the
/// display ordering already puts it.
///
/// The ONE place D1's ordering is written down: [`head_of`] takes its minimum and
/// [`fold`] sorts gathered members by it, so the head can never drift from the top
/// of the run drawn beneath it.
fn member_rank(session: &Session) -> (Reverse<Option<OffsetDateTime>>, &str) {
    (Reverse(session.timestamp), session.session_id.as_str())
}

/// The lineage's head: the member with the NEWEST timestamp (a member with no
/// timestamp sorts last), tie-broken by `session_id` ascending — [`member_rank`].
///
/// Newest — NOT most-messages — because the board already sorts repo -> branch
/// -> timestamp-desc and ranks groups by their MAX timestamp. A head chosen any
/// other way could carry a timestamp below its own lineage's max, and the folded
/// row would then sort incoherently against the very rows it stands for. (The two
/// rules disagree in only 1 of 24 measured lineages, so this is nearly free.)
///
/// # Panics
///
/// Panics if `members` is empty. That is an internal precondition, not hostile
/// input: [`fold`] only ever builds member sets by pushing into them.
pub fn head_of(sessions: &[Session], members: &[usize]) -> usize {
    members
        .iter()
        .copied()
        .min_by_key(|&i| member_rank(&sessions[i]))
        .expect("head_of requires a non-empty lineage")
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
/// - **Heads keep their incoming order.** The visible set is never re-sorted, so
///   folding and expanding can never re-rank one head against another.
/// - **An expanded lineage's other members are GATHERED** immediately beneath
///   their head, in the lineage's own [`member_rank`] order.
///
/// Gathering is deliberate, and filtering alone does NOT produce it: time
/// scatters a lineage (measured over the real store, 18 of 27 head->child pairs
/// have unrelated rows between them), so a merely-unhidden child lands at its own
/// timestamp slot, detached from the head that explains it. Since every member
/// shares its head's label, such a row reads as an orphan.
///
/// It is safe by construction: D4 scopes a lineage to one `(repo, branch)`, so a
/// gathered member stays INSIDE its own group and same-group rows stay contiguous
/// with exactly one head — `tui::app::build_rows`' invariant. D1 makes the head
/// the newest member of its lineage, so a child only ever moves UP toward its
/// head and can never land above it.
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
    // Head -> the members drawn beneath it, for expanded lineages only.
    let mut gathered: HashMap<usize, Vec<usize>> = HashMap::new();
    // Every non-head member of a multi-member lineage: dropped where it sits, then
    // either left out (collapsed) or re-emitted under its head (expanded).
    let mut displaced: HashSet<usize> = HashSet::new();
    for (key, group) in &members {
        // A lineage of one has nothing to hide and nothing to gather.
        if group.len() < 2 {
            continue;
        }
        let head = head_of(sessions, group);
        let mut rest: Vec<usize> = group.iter().copied().filter(|&i| i != head).collect();
        displaced.extend(rest.iter().copied());
        if expanded.contains(key) {
            rest.sort_by_key(|&i| member_rank(&sessions[i]));
            gathered.insert(head, rest);
        } else {
            hidden.insert(head, rest.len());
        }
    }

    // One pass over the incoming order: heads stay put, and an expanded head's
    // members follow it immediately. Deterministic despite `HashMap` iteration —
    // `filtered` drives the order and each lineage is independent.
    let mut visible = Vec::with_capacity(filtered.len());
    for &i in filtered {
        if let Some(rest) = gathered.get(&i) {
            visible.push(i);
            visible.extend(rest.iter().copied());
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
}
