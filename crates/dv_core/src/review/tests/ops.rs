//! The checked operations (PLAN §2.4, §2.5): one open review per target, thread and status
//! rules by author, read-only submitted reviews.

use super::*;
use crate::git::GitRepo;
use crate::review::ops::{
    add_thread, close_round, delete_item, edit_body, in_context, is_review_of, link_pr,
    mark_viewed, open_review, record_submission, reply, reviews_in_context, set_status,
    set_thread_state, start_work,
};
use crate::testing::open_at;

fn pr_target(head: char) -> Target {
    Target::Pr {
        owner: "kylekz".to_string(),
        repo: "difftest".to_string(),
        number: 12,
        base_oid: oid('a'),
        head_oid: oid(head),
    }
}

/// A store with one open `working` review.
fn with_review() -> (FixtureRepo, ReviewStore, ReviewId) {
    let (repo, store) = fixture_store();
    let opened = open_review(&store, Target::Working, reviewer()).unwrap();
    assert!(opened.created);
    (repo, store, opened.review.id)
}

fn load(store: &ReviewStore, id: &ReviewId) -> Review {
    let loaded = store.load(id).unwrap();
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    loaded.review
}

fn rejected(result: Result<impl std::fmt::Debug, OpError>) -> Rejection {
    match result {
        Err(OpError::Rejected(reason)) => reason,
        other => panic!("expected a rejection, got {other:?}"),
    }
}

/// An author of `kind`.
fn by(kind: AuthorKind) -> Author {
    match kind {
        AuthorKind::Reviewer => reviewer(),
        AuthorKind::Agent => agent(),
        AuthorKind::Github => Author::github("octocat"),
    }
}

fn not_allowed(result: Result<impl std::fmt::Debug, OpError>) -> String {
    match result {
        Err(err @ OpError::NotAllowed { .. }) => err.to_string(),
        other => panic!("expected NotAllowed, got {other:?}"),
    }
}

#[test]
fn opening_a_target_resumes_its_open_review() {
    let (repo, store) = fixture_store();
    let first = open_review(&store, Target::Working, reviewer()).unwrap();
    assert!(first.created);
    let again = open_review(&store, Target::Working, agent()).unwrap();
    assert!(!again.created);
    assert_eq!(again.review.id, first.review.id);
    assert_eq!(again.review.author, reviewer());

    // Another target is another review.
    let staged = open_review(&store, Target::Staged, reviewer()).unwrap();
    assert!(staged.created);
    assert_ne!(staged.review.id, first.review.id);

    // A finished review doesn't count: the next open starts a new one.
    set_status(
        &store,
        &first.review.id,
        ReviewStatus::Approved,
        &by(AuthorKind::Reviewer),
        None,
    )
    .unwrap();
    let next = open_review(&store, Target::Working, reviewer()).unwrap();
    assert!(next.created);
    assert_ne!(next.review.id, first.review.id);
    assert!(
        !open_review(&store, Target::Working, reviewer())
            .unwrap()
            .created
    );

    // An unreadable log among the reviews doesn't stop opening.
    store
        .backend()
        .store_append(&format!("reviews/{}.jsonl", ReviewId::generate()), b"junk")
        .unwrap();
    assert_eq!(
        open_review(&store, Target::Working, reviewer())
            .unwrap()
            .review
            .id,
        next.review.id
    );
    drop(repo);
}

#[test]
fn working_reviews_are_per_worktree_and_commit_reviews_are_shared() {
    let (repo, main) = fixture_store();
    let worktree = repo.add_worktree("wt", "feat");
    let linked = ReviewStore::new(Arc::new(open_at(&repo, &worktree)));

    let main_working = open_review(&main, Target::Working, reviewer()).unwrap();
    let linked_working = open_review(&linked, Target::Working, reviewer()).unwrap();
    assert!(linked_working.created, "another worktree's changes");
    assert_ne!(linked_working.review.id, main_working.review.id);
    assert_eq!(
        open_review(&main, Target::Working, reviewer())
            .unwrap()
            .review
            .id,
        main_working.review.id
    );

    let commit = Target::Commit {
        rev: "HEAD".to_string(),
    };
    let from_main = open_review(&main, commit.clone(), reviewer()).unwrap();
    let from_linked = open_review(&linked, commit.clone(), reviewer()).unwrap();
    assert!(!from_linked.created, "the same commits from any worktree");
    assert_eq!(from_linked.review.id, from_main.review.id);

    // The rule open_review resumes by, as the switcher asks it of a listed summary.
    let summary = main_working.review.summary();
    assert!(is_review_of(&summary, &Target::Working, main.worktree()));
    assert!(!is_review_of(&summary, &Target::Working, linked.worktree()));
    assert!(!is_review_of(&summary, &Target::Staged, main.worktree()));
    let summary = from_main.review.summary();
    assert!(is_review_of(&summary, &commit, linked.worktree()));
}

