//! The fold: events → state, and the rules it enforces in file order (PLAN §2.4, §2.5, §7).

use super::*;
use crate::review::fold::{check_status, check_thread_state};

fn review_id() -> ReviewId {
    ReviewId::parse("r-01j9z3k4m5n6p7q8r9s0t1v2w3").unwrap()
}

/// The events for `bodies`, at times 1, 2, 3, ….
fn events(bodies: Vec<EventBody>) -> Vec<Event> {
    bodies
        .into_iter()
        .enumerate()
        .map(|(n, body)| event_at(n as u64 + 1, body))
        .collect()
}

fn fold_bodies(bodies: Vec<EventBody>) -> Folded {
    fold(&review_id(), &events(bodies)).unwrap()
}

/// The rejection reasons, in order.
fn rejections(folded: &Folded) -> Vec<Rejection> {
    folded
        .warnings
        .iter()
        .map(|warning| match warning {
            LogWarning::Rejected { reason, .. } => reason.clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

#[test]
fn every_kind_folds_into_the_state() {
    let thread = ThreadId::generate();
    let reply = CommentId::generate();
    let folded = fold_bodies(vec![
        created(Target::Working),
        EventBody::Viewed {
            round: 1,
            path: "a.txt".to_string(),
            on: true,
        },
        thread_added(&thread, reviewer()),
        EventBody::ReplyAdded {
            thread: thread.clone(),
            id: reply.clone(),
            body: "On it.".to_string(),
            author: agent(),
            anchor: None,
        },
        status(ReviewStatus::ChangesRequested),
        EventBody::RoundClosed {
            n: 1,
            new_oid: oid('b'),
            old_oid: oid('a'),
            verdict: Some(Verdict::RequestChanges),
            body: Some("Rename it.".to_string()),
        },
        EventBody::ThreadState {
            thread: thread.clone(),
            state: ThreadState::Addressed,
            note: Some("renamed".to_string()),
            author: agent(),
            anchor: None,
        },
        EventBody::BodyEdited {
            target_id: ItemId::Comment(reply.clone()),
            body: "Done.".to_string(),
        },
        EventBody::BodyEdited {
            target_id: ItemId::Thread(thread.clone()),
            body: "Why this name?".to_string(),
        },
        EventBody::PrLinked {
            owner: "kylekz".to_string(),
            repo: "difftest".to_string(),
            number: 4,
        },
        status(ReviewStatus::Ready),
        EventBody::Viewed {
            round: 2,
            path: "b.txt".to_string(),
            on: true,
        },
    ]);
    assert!(folded.warnings.is_empty(), "{:?}", folded.warnings);
    let review = folded.review;
    assert_eq!(review.id, review_id());
    assert_eq!(review.target, Target::Working);
    assert_eq!(review.worktree, "/repo");
    assert_eq!(review.author, reviewer());
    assert_eq!((review.created_at, review.updated_at), (1, 12));
    assert_eq!(review.status, ReviewStatus::Ready);
    assert_eq!(
        review.rounds,
        [Round {
            n: 1,
            old_oid: oid('a'),
            new_oid: oid('b'),
            verdict: Some(Verdict::RequestChanges),
            body: Some("Rename it.".to_string()),
            closed_at: 6,
            closed_from: Some(ReviewStatus::ChangesRequested),
        }]
    );
    assert_eq!(review.current_round(), 2);
    assert!(review.is_viewed(1, "a.txt") && !review.is_viewed(2, "a.txt"));
    assert!(review.is_viewed(2, "b.txt"));
    assert_eq!(
        review.pr,
        Some(PrLink {
            owner: "kylekz".to_string(),
            repo: "difftest".to_string(),
            number: 4
        })
    );

    let [t] = &review.threads[..] else {
        panic!("{:?}", review.threads);
    };
    assert_eq!(t.id, thread);
    assert_eq!(t.body, "Why this name?");
    assert_eq!((t.created_at, t.edited_at), (3, Some(9)));
    assert_eq!(t.author, reviewer());
    assert_eq!(t.anchor, anchor("src/lib.rs", 3));
    assert_eq!(t.state, ThreadState::Addressed);
    assert_eq!(
        t.last_change,
        Some(StateChange {
            state: ThreadState::Addressed,
            note: Some("renamed".to_string()),
            author: agent(),
            at: 7,
        })
    );
    let [r] = &t.replies[..] else {
        panic!("{:?}", t.replies);
    };
    assert_eq!((r.id.clone(), r.body.as_str()), (reply, "Done."));
    assert_eq!(
        (r.created_at, r.edited_at, r.deleted_at),
        (4, Some(8), None)
    );
    assert_eq!(r.author, agent());

    let summary = review.summary();
    assert_eq!(summary.current_round, 2);
    assert_eq!(summary.threads.addressed, 1);
    assert_eq!(summary.threads.open + summary.threads.resolved, 0);
}

#[test]
fn a_pr_review_starts_as_a_draft_and_submission_makes_it_read_only() {
    let pr = Target::Pr {
        owner: "kylekz".to_string(),
        repo: "difftest".to_string(),
        number: 9,
        base_oid: oid('a'),
        head_oid: oid('b'),
    };
    let thread = ThreadId::generate();
    let folded = fold_bodies(vec![
        created(pr),
        thread_added(&thread, reviewer()),
        EventBody::Submitted {
            github_review_id: 77,
            url: "https://example.invalid/r/77".to_string(),
            event: Verdict::Comment,
        },
        // Everything after the submission is refused, a second submission included.
        EventBody::ReplyAdded {
            thread: thread.clone(),
            id: CommentId::generate(),
            body: "late".to_string(),
            author: agent(),
            anchor: None,
        },
        EventBody::Viewed {
            round: 1,
            path: "a".to_string(),
            on: true,
        },
        EventBody::Submitted {
            github_review_id: 78,
            url: "https://example.invalid/r/78".to_string(),
            event: Verdict::Approve,
        },
        status(ReviewStatus::Closed),
    ]);
    assert_eq!(
        rejections(&folded),
        [
            Rejection::Submitted,
            Rejection::Submitted,
            Rejection::Submitted,
            Rejection::Submitted
        ]
    );
    let review = folded.review;
    assert_eq!(review.status, ReviewStatus::Submitted);
    assert!(review.is_pr_review());
    assert_eq!(review.submissions.len(), 1);
    assert_eq!(review.submissions[0].github_review_id, 77);
    assert_eq!(review.submissions[0].at, 3);
    assert_eq!(review.pr.as_ref().map(|pr| pr.number), Some(9));
    assert!(review.threads[0].replies.is_empty());
    assert!(review.viewed.is_empty());
}

#[test]
fn only_a_draft_can_be_submitted() {
    let folded = fold_bodies(vec![
        created(Target::Working),
        EventBody::Submitted {
            github_review_id: 1,
            url: String::new(),
            event: Verdict::Approve,
        },
    ]);
    assert_eq!(
        rejections(&folded),
        [Rejection::NotSubmittable {
            status: ReviewStatus::Open
        }]
    );
    assert!(folded.review.submissions.is_empty());
}

#[test]
fn pr_links_match_a_pr_target_and_can_change_on_a_local_review() {
    let link = |number| EventBody::PrLinked {
        owner: "KyleKZ".to_string(),
        repo: "DiffTest".to_string(),
        number,
    };
    let pr = Target::Pr {
        owner: "kylekz".to_string(),
        repo: "difftest".to_string(),
        number: 9,
        base_oid: oid('a'),
        head_oid: oid('b'),
    };
    let folded = fold_bodies(vec![created(pr), link(9), link(10)]);
    let [Rejection::PrMismatch { target, linked }] = &rejections(&folded)[..] else {
        panic!("{:?}", folded.warnings);
    };
    assert_eq!((target.number, linked.number), (9, 10));
    assert_eq!(folded.review.pr.unwrap().number, 9);
    // Owner and repo compare case-insensitively, so the link above was a no-op.
    assert_eq!(folded.review.updated_at, 1);

    let folded = fold_bodies(vec![created(Target::Working), link(3), link(3), link(4)]);
    assert!(folded.warnings.is_empty());
    assert_eq!(folded.review.pr.unwrap().number, 4);
    assert_eq!(folded.review.updated_at, 4);
}

#[test]
fn events_naming_unknown_ids_are_skipped_with_warnings() {
    let thread = ThreadId::generate();
    let ghost = ThreadId::generate();
    let ghost_reply = CommentId::generate();
    let folded = fold_bodies(vec![
        created(Target::Working),
        thread_added(&thread, reviewer()),
        EventBody::ReplyAdded {
            thread: ghost.clone(),
            id: CommentId::generate(),
            body: "?".to_string(),
            author: agent(),
            anchor: None,
        },
        thread_state(&ghost, ThreadState::Addressed, agent()),
        EventBody::BodyEdited {
            target_id: ItemId::Comment(ghost_reply.clone()),
            body: "?".to_string(),
        },
        EventBody::Deleted {
            target_id: ItemId::Thread(ghost.clone()),
        },
        // Ids are unique: a second thread or reply with a used id is refused.
        thread_added(&thread, agent()),
    ]);
    assert_eq!(
        rejections(&folded),
        [
            Rejection::UnknownThread {
                thread: ghost.clone()
            },
            Rejection::UnknownThread {
                thread: ghost.clone()
            },
            Rejection::UnknownItem {
                item: ItemId::Comment(ghost_reply)
            },
            Rejection::UnknownThread { thread: ghost },
            Rejection::DuplicateId {
                id: thread.to_string()
            },
        ]
    );
    let review = folded.review;
    assert_eq!(review.threads.len(), 1);
    assert_eq!(review.threads[0].author, reviewer());
    assert_eq!(review.updated_at, 2, "skipped events don't touch the state");
    // Each warning names the event and its kind.
    let LogWarning::Rejected { kind, .. } = &folded.warnings[0] else {
        panic!()
    };
    assert_eq!(kind, "reply_added");
}

#[test]
fn deleted_threads_and_replies_are_tombstones() {
    let thread = ThreadId::generate();
    let other = ThreadId::generate();
    let reply = CommentId::generate();
    let folded = fold_bodies(vec![
        created(Target::Working),
        thread_added(&thread, reviewer()),
        thread_added(&other, reviewer()),
        EventBody::ReplyAdded {
            thread: other.clone(),
            id: reply.clone(),
            body: "x".to_string(),
            author: agent(),
            anchor: None,
        },
        EventBody::Deleted {
            target_id: ItemId::Comment(reply.clone()),
        },
        EventBody::Deleted {
            target_id: ItemId::Thread(thread.clone()),
        },
        // Later events naming a tombstone are recognised and refused.
        EventBody::BodyEdited {
            target_id: ItemId::Comment(reply.clone()),
            body: "y".to_string(),
        },
        EventBody::Deleted {
            target_id: ItemId::Comment(reply.clone()),
        },
        EventBody::ReplyAdded {
            thread: thread.clone(),
            id: CommentId::generate(),
            body: "z".to_string(),
            author: agent(),
            anchor: None,
        },
        thread_state(&thread, ThreadState::Resolved, reviewer()),
        EventBody::Deleted {
            target_id: ItemId::Thread(thread.clone()),
        },
        // A thread id is not reusable after deletion either.
        thread_added(&thread, reviewer()),
    ]);
    let deleted_reply = Rejection::Deleted {
        item: ItemId::Comment(reply.clone()),
    };
    let deleted_thread = Rejection::Deleted {
        item: ItemId::Thread(thread.clone()),
    };
    assert_eq!(
        rejections(&folded),
        [
            deleted_reply.clone(),
            deleted_reply,
            deleted_thread.clone(),
            deleted_thread.clone(),
            deleted_thread,
            Rejection::DuplicateId {
                id: thread.to_string()
            },
        ]
    );
    let review = folded.review;
    assert_eq!(review.threads.len(), 2, "tombstones stay in the state");
    assert_eq!(review.threads[0].deleted_at, Some(6));
    assert_eq!(review.threads[0].state, ThreadState::Open);
    assert_eq!(review.threads[1].replies[0].deleted_at, Some(5));
    assert_eq!(review.threads[1].replies[0].body, "x");
    let live: Vec<&ThreadId> = review.live_threads().map(|t| &t.id).collect();
    assert_eq!(live, [&other]);
    assert_eq!(review.summary().threads.open, 1);
}

#[test]
fn replies_in_a_deleted_thread_are_gone_with_it() {
    let thread = ThreadId::generate();
    let reply = CommentId::generate();
    let folded = fold_bodies(vec![
        created(Target::Working),
        thread_added(&thread, reviewer()),
        EventBody::ReplyAdded {
            thread: thread.clone(),
            id: reply.clone(),
            body: "x".to_string(),
            author: agent(),
            anchor: None,
        },
        EventBody::Deleted {
            target_id: ItemId::Thread(thread.clone()),
        },
        EventBody::BodyEdited {
            target_id: ItemId::Comment(reply),
            body: "y".to_string(),
        },
    ]);
    assert_eq!(
        rejections(&folded),
        [Rejection::Deleted {
            item: ItemId::Thread(thread)
        }]
    );
}

/// Who may move a local thread from one state to another (PLAN §2.5).
#[test]
fn thread_state_transitions_follow_the_table() {
    use AuthorKind::{Agent, Github, Reviewer};
    use ThreadState::{Addressed, Open, Resolved};
    let id = ThreadId::generate();
    let states = [Open, Addressed, Resolved];
    for from in states {
        for to in states {
            for by in [Reviewer, Agent, Github] {
                let expected = match (from, to, by) {
                    (_, _, Github) => "refused",
                    (Open, Open, _) | (Resolved, Resolved, _) => "unchanged",
                    // Agents only address; addressing again replaces the note.
                    (Open | Addressed, Addressed, Agent) => "changed",
                    (_, _, Agent) => "refused",
                    // The reviewer may do anything but address a resolved thread.
                    (Resolved, Addressed, Reviewer) => "refused",
                    (_, _, Reviewer) => "changed",
                };
                let actual = match check_thread_state(&id, from, to, by) {
                    Ok(Outcome::Changed) => "changed",
                    Ok(Outcome::Unchanged) => "unchanged",
                    Err(_) => "refused",
                };
                assert_eq!(actual, expected, "{from} -> {to} by {by}");
            }
        }
    }
    // GitHub's threads are never in a log, so nothing there is GitHub's to change.
    assert_eq!(
        check_thread_state(&id, Open, Open, Github),
        Err(Rejection::GithubAuthor)
    );
}

#[test]
fn a_late_address_after_the_reviewer_resolved_is_skipped() {
    // The agent validated "address" against a state where the thread was open, but the
    // reviewer's "resolve" landed first: file order decides, and resolved stays resolved.
    let thread = ThreadId::generate();
    let folded = fold_bodies(vec![
        created(Target::Working),
        thread_added(&thread, reviewer()),
        thread_state(&thread, ThreadState::Resolved, reviewer()),
        thread_state(&thread, ThreadState::Addressed, agent()),
        thread_state(&thread, ThreadState::Open, agent()),
        thread_state(&thread, ThreadState::Open, reviewer()),
        thread_state(&thread, ThreadState::Addressed, agent()),
    ]);
    assert_eq!(
        rejections(&folded),
        [
            Rejection::ThreadTransition {
                from: ThreadState::Resolved,
                to: ThreadState::Addressed,
                author: AuthorKind::Agent
            },
            Rejection::ThreadTransition {
                from: ThreadState::Resolved,
                to: ThreadState::Open,
                author: AuthorKind::Agent
            },
        ]
    );
    let thread = &folded.review.threads[0];
    assert_eq!(thread.state, ThreadState::Addressed);
    assert_eq!(thread.last_change.as_ref().unwrap().at, 7);
}

/// GitHub's threads are shown from the pull request's mirror (PLAN §9.6): a thread, a reply
/// or a state change written as GitHub's is skipped, whatever it names.
#[test]
fn nothing_github_wrote_is_folded() {
    let mirrored = ThreadId::generate();
    let local = ThreadId::generate();
    let reply = |thread: &ThreadId, author: Author| EventBody::ReplyAdded {
        thread: thread.clone(),
        id: CommentId::generate(),
        body: "x".to_string(),
        author,
        anchor: None,
    };
    let folded = fold_bodies(vec![
        created(Target::Working),
        thread_added(&mirrored, Author::github("octocat")),
        thread_added(&local, reviewer()),
        reply(&mirrored, reviewer()),
        reply(&local, Author::github("hubot")),
        thread_state(&local, ThreadState::Resolved, Author::github("hubot")),
        reply(&local, agent()),
    ]);
    assert_eq!(
        rejections(&folded),
        [
            Rejection::GithubAuthor,
            Rejection::UnknownThread {
                thread: mirrored.clone()
            },
            Rejection::GithubAuthor,
            Rejection::GithubAuthor,
        ]
    );
    let [thread] = &folded.review.threads[..] else {
        panic!("{:?}", folded.review.threads);
    };
    assert_eq!((&thread.id, thread.state), (&local, ThreadState::Open));
    assert_eq!(thread.replies.len(), 1);
}

/// Which status moves are allowed (PLAN §2.4).
#[test]
fn status_transitions_follow_the_table() {
    use ReviewStatus::*;
    let all = [
        Open,
        ChangesRequested,
        InProgress,
        Ready,
        Approved,
        Closed,
        Draft,
        Submitted,
    ];
    let active = [Open, ChangesRequested, InProgress, Ready];
    let targets = [ChangesRequested, InProgress, Ready, Approved, Closed];
    for from in all {
        for to in all {
            let expected = if from == to {
                Ok(Outcome::Unchanged)
            } else if (active.contains(&from) && targets.contains(&to))
                || (from == Draft && to == Closed)
            {
                Ok(Outcome::Changed)
            } else {
                Err(Rejection::StatusTransition { from, to })
            };
            assert_eq!(check_status(from, to), expected, "{from} -> {to}");
        }
    }
    for status in [Approved, Closed, Submitted] {
        assert!(status.is_final() && !status.is_open());
    }

    // In a log: a final status sticks, and a local review never becomes a draft.
    let folded = fold_bodies(vec![
        created(Target::Working),
        status(Ready),
        status(Ready),
        status(Draft),
        status(Approved),
        status(Ready),
    ]);
    assert_eq!(
        rejections(&folded),
        [
            Rejection::StatusTransition {
                from: Ready,
                to: Draft
            },
            Rejection::StatusTransition {
                from: Approved,
                to: Ready
            },
        ]
    );
    assert_eq!(folded.review.status, Approved);
    assert_eq!(
        folded.review.updated_at, 5,
        "the repeated status was a no-op"
    );
}

#[test]
fn rounds_close_in_order_and_viewed_needs_an_existing_round() {
    let round = |n| EventBody::RoundClosed {
        n,
        new_oid: oid('b'),
        old_oid: oid('a'),
        verdict: None,
        body: None,
    };
    let viewed = |round, path: &str, on| EventBody::Viewed {
        round,
        path: path.to_string(),
        on,
    };
    let folded = fold_bodies(vec![
        created(Target::Working),
        round(2),
        round(1),
        round(1),
        viewed(3, "a", true),
        viewed(2, "a", true),
        viewed(2, "a", true),
        viewed(1, "b", true),
        viewed(1, "b", false),
        viewed(2, "../a", true),
        viewed(0, "a", true),
    ]);
    assert_eq!(
        rejections(&folded),
        [
            Rejection::RoundOrder {
                expected: 1,
                got: 2
            },
            Rejection::RoundOrder {
                expected: 2,
                got: 1
            },
            Rejection::UnknownRound {
                round: 3,
                current: 2
            },
            Rejection::InvalidPath {
                path: "../a".to_string()
            },
            Rejection::UnknownRound {
                round: 0,
                current: 2
            },
        ]
    );
    let review = folded.review;
    assert_eq!(review.rounds.len(), 1);
    assert_eq!(review.rounds[0].verdict, None);
    assert!(review.is_viewed(2, "a"));
    assert!(!review.is_viewed(1, "b"));
    assert!(
        !review.viewed.contains_key(&1),
        "an emptied round leaves no entry"
    );
}

#[test]
fn anchors_are_checked() {
    let bad = |edit: fn(&mut Anchor)| {
        let mut anchor = anchor("src/lib.rs", 3);
        edit(&mut anchor);
        EventBody::ThreadAdded {
            id: ThreadId::generate(),
            anchor,
            body: "x".to_string(),
            author: reviewer(),
            annotation: false,
        }
    };
    let folded = fold_bodies(vec![
        created(Target::Working),
        bad(|a| a.path = "/abs".to_string()),
        bad(|a| a.path = "a/../b".to_string()),
        bad(|a| a.old_path = Some(String::new())),
        bad(|a| a.start_line = 0),
        bad(|a| a.end_line = 2),
        bad(|a| a.end_line = 4),
        bad(|a| a.round = 2),
        bad(|a| {
            a.end_line = 5;
            a.text = vec!["3".into(), "4".into(), "5".into()];
            a.old_path = Some("old/lib.rs".into());
            a.side = AnchorSide::Old;
        }),
    ]);
    let reasons = rejections(&folded);
    assert_eq!(reasons.len(), 7, "{reasons:?}");
    assert!(
        reasons[..6]
            .iter()
            .all(|r| matches!(r, Rejection::InvalidAnchor { .. })),
        "{reasons:?}"
    );
    assert_eq!(
        reasons[6],
        Rejection::UnknownRound {
            round: 2,
            current: 1
        }
    );
    let [thread] = &folded.review.threads[..] else {
        panic!()
    };
    assert_eq!(thread.anchor.line_count(), 3);
}

/// A file-level anchor on `path` (a comment on the whole file) in round 1.
fn file_anchor(path: &str, side: AnchorSide) -> Anchor {
    Anchor::file_level(1, path.to_string(), None, side)
}

#[test]
fn file_level_anchors_fold_and_malformed_ones_are_refused() {
    let added = |anchor: Anchor| EventBody::ThreadAdded {
        id: ThreadId::generate(),
        anchor,
        body: "About the whole file".to_string(),
        author: reviewer(),
        annotation: false,
    };
    let bad = |edit: fn(&mut Anchor)| {
        let mut anchor = file_anchor("src/lib.rs", AnchorSide::New);
        edit(&mut anchor);
        added(anchor)
    };
    let folded = fold_bodies(vec![
        created(Target::Working),
        added(file_anchor("src/lib.rs", AnchorSide::New)),
        // The old side, for a deleted file.
        added(file_anchor("src/gone.rs", AnchorSide::Old)),
        // Lines 0 with text or context, or one end 0 only, are no file-level anchor.
        bad(|a| a.text = vec!["fn main() {}".into()]),
        bad(|a| a.context_before = vec!["use std::io;".into()]),
        bad(|a| a.context_after = vec!["}".into()]),
        bad(|a| a.end_line = 3),
        bad(|a| {
            a.start_line = 3;
            a.text = vec!["3".into()];
        }),
        bad(|a| a.path = "../x".to_string()),
        bad(|a| a.round = 2),
    ]);
    let reasons = rejections(&folded);
    assert_eq!(reasons.len(), 7, "{reasons:?}");
    assert!(
        reasons[..3].iter().all(|r| matches!(
            r,
            Rejection::InvalidAnchor { reason } if reason.contains("file-level")
        )),
        "{reasons:?}"
    );
    assert!(
        reasons[3..6]
            .iter()
            .all(|r| matches!(r, Rejection::InvalidAnchor { .. })),
        "{reasons:?}"
    );
    assert_eq!(
        reasons[6],
        Rejection::UnknownRound {
            round: 2,
            current: 1
        }
    );
    let [new, old] = &folded.review.threads[..] else {
        panic!("{:?}", folded.review.threads)
    };
    assert!(new.anchor.is_file_level() && old.anchor.is_file_level());
    assert_eq!(new.anchor.line_count(), 0);
    assert_eq!(old.anchor.side, AnchorSide::Old);

    // A file-level anchor recorded on a reply (its file renamed since) moves the thread there,
    // keeping the first; a malformed one skips the reply.
    let thread = ThreadId::generate();
    let reply_with = |anchor: Anchor| EventBody::ReplyAdded {
        thread: thread.clone(),
        id: CommentId::generate(),
        body: "Renamed it.".to_string(),
        author: agent(),
        anchor: Some(anchor),
    };
    let mut with_text = file_anchor("src/renamed.rs", AnchorSide::New);
    with_text.text = vec!["x".into()];
    let folded = fold_bodies(vec![
        created(Target::Working),
        EventBody::ThreadAdded {
            id: thread.clone(),
            anchor: file_anchor("src/lib.rs", AnchorSide::New),
            body: "About the whole file".to_string(),
            author: reviewer(),
            annotation: false,
        },
        reply_with(with_text),
        reply_with(file_anchor("src/renamed.rs", AnchorSide::New)),
    ]);
    assert_eq!(rejections(&folded).len(), 1, "{:?}", folded.warnings);
    let t = &folded.review.threads[0];
    assert_eq!(t.anchor, file_anchor("src/renamed.rs", AnchorSide::New));
    assert_eq!(
        t.original_anchor,
        Some(file_anchor("src/lib.rs", AnchorSide::New))
    );
    assert_eq!(t.replies.len(), 1);
}

#[test]
fn the_review_starts_at_its_created_event() {
    let id = review_id();
    // Nothing before `created` applies, a second `created` is refused, and a log without one
    // has no review.
    let early = event_at(1, status(ReviewStatus::Closed));
    let first = event_at(2, created(Target::Working));
    let second = event_at(3, created(Target::Staged));
    let folded = fold(&id, &[early.clone(), first, second.clone()]).unwrap();
    assert_eq!(folded.review.target, Target::Working);
    assert_eq!(folded.review.status, ReviewStatus::Open);
    assert_eq!(folded.review.created_at, 2);
    let rejected: Vec<(EventId, Rejection)> = folded
        .warnings
        .iter()
        .map(|w| match w {
            LogWarning::Rejected { eid, reason, .. } => (eid.clone(), reason.clone()),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        rejected,
        [
            (early.eid.clone(), Rejection::NotCreated),
            (second.eid, Rejection::AlreadyCreated)
        ]
    );
    assert_eq!(fold(&id, &[early]), Err(FoldError::NotCreated));
    assert_eq!(fold(&id, &[]), Err(FoldError::NotCreated));
}

#[test]
fn an_event_that_lands_twice_folds_once() {
    let thread = ThreadId::generate();
    let base = events(vec![
        created(Target::Working),
        thread_added(&thread, reviewer()),
    ]);
    let reply = event_at(
        3,
        EventBody::ReplyAdded {
            thread: thread.clone(),
            id: CommentId::generate(),
            body: "x".to_string(),
            author: agent(),
            anchor: None,
        },
    );
    let mut all = base;
    all.push(reply.clone());
    all.push(reply.clone());
    let folded = fold(&review_id(), &all).unwrap();
    assert_eq!(rejections(&folded), [Rejection::DuplicateEvent]);
    assert_eq!(folded.review.threads[0].replies.len(), 1);
}

#[test]
fn a_log_folds_through_bad_lines() {
    // A torn last line, a hand-mangled line and a newer dv's event, around valid ones.
    let thread = ThreadId::generate();
    let good = events(vec![
        created(Target::Working),
        thread_added(&thread, reviewer()),
        thread_state(&thread, ThreadState::Resolved, reviewer()),
    ]);
    let mut log = log_of(&good[..2]);
    log.extend(b"{\"v\":1,\"eid\":\n");
    log.extend(br#"{"v":1,"eid":"e-01j9z3k4m5n6p7q8r9s0t1v2w3","at":9,"kind":"thread_pinned","thread":"x"}"#);
    log.extend(b"\n");
    log.extend(log_of(&good[2..]));
    let torn = event_at(4, thread_state(&thread, ThreadState::Open, reviewer()))
        .to_line()
        .unwrap();
    log.extend(&torn[..torn.len() - 3]);

    let parsed = parse_log(&log);
    assert_eq!(parsed.newer_events, 1);
    assert_eq!(parsed.warnings.len(), 2, "{:?}", parsed.warnings);
    let folded = fold(&review_id(), &parsed.events).unwrap();
    assert!(folded.warnings.is_empty());
    assert_eq!(folded.review.threads[0].state, ThreadState::Resolved);
}

#[test]
fn rejections_and_warnings_serialise_for_the_cache() {
    let warnings = vec![
        LogWarning::Malformed {
            line: 3,
            error: "expected value".to_string(),
        },
        LogWarning::Rejected {
            eid: EventId::generate(),
            kind: "thread_state".to_string(),
            reason: Rejection::ThreadTransition {
                from: ThreadState::Resolved,
                to: ThreadState::Addressed,
                author: AuthorKind::Agent,
            },
        },
        LogWarning::Rejected {
            eid: EventId::generate(),
            kind: "status".to_string(),
            reason: Rejection::Submitted,
        },
    ];
    let json = serde_json::to_string(&warnings).unwrap();
    assert_eq!(
        serde_json::from_str::<Vec<LogWarning>>(&json).unwrap(),
        warnings
    );
    assert_eq!(warnings[0].to_string(), "line 3: expected value");
    assert!(
        warnings[1]
            .to_string()
            .ends_with(": an agent can't move a thread from resolved to addressed"),
        "{}",
        warnings[1]
    );
}

/// `anchor("src/lib.rs", line)` moved into `round`.
fn moved_anchor(line: u32, round: u32) -> Anchor {
    Anchor {
        round,
        ..anchor("src/lib.rs", line)
    }
}

#[test]
fn a_moved_threads_next_events_record_its_anchor_and_keep_the_first() {
    let thread = ThreadId::generate();
    let reply_with = |anchor: Option<Anchor>| EventBody::ReplyAdded {
        thread: thread.clone(),
        id: CommentId::generate(),
        body: "Moved it.".to_string(),
        author: agent(),
        anchor,
    };
    let state_with = |state, author, anchor| EventBody::ThreadState {
        thread: thread.clone(),
        state,
        note: None,
        author,
        anchor,
    };
    let mut reversed = moved_anchor(9, 2);
    reversed.end_line = 8;
    let folded = fold_bodies(vec![
        created(Target::Working),
        thread_added(&thread, reviewer()),
        EventBody::RoundClosed {
            n: 1,
            new_oid: oid('b'),
            old_oid: oid('a'),
            verdict: Some(Verdict::RequestChanges),
            body: None,
        },
        // No anchor: the thread stays where it was opened.
        reply_with(None),
        // The first move keeps the anchor the thread was opened with.
        reply_with(Some(moved_anchor(5, 2))),
        // A second move replaces the current anchor only.
        state_with(ThreadState::Addressed, agent(), Some(moved_anchor(7, 2))),
        // An invalid anchor skips the whole event, its reply or state change included.
        reply_with(Some(reversed)),
        state_with(ThreadState::Resolved, reviewer(), Some(moved_anchor(8, 3))),
        state_with(ThreadState::Resolved, reviewer(), None),
        // A state change that changes nothing records nothing, its anchor included.
        state_with(ThreadState::Resolved, reviewer(), Some(moved_anchor(11, 2))),
    ]);
    let reasons = rejections(&folded);
    assert_eq!(reasons.len(), 2, "{reasons:?}");
    assert!(
        matches!(&reasons[0], Rejection::InvalidAnchor { reason } if reason.contains("reversed")),
        "{reasons:?}"
    );
    assert_eq!(
        reasons[1],
        Rejection::UnknownRound {
            round: 3,
            current: 2
        }
    );
    let t = &folded.review.threads[0];
    assert_eq!(t.anchor, moved_anchor(7, 2));
    assert_eq!(t.original_anchor, Some(anchor("src/lib.rs", 3)));
    assert_eq!(t.first_anchor(), &anchor("src/lib.rs", 3));
    assert_eq!(t.replies.len(), 2);
    assert_eq!(t.state, ThreadState::Resolved);

    // A thread that never moved has no original anchor; its first anchor is its anchor.
    let folded = fold_bodies(vec![
        created(Target::Working),
        thread_added(&thread, reviewer()),
        reply_added(&thread, "Looks fine.", agent()),
        // Recording the anchor it already has is no move.
        reply_with(Some(anchor("src/lib.rs", 3))),
    ]);
    assert!(folded.warnings.is_empty(), "{:?}", folded.warnings);
    let t = &folded.review.threads[0];
    assert_eq!(t.original_anchor, None);
    assert_eq!(t.first_anchor(), &t.anchor);
}

#[test]
fn status_events_keep_their_note_and_author() {
    let with_note = |status, note: Option<&str>, author: Option<Author>| EventBody::Status {
        status,
        note: note.map(str::to_string),
        author,
        if_status: None,
        if_round: None,
    };
    let bodies = [
        created(Target::Working),
        with_note(ReviewStatus::ChangesRequested, None, Some(reviewer())),
        with_note(ReviewStatus::Ready, Some("Renamed both."), Some(agent())),
        // The same note again changes nothing; another one replaces it; none is a no-op.
        with_note(ReviewStatus::Ready, Some("Renamed both."), Some(agent())),
        with_note(
            ReviewStatus::Ready,
            Some("Renamed both, and the test."),
            Some(agent()),
        ),
        with_note(ReviewStatus::Ready, None, Some(agent())),
        // An older writer's status, without either.
        status(ReviewStatus::Approved),
        // A finished review takes no new note.
        with_note(ReviewStatus::Approved, Some("late"), Some(reviewer())),
    ];
    let change = |count: usize| {
        let folded = fold_bodies(bodies[..count].to_vec());
        assert!(folded.warnings.is_empty(), "{:?}", folded.warnings);
        (folded.review.status_change, folded.review.updated_at)
    };
    assert_eq!(change(1), (None, 1));
    assert_eq!(
        change(2),
        (
            Some(StatusChange {
                status: ReviewStatus::ChangesRequested,
                note: None,
                author: Some(reviewer()),
                at: 2,
            }),
            2
        )
    );
    let ready = |note: &str, at| StatusChange {
        status: ReviewStatus::Ready,
        note: Some(note.to_string()),
        author: Some(agent()),
        at,
    };
    assert_eq!(change(3), (Some(ready("Renamed both.", 3)), 3));
    assert_eq!(change(4), (Some(ready("Renamed both.", 3)), 3));
    assert_eq!(
        change(5),
        (Some(ready("Renamed both, and the test.", 5)), 5)
    );
    assert_eq!(
        change(6),
        (Some(ready("Renamed both, and the test.", 5)), 5)
    );
    let approved = StatusChange {
        status: ReviewStatus::Approved,
        note: None,
        author: None,
        at: 7,
    };
    assert_eq!(change(7), (Some(approved.clone()), 7));
    assert_eq!(change(8), (Some(approved), 7));
}

#[test]
fn a_conditional_status_applies_only_on_its_status() {
    let start_work = EventBody::Status {
        status: ReviewStatus::InProgress,
        note: None,
        author: Some(agent()),
        if_status: Some(ReviewStatus::ChangesRequested),
        if_round: None,
    };
    let ready = EventBody::Status {
        status: ReviewStatus::Ready,
        note: Some("Done.".to_string()),
        author: Some(agent()),
        if_status: None,
        if_round: None,
    };
    // Where changes are requested, it applies.
    let folded = fold_bodies(vec![
        created(Target::Working),
        status(ReviewStatus::ChangesRequested),
        start_work.clone(),
    ]);
    assert!(folded.warnings.is_empty(), "{:?}", folded.warnings);
    assert_eq!(folded.review.status, ReviewStatus::InProgress);
    // After the agent's handoff landed first, it is a no-op, not a bad event.
    let folded = fold_bodies(vec![
        created(Target::Working),
        status(ReviewStatus::ChangesRequested),
        ready,
        start_work,
    ]);
    assert!(folded.warnings.is_empty(), "{:?}", folded.warnings);
    assert_eq!(folded.review.status, ReviewStatus::Ready);
    assert_eq!(
        folded
            .review
            .status_change
            .and_then(|change| change.note)
            .as_deref(),
        Some("Done.")
    );
}