#[test]
fn a_pr_review_resumes_by_pr_whatever_head_it_was_fetched_at() {
    let (_repo, store) = fixture_store();
    let first = open_review(&store, pr_target('b'), reviewer()).unwrap();
    assert_eq!(first.review.status, ReviewStatus::Draft);
    let pushed = open_review(&store, pr_target('c'), reviewer()).unwrap();
    assert!(!pushed.created);
    assert_eq!(pushed.review.id, first.review.id);

    // After submission the next round is a new draft for the same PR (PLAN §2.4).
    record_submission(
        &store,
        &first.review.id,
        5,
        "https://x.invalid/5",
        Verdict::Comment,
    )
    .unwrap();
    let round_two = open_review(&store, pr_target('c'), reviewer()).unwrap();
    assert!(round_two.created);
    assert_eq!(round_two.review.status, ReviewStatus::Draft);
}

#[test]
fn threads_and_replies() {
    let (_repo, store, id) = with_review();
    let thread = add_thread(
        &store,
        &id,
        anchor("src/lib.rs", 2),
        "Rename this?",
        reviewer(),
    )
    .unwrap();
    let answer = reply(&store, &id, &thread, "Renamed.", agent(), None).unwrap();
    let review = load(&store, &id);
    let t = review.thread(&thread).unwrap();
    assert_eq!(t.body, "Rename this?");
    assert_eq!(t.replies[0].id, answer);
    assert_eq!(t.replies[0].author, agent());

    // Bodies can't be blank, anchors must be well formed, threads must exist.
    assert!(matches!(
        add_thread(&store, &id, anchor("src/lib.rs", 2), "  \n", reviewer()),
        Err(OpError::Empty { .. })
    ));
    assert!(matches!(
        reply(&store, &id, &thread, "", agent(), None),
        Err(OpError::Empty { .. })
    ));
    let mut bad = anchor("src/lib.rs", 2);
    bad.end_line = 3;
    assert!(matches!(
        rejected(add_thread(&store, &id, bad, "x", reviewer())),
        Rejection::InvalidAnchor { .. }
    ));
    let mut later = anchor("src/lib.rs", 2);
    later.round = 2;
    assert_eq!(
        rejected(add_thread(&store, &id, later, "x", reviewer())),
        Rejection::UnknownRound {
            round: 2,
            current: 1
        }
    );
    let ghost = ThreadId::generate();
    assert_eq!(
        rejected(reply(&store, &id, &ghost, "x", agent(), None)),
        Rejection::UnknownThread { thread: ghost }
    );
    assert!(matches!(
        add_thread(
            &store,
            &ReviewId::generate(),
            anchor("a", 1),
            "x",
            reviewer()
        ),
        Err(OpError::Store(StoreError::UnknownReview { .. }))
    ));
    // Nothing refused was written.
    assert_eq!(store.events(&id).unwrap().events.len(), 3);
}

#[test]
fn agents_address_and_only_the_reviewer_resolves_or_reopens() {
    let (_repo, store, id) = with_review();
    let thread = add_thread(&store, &id, anchor("src/lib.rs", 1), "Fix", reviewer()).unwrap();
    let set =
        |state, note, author| set_thread_state(&store, &id, &thread, state, note, author, None);

    // An agent is told that resolving is the reviewer's, before the thread's state counts.
    match set(ThreadState::Resolved, None, agent()) {
        Err(error @ OpError::NotAllowed { .. }) => assert_eq!(
            error.to_string(),
            "an agent can't resolve a thread: only the reviewer resolves threads"
        ),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        set(ThreadState::Open, None, agent()),
        Err(OpError::NotAllowed { .. })
    ));
    assert!(set(ThreadState::Addressed, Some(" did it "), agent()).unwrap());
    let t = load(&store, &id).threads[0].clone();
    assert_eq!(t.state, ThreadState::Addressed);
    let change = t.last_change.unwrap();
    assert_eq!(change.note.as_deref(), Some("did it"));
    assert_eq!(change.author, agent());
    // Addressing again replaces the note.
    assert!(set(ThreadState::Addressed, Some("did it properly"), agent()).unwrap());
    assert!(matches!(
        set(ThreadState::Open, None, agent()),
        Err(OpError::NotAllowed { .. })
    ));
    assert!(set(ThreadState::Open, Some("not quite"), reviewer()).unwrap());
    assert!(!set(ThreadState::Open, None, reviewer()).unwrap(), "no-op");
    assert!(set(ThreadState::Resolved, None, reviewer()).unwrap());
    assert!(matches!(
        rejected(set(ThreadState::Addressed, None, agent())),
        Rejection::ThreadTransition { .. }
    ));
    assert!(matches!(
        rejected(set(ThreadState::Resolved, None, Author::github("octocat"))),
        Rejection::GithubOnLocalThread { .. }
    ));
    assert_eq!(load(&store, &id).threads[0].state, ThreadState::Resolved);
    // The no-op and the refusals wrote nothing: `created`, the thread and four changes.
    assert_eq!(store.events(&id).unwrap().events.len(), 1 + 1 + 4);
}

#[test]
fn github_threads_are_read_only() {
    let (_repo, store, id) = with_review();
    let mirrored = add_thread(
        &store,
        &id,
        anchor("src/lib.rs", 1),
        "From GitHub",
        Author::github("octocat"),
    )
    .unwrap();
    assert!(matches!(
        rejected(reply(&store, &id, &mirrored, "x", reviewer(), None)),
        Rejection::ReadOnlyThread { .. }
    ));
    assert!(matches!(
        rejected(set_thread_state(
            &store,
            &id,
            &mirrored,
            ThreadState::Resolved,
            None,
            reviewer(),
            None
        )),
        Rejection::ReadOnlyThread { .. }
    ));
    let item = ItemId::Thread(mirrored);
    not_allowed(edit_body(&store, &id, &item, "x", &reviewer()));
    not_allowed(delete_item(&store, &id, &item, &reviewer()));
}

#[test]
fn status_changes_by_who_may_make_them() {
    let (_repo, store, id) = with_review();
    let message = not_allowed(set_status(
        &store,
        &id,
        ReviewStatus::Approved,
        &by(AuthorKind::Agent),
        None,
    ));
    assert_eq!(message, "an agent can't set a review's status to approved");
    not_allowed(set_status(
        &store,
        &id,
        ReviewStatus::Ready,
        &by(AuthorKind::Reviewer),
        None,
    ));
    not_allowed(set_status(
        &store,
        &id,
        ReviewStatus::Closed,
        &by(AuthorKind::Github),
        None,
    ));
    for status in [
        ReviewStatus::Open,
        ReviewStatus::Draft,
        ReviewStatus::Submitted,
    ] {
        for actor in [AuthorKind::Reviewer, AuthorKind::Agent] {
            not_allowed(set_status(&store, &id, status, &by(actor), None));
        }
    }

    // The core loop (PLAN §3 A): request changes, the agent works and is done, approve.
    let steps = [
        (ReviewStatus::ChangesRequested, AuthorKind::Reviewer),
        (ReviewStatus::InProgress, AuthorKind::Agent),
        (ReviewStatus::Ready, AuthorKind::Agent),
        (ReviewStatus::ChangesRequested, AuthorKind::Reviewer),
        (ReviewStatus::Ready, AuthorKind::Agent),
        (ReviewStatus::Approved, AuthorKind::Reviewer),
    ];
    for (status, actor) in steps {
        assert!(
            set_status(&store, &id, status, &by(actor), None).unwrap(),
            "{status}"
        );
        assert_eq!(load(&store, &id).status, status);
    }
    assert!(
        !set_status(
            &store,
            &id,
            ReviewStatus::Approved,
            &by(AuthorKind::Reviewer),
            None
        )
        .unwrap()
    );
    // A finished review moves nowhere, and says it is finished, as every other write to it
    // does: the agent's `dv review done` racing the approval, the reviewer's close after it.
    assert_eq!(
        finished(set_status(
            &store,
            &id,
            ReviewStatus::Ready,
            &by(AuthorKind::Agent),
            Some("Done.")
        )),
        ReviewStatus::Approved
    );
    assert_eq!(
        finished(set_status(
            &store,
            &id,
            ReviewStatus::Closed,
            &by(AuthorKind::Reviewer),
            None
        )),
        ReviewStatus::Approved
    );
    assert_eq!(load(&store, &id).status, ReviewStatus::Approved);
}

#[test]
fn a_draft_pr_review_is_abandoned_with_closed_not_with_local_statuses() {
    let (_repo, store) = fixture_store();
    let id = open_review(&store, pr_target('b'), reviewer())
        .unwrap()
        .review
        .id;
    assert!(matches!(
        rejected(set_status(
            &store,
            &id,
            ReviewStatus::ChangesRequested,
            &by(AuthorKind::Reviewer),
            None
        )),
        Rejection::StatusTransition { .. }
    ));
    assert!(
        set_status(
            &store,
            &id,
            ReviewStatus::Closed,
            &by(AuthorKind::Reviewer),
            None
        )
        .unwrap()
    );
    // Abandoned, it is still a PR review: that is its target's kind, not its status.
    let abandoned = load(&store, &id);
    assert_eq!(abandoned.status, ReviewStatus::Closed);
    assert!(abandoned.is_pr_review());
}

#[test]
fn operations_fold_without_rewriting_the_snapshot_cache() {
    // An operation's own append makes a snapshot of the state it loaded stale at once, so it
    // folds without writing one (PLAN §7); a fresh snapshot still answers its load.
    let (_repo, store, id) = with_review();
    let rel = format!("reviews/{id}.state.json");
    let cache = || store.backend().store_read(&rel).unwrap();
    assert_eq!(cache(), None);
    add_thread(&store, &id, anchor("src/lib.rs", 1), "Why?", reviewer()).unwrap();
    assert!(mark_viewed(&store, &id, "src/lib.rs", true).unwrap());
    assert!(
        set_status(
            &store,
            &id,
            ReviewStatus::Ready,
            &by(AuthorKind::Agent),
            None
        )
        .unwrap()
    );
    assert_eq!(cache(), None, "no operation wrote the snapshot cache");

    // A load (the list, the UI) writes it; operations leave it as it is.
    assert!(!store.load(&id).unwrap().from_cache);
    let written = cache().expect("a load writes the cache");
    let mut doctored: serde_json::Value = serde_json::from_slice(&written).unwrap();
    doctored["review"]["status"] = "approved".into();
    let doctored = doctored.to_string().into_bytes();
    store.backend().store_write_atomic(&rel, &doctored).unwrap();
    // The operation's load came from the (doctored) cache: from `approved` nothing may move.
    assert_eq!(
        finished(set_status(
            &store,
            &id,
            ReviewStatus::ChangesRequested,
            &by(AuthorKind::Reviewer),
            None
        )),
        ReviewStatus::Approved
    );
    assert!(
        set_status(
            &store,
            &id,
            ReviewStatus::InProgress,
            &by(AuthorKind::Agent),
            None
        )
        .is_err()
    );
    store.backend().store_remove(&rel).unwrap();
    assert!(
        set_status(
            &store,
            &id,
            ReviewStatus::InProgress,
            &by(AuthorKind::Agent),
            None
        )
        .unwrap()
    );
    assert_eq!(cache(), None);
    assert_eq!(load(&store, &id).status, ReviewStatus::InProgress);
}

#[test]
fn viewed_files_belong_to_the_round_in_progress() {
    let (_repo, store, id) = with_review();
    assert!(mark_viewed(&store, &id, "src/lib.rs", true).unwrap());
    assert!(!mark_viewed(&store, &id, "src/lib.rs", true).unwrap());
    assert!(matches!(
        rejected(mark_viewed(&store, &id, "../escape", true)),
        Rejection::InvalidPath { .. }
    ));
    assert_eq!(
        close_round(
            &store,
            &id,
            oid('a'),
            oid('b'),
            Some(Verdict::RequestChanges)
        )
        .unwrap(),
        1
    );
    assert!(mark_viewed(&store, &id, "other.rs", true).unwrap());
    assert!(!mark_viewed(&store, &id, "src/lib.rs", false).unwrap());
    let review = load(&store, &id);
    assert!(review.is_viewed(1, "src/lib.rs"));
    assert!(review.is_viewed(2, "other.rs") && !review.is_viewed(2, "src/lib.rs"));
    assert_eq!(review.rounds[0].verdict, Some(Verdict::RequestChanges));
    assert_eq!(
        close_round(&store, &id, oid('b'), oid('c'), None).unwrap(),
        2
    );
    assert_eq!(load(&store, &id).current_round(), 3);
}

#[test]
fn pr_links_and_submissions() {
    let (_repo, store, id) = with_review();
    let pr = PrLink {
        owner: "kylekz".to_string(),
        repo: "difftest".to_string(),
        number: 3,
    };
    assert!(link_pr(&store, &id, pr.clone()).unwrap());
    assert!(!link_pr(&store, &id, pr.clone()).unwrap());
    assert_eq!(load(&store, &id).pr, Some(pr));
    // Linked to a PR, a local review is still a local review.
    assert!(!load(&store, &id).is_pr_review());
    // Only a draft PR review is submitted.
    assert!(matches!(
        rejected(record_submission(&store, &id, 1, "u", Verdict::Approve)),
        Rejection::NotSubmittable { .. }
    ));

    let (_repo, store) = fixture_store();
    let id = open_review(&store, pr_target('b'), reviewer())
        .unwrap()
        .review
        .id;
    let thread = add_thread(&store, &id, anchor("src/lib.rs", 1), "Nit", reviewer()).unwrap();
    let mismatch = PrLink {
        owner: "kylekz".to_string(),
        repo: "difftest".to_string(),
        number: 13,
    };
    assert!(matches!(
        rejected(link_pr(&store, &id, mismatch)),
        Rejection::PrMismatch { .. }
    ));
    record_submission(
        &store,
        &id,
        900,
        "https://x.invalid/900",
        Verdict::RequestChanges,
    )
    .unwrap();
    let review = load(&store, &id);
    assert_eq!(review.status, ReviewStatus::Submitted);
    assert_eq!(review.submissions[0].github_review_id, 900);

    // Read-only from here on (PLAN §2.4).
    assert_eq!(
        rejected(reply(&store, &id, &thread, "late", agent(), None)),
        Rejection::Submitted
    );
    assert_eq!(
        rejected(add_thread(
            &store,
            &id,
            anchor("src/lib.rs", 1),
            "x",
            reviewer()
        )),
        Rejection::Submitted
    );
    assert_eq!(
        rejected(mark_viewed(&store, &id, "src/lib.rs", true)),
        Rejection::Submitted
    );
    assert_eq!(
        rejected(set_status(
            &store,
            &id,
            ReviewStatus::Closed,
            &by(AuthorKind::Reviewer),
            None
        )),
        Rejection::Submitted
    );
    assert_eq!(
        rejected(record_submission(&store, &id, 901, "u", Verdict::Approve)),
        Rejection::Submitted
    );
    assert_eq!(
        rejected(edit_body(
            &store,
            &id,
            &ItemId::Thread(thread.clone()),
            "x",
            &reviewer()
        )),
        Rejection::Submitted
    );
    assert_eq!(
        rejected(close_round(&store, &id, oid('a'), oid('b'), None)),
        Rejection::Submitted
    );
}

#[test]
fn only_an_items_author_edits_or_deletes_it() {
    let (_repo, store, id) = with_review();
    let thread = add_thread(&store, &id, anchor("src/lib.rs", 1), "Typo", reviewer()).unwrap();
    let answer = reply(&store, &id, &thread, "Fixed", agent(), None).unwrap();
    let thread_item = ItemId::Thread(thread.clone());
    let reply_item = ItemId::Comment(answer.clone());

    not_allowed(edit_body(&store, &id, &thread_item, "x", &agent()));
    not_allowed(edit_body(&store, &id, &reply_item, "x", &reviewer()));
    not_allowed(edit_body(
        &store,
        &id,
        &reply_item,
        "x",
        &Author::agent("codex"),
    ));
    not_allowed(delete_item(&store, &id, &reply_item, &reviewer()));
    assert!(matches!(
        edit_body(&store, &id, &reply_item, " ", &agent()),
        Err(OpError::Empty { .. })
    ));

    edit_body(&store, &id, &thread_item, "Typo here", &reviewer()).unwrap();
    // The model an agent writes with is no part of who it is (PLAN §2.5): on another model
    // it still edits what it wrote.
    edit_body(
        &store,
        &id,
        &reply_item,
        "Fixed",
        &agent().with_model("Claude Opus 5.5"),
    )
    .unwrap();
    edit_body(&store, &id, &reply_item, "Fixed in 3f2a", &agent()).unwrap();
    delete_item(&store, &id, &reply_item, &agent()).unwrap();
    assert!(matches!(
        rejected(delete_item(&store, &id, &reply_item, &agent())),
        Rejection::Deleted { .. }
    ));
    let review = load(&store, &id);
    let t = review.thread(&thread).unwrap();
    assert_eq!(t.body, "Typo here");
    assert_eq!(t.replies[0].body, "Fixed in 3f2a");
    assert!(t.replies[0].is_deleted());

    delete_item(&store, &id, &thread_item, &reviewer()).unwrap();
    assert_eq!(load(&store, &id).live_threads().count(), 0);
    assert!(matches!(
        rejected(reply(&store, &id, &thread, "x", agent(), None)),
        Rejection::Deleted { .. }
    ));
    let ghost = ItemId::Comment(CommentId::generate());
    assert!(matches!(
        rejected(delete_item(&store, &id, &ghost, &agent())),
        Rejection::UnknownItem { .. }
    ));
}

#[test]
fn replies_and_state_changes_record_a_relocated_anchor() {
    let (_repo, store, id) = with_review();
    let thread = add_thread(&store, &id, anchor("src/lib.rs", 2), "Rename?", reviewer()).unwrap();
    close_round(
        &store,
        &id,
        oid('a'),
        oid('b'),
        Some(Verdict::RequestChanges),
    )
    .unwrap();
    let moved = |line| Anchor {
        round: 2,
        ..anchor("src/lib.rs", line)
    };
    reply(&store, &id, &thread, "Renamed.", agent(), Some(moved(4))).unwrap();
    let t = load(&store, &id).threads[0].clone();
    assert_eq!(t.anchor, moved(4));
    assert_eq!(t.original_anchor, Some(anchor("src/lib.rs", 2)));
    assert!(
        set_thread_state(
            &store,
            &id,
            &thread,
            ThreadState::Addressed,
            Some("done"),
            agent(),
            Some(moved(6))
        )
        .unwrap()
    );
    let t = load(&store, &id).threads[0].clone();
    assert_eq!(t.anchor, moved(6));
    assert_eq!(
        t.original_anchor,
        Some(anchor("src/lib.rs", 2)),
        "kept once"
    );

    let events = || store.events(&id).unwrap().events.len();
    let before = events();
    // Refused: an anchor the fold would skip, and a move nobody may make.
    let mut reversed = moved(8);
    reversed.start_line = 9;
    assert!(matches!(
        rejected(reply(&store, &id, &thread, "x", agent(), Some(reversed))),
        Rejection::InvalidAnchor { .. }
    ));
    assert!(matches!(
        set_thread_state(
            &store,
            &id,
            &thread,
            ThreadState::Resolved,
            None,
            agent(),
            Some(moved(8))
        ),
        Err(OpError::NotAllowed { .. })
    ));
    let set = |state, author, relocated| {
        set_thread_state(&store, &id, &thread, state, None, author, relocated).unwrap()
    };
    assert!(set(ThreadState::Open, reviewer(), None), "reopened");
    assert!(set(ThreadState::Resolved, reviewer(), None), "resolved");
    // A state the thread already has writes nothing, its anchor included.
    assert!(
        !set_thread_state(
            &store,
            &id,
            &thread,
            ThreadState::Resolved,
            None,
            reviewer(),
            Some(moved(8))
        )
        .unwrap()
    );
    assert_eq!(events(), before + 2, "a reopen and a resolve");
    assert_eq!(load(&store, &id).threads[0].anchor, moved(6));
}

#[test]
fn a_status_change_keeps_its_note_and_author() {
    let (_repo, store, id) = with_review();
    assert!(
        set_status(
            &store,
            &id,
            ReviewStatus::ChangesRequested,
            &reviewer(),
            None
        )
        .unwrap()
    );
    let review = load(&store, &id);
    let change = review.status_change.unwrap();
    assert_eq!(
        (change.status, change.note, change.author),
        (ReviewStatus::ChangesRequested, None, Some(reviewer()))
    );
    // `dv review done --note`: trimmed; a blank note is none.
    assert!(
        set_status(
            &store,
            &id,
            ReviewStatus::Ready,
            &agent(),
            Some("  Renamed both.\n\nAnd the test.\n")
        )
        .unwrap()
    );
    let change = load(&store, &id).status_change.unwrap();
    assert_eq!(change.status, ReviewStatus::Ready);
    assert_eq!(
        change.note.as_deref(),
        Some("Renamed both.\n\nAnd the test.")
    );
    assert_eq!(change.author, Some(agent()));
    // Done again: the same note is no change, another one replaces it, none leaves it.
    assert!(
        !set_status(
            &store,
            &id,
            ReviewStatus::Ready,
            &agent(),
            Some("Renamed both.\n\nAnd the test.")
        )
        .unwrap()
    );
    assert!(
        set_status(
            &store,
            &id,
            ReviewStatus::Ready,
            &agent(),
            Some("Also the docs.")
        )
        .unwrap()
    );
    assert!(!set_status(&store, &id, ReviewStatus::Ready, &agent(), Some("  ")).unwrap());
    assert_eq!(
        load(&store, &id).status_change.unwrap().note.as_deref(),
        Some("Also the docs.")
    );
    // The role rules follow the author's kind.
    assert_eq!(
        not_allowed(set_status(
            &store,
            &id,
            ReviewStatus::Ready,
            &reviewer(),
            Some("x")
        )),
        "the reviewer can't set a review's status to ready"
    );
}

/// The reviews of a repo with a linked worktree on `feat`, made from either worktree.
#[test]
fn the_reviews_in_context_are_this_worktrees_work_in_progress() {
    let (repo, main) = fixture_store();
    let tree = repo.add_worktree("wt", "feat");
    let linked = ReviewStore::new(Arc::new(open_at(&repo, &tree)));
    let branch = |name: &str| Target::Branch {
        branch: name.to_string(),
        base: None,
    };
    let make = |store: &ReviewStore, target: Target| store.create(target, reviewer()).unwrap().id;
    let main_working = make(&main, Target::Working);
    let main_staged = make(&main, Target::Staged);
    let linked_working = make(&linked, Target::Working);
    let feat = make(&main, branch("feat"));
    let feat_spelled = make(&linked, branch("refs/heads/feat"));
    let main_branch = make(&linked, branch("heads/main"));
    make(
        &main,
        Target::Commit {
            rev: "HEAD".to_string(),
        },
    );
    let closed = make(&main, Target::Working);
    set_status(&main, &closed, ReviewStatus::Closed, &reviewer(), None).unwrap();

    let ids = |store: &ReviewStore, git: &GitRepo| -> Vec<ReviewId> {
        let mut ids: Vec<ReviewId> = reviews_in_context(store, git)
            .unwrap()
            .into_iter()
            .map(|summary| summary.id)
            .collect();
        ids.sort();
        ids
    };
    let sorted = |mut ids: Vec<ReviewId>| {
        ids.sort();
        ids
    };
    let main_git = GitRepo::new(main.backend().clone());
    let linked_git = GitRepo::new(linked.backend().clone());
    assert_eq!(
        ids(&main, &main_git),
        sorted(vec![main_working.clone(), main_staged.clone(), main_branch])
    );
    assert_eq!(
        ids(&linked, &linked_git),
        sorted(vec![linked_working.clone(), feat, feat_spelled])
    );

    // Detached, only the worktree's own working and staged reviews are its context.
    repo.git(&["checkout", "-q", "--detach"]);
    assert_eq!(
        ids(&main, &main_git),
        sorted(vec![main_working.clone(), main_staged])
    );
    // The rule itself, over summaries: a review is in the context of its home only.
    let summary = main.load(&linked_working).unwrap().review.summary();
    assert_eq!(in_context([&summary], &linked_git).unwrap().len(), 1);
    assert!(in_context([&summary], &main_git).unwrap().is_empty());
}

fn finished(result: Result<impl std::fmt::Debug, OpError>) -> ReviewStatus {
    match result {
        Err(OpError::Finished { status }) => status,
        other => panic!("expected Finished, got {other:?}"),
    }
}

#[test]
fn a_finished_local_review_takes_no_more_thread_writes() {
    for status in [ReviewStatus::Approved, ReviewStatus::Closed] {
        let (_repo, store, id) = with_review();
        let thread = add_thread(&store, &id, anchor("src/lib.rs", 1), "Typo", reviewer()).unwrap();
        let answer = reply(&store, &id, &thread, "Fixed", agent(), None).unwrap();
        set_status(&store, &id, status, &reviewer(), None).unwrap();
        let events = || store.events(&id).unwrap().events.len();
        let before = events();

        // Read-only, as the GUI shows it (PLAN §2.4, DESIGN §4.4): for the reviewer and agents.
        for author in [reviewer(), agent()] {
            assert_eq!(
                finished(add_thread(
                    &store,
                    &id,
                    anchor("src/lib.rs", 2),
                    "Late",
                    author.clone()
                )),
                status
            );
            assert_eq!(
                finished(reply(&store, &id, &thread, "late", author.clone(), None)),
                status
            );
            assert_eq!(
                finished(set_thread_state(
                    &store,
                    &id,
                    &thread,
                    ThreadState::Addressed,
                    Some("done"),
                    author,
                    None
                )),
                status
            );
        }
        for state in [ThreadState::Resolved, ThreadState::Open] {
            assert_eq!(
                finished(set_thread_state(
                    &store,
                    &id,
                    &thread,
                    state,
                    None,
                    reviewer(),
                    None
                )),
                status
            );
        }
        assert_eq!(
            finished(edit_body(
                &store,
                &id,
                &ItemId::Comment(answer.clone()),
                "x",
                &agent()
            )),
            status
        );
        assert_eq!(
            finished(delete_item(
                &store,
                &id,
                &ItemId::Thread(thread.clone()),
                &reviewer()
            )),
            status
        );
        assert_eq!(
            finished(mark_viewed(&store, &id, "src/lib.rs", true)),
            status
        );
        assert_eq!(events(), before, "nothing was written");
        // An agent is still told that resolving is the reviewer's, whatever the review's state.
        not_allowed(set_thread_state(
            &store,
            &id,
            &thread,
            ThreadState::Resolved,
            None,
            agent(),
            None,
        ));
    }
}

#[test]
fn an_agent_starts_work_only_on_a_review_whose_changes_are_still_requested() {
    let (_repo, store, id) = with_review();
    // Nothing to start on an open review, nor for the reviewer.
    assert!(!start_work(&store, &id, &agent()).unwrap());
    set_status(
        &store,
        &id,
        ReviewStatus::ChangesRequested,
        &reviewer(),
        None,
    )
    .unwrap();
    assert!(!start_work(&store, &id, &reviewer()).unwrap());
    assert_eq!(load(&store, &id).status, ReviewStatus::ChangesRequested);

    assert!(start_work(&store, &id, &agent()).unwrap());
    let review = load(&store, &id);
    assert_eq!(review.status, ReviewStatus::InProgress);
    assert_eq!(
        review.status_change.map(|change| change.author),
        Some(Some(agent()))
    );
    assert!(!start_work(&store, &id, &agent()).unwrap(), "only once");

    // The agent's handoff landed between its write and this follow-up: the handoff wins,
    // however the review stood when the write loaded it.
    set_status(
        &store,
        &id,
        ReviewStatus::ChangesRequested,
        &reviewer(),
        None,
    )
    .unwrap();
    set_status(&store, &id, ReviewStatus::Ready, &agent(), Some("Done.")).unwrap();
    assert!(!start_work(&store, &id, &agent()).unwrap());
    let review = load(&store, &id);
    assert_eq!(review.status, ReviewStatus::Ready);
    assert_eq!(
        review
            .status_change
            .and_then(|change| change.note)
            .as_deref(),
        Some("Done.")
    );
}

#[test]
fn a_handoff_landing_between_start_works_load_and_its_append_still_wins() {
    let (_repo, store, id) = with_review();
    set_status(
        &store,
        &id,
        ReviewStatus::ChangesRequested,
        &reviewer(),
        None,
    )
    .unwrap();
    // `start_work` reads the review while changes are still requested...
    let read = store.load_for_update(&id).unwrap().review;
    // ...the agent's `dv review done`, run beside it, lands first...
    set_status(&store, &id, ReviewStatus::Ready, &agent(), Some("Done.")).unwrap();
    // ...and only then does the follow-up append, on what it read.
    assert!(crate::review::ops::start_work_on(&store, &id, read, &agent()).unwrap());
    let review = load(&store, &id);
    assert_eq!(review.status, ReviewStatus::Ready, "the handoff wins");
    assert_eq!(
        review
            .status_change
            .and_then(|change| change.note)
            .as_deref(),
        Some("Done.")
    );
    let loaded = store.load(&id).unwrap();
    assert!(
        loaded.warnings.is_empty(),
        "a condition that no longer holds is a no-op, not a bad event: {:?}",
        loaded.warnings
    );
}
