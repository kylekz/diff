//! Opening a pull request (PLAN §9.3): the fetch, the verification, which review it is.

use serde_json::json;

use super::*;
use crate::review::{EventBody, ReviewStatus, Verdict, ops};
use crate::target::{DiffSpec, Side, Target};

#[test]
fn a_first_open_fetches_the_pull_request_and_makes_its_draft() {
    let fixture = PrFixture::new();
    let clone = fixture.github.checkout();
    let tracking = clone.git(&["rev-parse", "origin/main"]);
    let fetch_head = std::fs::read(clone.path().join(".git/FETCH_HEAD")).unwrap();
    // `main` moves on at GitHub, which the pull request's fetch must not tell `origin/main`.
    let author = fixture.github.author();
    author.checkout("main").write("NOTES.md", "later\n");
    author.commit("Notes");
    fixture.github.publish("main");
    // And a tag on the pull request's head, which a plain fetch would follow.
    author.git(&["tag", "v1", "feat"]);
    author.git(&[
        "push".as_ref(),
        "-q".as_ref(),
        fixture.github.bare().as_os_str(),
        "refs/tags/v1".as_ref(),
    ]);

    let opened = fixture.open();
    assert!(opened.created);
    let review = &opened.review;
    assert_eq!(review.status, ReviewStatus::Draft);
    assert_eq!(review.author, reviewer());
    let (head, base) = (fixture.head(), fixture.main());
    let merge_base = oid(tracking.trim());
    assert_eq!(
        review.target,
        Target::Pr {
            owner: GITHUB_OWNER.to_string(),
            repo: "api".to_string(),
            number: NUMBER,
            base_oid: base.clone(),
            head_oid: head.clone(),
        }
    );

    // The commits are in dv's own namespace, and nothing else of the clone moved.
    assert_eq!(
        fixture.refs("refs/dv/pr/"),
        [
            (
                "refs/dv/pr/dv-fixture/api/7/base".to_string(),
                base.to_string()
            ),
            (
                "refs/dv/pr/dv-fixture/api/7/head".to_string(),
                head.to_string()
            ),
        ]
    );
    assert_eq!(clone.git(&["rev-parse", "origin/main"]), tracking);
    assert_eq!(
        std::fs::read(clone.path().join(".git/FETCH_HEAD")).unwrap(),
        fetch_head
    );
    assert_eq!(clone.git(&["status", "--porcelain"]), "");
    assert_eq!(clone.git(&["tag", "--list"]), "");
    assert_eq!(
        fixture
            .refs("refs/")
            .into_iter()
            .map(|(name, _)| name)
            .filter(|name| !name.starts_with("refs/dv/"))
            .collect::<Vec<_>>(),
        [
            "refs/heads/main",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main"
        ]
    );

    // The draft shows the head against its merge base with `main`, pinned.
    let pins = Pins {
        head: head.clone(),
        base: merge_base.clone(),
    };
    assert_eq!(fixture.pins(review), pins);
    assert_eq!(
        pins::resolve(&fixture.git(), &review.id, &review.target).unwrap(),
        DiffSpec {
            old: Side::Commit(merge_base),
            new: Side::Commit(head.clone()),
        }
    );
    assert_eq!(opened.refreshed.pins, Some(pins));
    assert_eq!(opened.refreshed.moved_from, None);

    // Its log says which pull request it is.
    let events = fixture.store().events(&review.id).unwrap().events;
    assert!(
        matches!(
            &events[1].body,
            EventBody::PrLinked { owner, repo, number }
                if owner == GITHUB_OWNER && repo == "api" && *number == NUMBER
        ),
        "{events:?}"
    );

    // The mirror was read at that head.
    assert!(opened.refreshed.stale.is_none(), "{:?}", opened.refreshed);
    let mirror = opened.refreshed.mirror.unwrap();
    assert_eq!(mirror.key, fixture.key());
    assert_eq!(mirror.pr.head_oid, head);
    assert_eq!(mirror.pr.title, "Add the answer");
    assert_eq!(mirror.pr.state, PrState::Open);
    assert_eq!(mirror.pr.head_repo, HeadRepo::Same);
    assert_eq!(mirror.files.len(), 1);
    assert_eq!(
        fixture.mirrors().read(&fixture.key()).unwrap(),
        MirrorRead::Found(mirror)
    );

    // gh was asked for the pull request once, by its repository's name.
    let asked = fixture.asked();
    assert_eq!(
        asked[0],
        format!(
            "pr view 7 --json {} --repo=github.com/dv-fixture/api",
            crate::github::pr::VIEW_FIELDS
        )
    );
    assert_eq!(
        asked
            .iter()
            .filter(|call| call.starts_with("pr view 7 --json id,"))
            .count(),
        1,
        "{asked:?}"
    );
}

#[test]
fn opening_again_resumes_the_draft() {
    let fixture = PrFixture::new();
    let first = fixture.open();
    for reference in [
        PrRef::Number(NUMBER),
        PrRef::parse("dv-fixture/API#7").unwrap(),
        PrRef::parse("https://github.com/dv-fixture/api/pull/7/files").unwrap(),
    ] {
        let again = fixture.open_ref(&reference).unwrap();
        assert!(!again.created, "{reference:?}");
        assert_eq!(again.review.id, first.review.id);
    }
    assert_eq!(fixture.store().list().unwrap().len(), 1);
}

#[test]
fn a_resumed_draft_follows_a_moved_head() {
    let mut fixture = PrFixture::new();
    let first = fixture.open();
    let before = fixture.pins(&first.review);
    let head = fixture.push("pub fn answer() -> u32 {\n    43\n}\n");

    let again = fixture.open();
    assert!(!again.created);
    assert_eq!(again.review.id, first.review.id);
    // The target keeps the commits it was made at; the pins moved.
    assert_eq!(again.review.target, first.review.target);
    let pins = Pins {
        head: head.clone(),
        base: before.base.clone(),
    };
    assert_eq!(fixture.pins(&again.review), pins);
    assert_eq!(again.refreshed.pins, Some(pins));
    assert_eq!(again.refreshed.moved_from, Some(before));
    assert_eq!(again.refreshed.mirror.unwrap().pr.head_oid, head);
}

#[test]
fn a_review_submitted_at_the_head_is_returned_and_a_new_head_starts_a_draft() {
    let mut fixture = PrFixture::new();
    let first = fixture.open();
    ops::record_submission(
        &fixture.store(),
        &first.review.id,
        9001,
        "https://github.com/dv-fixture/api/pull/7#pullrequestreview-9001",
        Verdict::Comment,
    )
    .unwrap();

    // Nothing new to review: the submitted review, as it was submitted.
    let again = fixture.open();
    assert!(!again.created);
    assert_eq!(again.review.id, first.review.id);
    assert_eq!(again.review.status, ReviewStatus::Submitted);
    let frozen = fixture.pins(&first.review);

    // New commits: the next draft, at the new head; the submitted review stays where it was.
    let head = fixture.push("pub fn answer() -> u32 {\n    43\n}\n");
    let next = fixture.open();
    assert!(next.created);
    assert_ne!(next.review.id, first.review.id);
    assert_eq!(next.review.status, ReviewStatus::Draft);
    assert_eq!(fixture.pins(&next.review).head, head);
    assert_eq!(fixture.pins(&first.review), frozen);
    let submitted = fixture.store().load(&first.review.id).unwrap().review;
    assert_eq!(fixture.refresh(&submitted).pins, Some(frozen.clone()));
    assert_eq!(fixture.pins(&first.review), frozen);
    assert_eq!(
        pins::resolve(&fixture.git(), &submitted.id, &submitted.target).unwrap(),
        frozen.spec()
    );

    // A closed draft is no draft to resume either.
    ops::set_status(
        &fixture.store(),
        &next.review.id,
        ReviewStatus::Closed,
        &reviewer(),
        None,
    )
    .unwrap();
    let third = fixture.open();
    assert!(third.created);
}

#[test]
fn a_push_between_gh_and_the_fetch_reads_both_again_once() {
    let mut fixture = PrFixture::new();
    let old = fixture.head();
    let head = fixture.push("pub fn answer() -> u32 {\n    43\n}\n");
    // gh still reports the old head the first time it is asked.
    let mut stale = fixture.pr.clone();
    stale.head = old.to_string();
    fixture.answer(vec![json!({
        "argv": ["pr", "view", "7", "--json", crate::github::pr::VIEW_FIELDS, "**"],
        "stdout": stale.view("api"),
        "times": 1
    })]);
    let opened = fixture.open();
    assert_eq!(fixture.pins(&opened.review).head, head);
    assert_eq!(opened.refreshed.mirror.unwrap().pr.head_oid, head);
    let views = fixture
        .asked()
        .iter()
        .filter(|call| call.starts_with("pr view 7 --json id,"))
        .count();
    assert_eq!(views, 2);
}

#[test]
fn a_head_that_keeps_moving_is_moved() {
    let mut fixture = PrFixture::new();
    let old = fixture.head();
    fixture.push("pub fn answer() -> u32 {\n    43\n}\n");
    // gh never catches up with what git fetches.
    fixture.pr.head = old.to_string();
    fixture.answer(Vec::new());
    let error = fixture.open_ref(&PrRef::Hash(NUMBER)).unwrap_err();
    assert!(
        matches!(error, GithubError::Moved { number: NUMBER }),
        "{error:?}"
    );
    assert_eq!(error.kind(), "moved");
    assert_eq!(
        error.to_string(),
        "#7 moved while dv was reading it (someone is pushing to it); refresh again"
    );
    // No review was made of a pull request that couldn't be read.
    assert!(fixture.store().list().unwrap().is_empty());
}

#[test]
fn a_forks_pull_request_is_fetched_from_the_base_repository() {
    // The head's branch is never published, as a fork's isn't: only `refs/pull/7/head`.
    let mut fixture = PrFixture::new();
    fixture.pr.fork = Some(Some("contributor".to_string()));
    fixture.answer(Vec::new());
    let opened = fixture.open();
    assert_eq!(fixture.pins(&opened.review).head, fixture.head());
    assert_eq!(
        opened.refreshed.mirror.unwrap().pr.head_repo,
        HeadRepo::Fork {
            owner: Some("contributor".to_string())
        }
    );

    // A fork that is gone has no owner; a deleted author is `ghost`.
    fixture.pr.fork = Some(None);
    fixture.pr.author = None;
    fixture.answer(Vec::new());
    let mirror = fixture.open().refreshed.mirror.unwrap();
    assert_eq!(mirror.pr.head_repo, HeadRepo::Fork { owner: None });
    assert_eq!(mirror.pr.author, "ghost");
}

#[test]
fn another_repositorys_pull_request_is_refused_before_gh_is_asked() {
    let fixture = PrFixture::new();
    for (reference, repository) in [
        ("acme/other#7", "acme/other"),
        ("https://github.com/dv-fixture/web/pull/7", "dv-fixture/web"),
        (
            "https://ghe.example.com/dv-fixture/api/pull/7",
            "ghe.example.com/dv-fixture/api",
        ),
    ] {
        let error = fixture
            .open_ref(&PrRef::parse(reference).unwrap())
            .unwrap_err();
        assert!(
            matches!(
                &error,
                GithubError::OtherRepository { number: 7, repository: named } if named == repository
            ),
            "{reference}: {error:?}"
        );
    }
    assert!(fixture.asked().is_empty());
    assert!(fixture.store().list().unwrap().is_empty());
}

#[test]
fn a_repository_named_dot_github_opens() {
    // git refuses `.github` as a ref's component, and the store as a file's name.
    let fixture = PrFixture::named(".github");
    let opened = fixture.open();
    assert_eq!(
        fixture
            .refs("refs/dv/pr/")
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>(),
        [
            "refs/dv/pr/dv-fixture/%2egithub/7/base",
            "refs/dv/pr/dv-fixture/%2egithub/7/head"
        ]
    );
    assert_eq!(
        fixture.key().path(),
        "github/github.com/dv-fixture/%2egithub/pr-7/kylekz.json"
    );
    assert!(matches!(
        fixture.mirrors().read(&fixture.key()).unwrap(),
        MirrorRead::Found(_)
    ));
    assert!(matches!(
        &opened.review.target,
        Target::Pr { repo, .. } if repo == ".github"
    ));
    // And it goes again with its review.
    fixture.store().delete(&opened.review.id).unwrap();
    assert_eq!(fixture.refs("refs/dv/"), []);
    assert_eq!(
        fixture.mirrors().read(&fixture.key()).unwrap(),
        MirrorRead::Refetch(Refetch::Missing)
    );
}

#[test]
fn gh_failing_fails_the_open_with_its_kind() {
    let fixture = PrFixture::new();
    fixture.answer(vec![fixture_gh::signed_out()]);
    let error = fixture.open_ref(&PrRef::Hash(NUMBER)).unwrap_err();
    assert!(
        matches!(error, GithubError::NotAuthenticated { .. }),
        "{error:?}"
    );
    assert_eq!(error.fix().as_deref(), Some("gh auth login"));

    fixture.answer(vec![json!({
        "argv": ["pr", "view", "8", "**"],
        "stderr": "GraphQL: Could not resolve to a PullRequest with the number of 8. (repository.pullRequest)\n",
        "exit": 1
    })]);
    let error = fixture.open_ref(&PrRef::Hash(8)).unwrap_err();
    assert!(matches!(error, GithubError::NotFound { .. }), "{error:?}");
    assert!(fixture.store().list().unwrap().is_empty());
    assert_eq!(fixture.refs("refs/dv/"), []);
}

#[test]
fn a_mirror_that_cant_be_read_leaves_the_review_open_and_stale() {
    let fixture = PrFixture::new();
    fixture.answer(vec![json!({
        "argv": ["api", "graphql", "**"],
        "stdin_contains": "reviewThreads(first",
        "stderr": "gh: Server Error (HTTP 502)\n",
        "exit": 1
    })]);
    let opened = fixture.open();
    assert!(opened.created);
    assert!(
        matches!(
            opened.refreshed.stale,
            Some(GithubError::Unavailable { .. })
        ),
        "{:?}",
        opened.refreshed.stale
    );
    assert_eq!(opened.refreshed.mirror, None);
    assert_eq!(fixture.pins(&opened.review).head, fixture.head());
}

#[test]
fn a_review_that_is_here_opens_as_it_stands_when_github_cant_be_asked() {
    let mut fixture = PrFixture::new();
    let first = fixture.open();
    let mirror = *first.refreshed.mirror.unwrap();
    let pinned = fixture.pins(&first.review);

    // Signed out (or offline): the draft opens, nothing moved, its mirror stale.
    fixture.push("pub fn answer() -> u32 {\n    43\n}\n");
    fixture.answer(vec![fixture_gh::signed_out()]);
    let offline = fixture.open();
    assert!(!offline.created);
    assert_eq!(offline.review.id, first.review.id);
    let stale = offline.refreshed.stale.unwrap();
    assert!(
        matches!(stale, GithubError::NotAuthenticated { .. }),
        "{stale:?}"
    );
    assert_eq!(offline.refreshed.mirror.as_deref(), Some(&mirror));
    assert_eq!(offline.refreshed.pins, Some(pinned.clone()));
    assert_eq!(offline.refreshed.moved_from, None);
    assert_eq!(fixture.pins(&first.review), pinned);

    // The pull request gone from GitHub (or out of this login's sight) is the same.
    fixture.answer(vec![json!({
        "argv": ["pr", "view", "7", "**"],
        "stderr": "GraphQL: Could not resolve to a PullRequest with the number of 7. (repository.pullRequest)\n",
        "exit": 1
    })]);
    let gone = fixture.open();
    assert_eq!(gone.review.id, first.review.id);
    assert!(matches!(
        gone.refreshed.stale,
        Some(GithubError::NotFound { .. })
    ));

    // With no draft, its latest submitted review, whatever head that was submitted at.
    ops::record_submission(
        &fixture.store(),
        &first.review.id,
        9001,
        "https://example.invalid/r",
        Verdict::Comment,
    )
    .unwrap();
    fixture.answer(vec![fixture_gh::signed_out()]);
    let submitted = fixture.open();
    assert!(!submitted.created);
    assert_eq!(submitted.review.id, first.review.id);
    assert_eq!(submitted.review.status, ReviewStatus::Submitted);
    assert!(submitted.refreshed.stale.is_some());

    // A closed review is none to open, and nothing is made without GitHub's word.
    let other = PrFixture::new();
    let draft = other.open().review;
    ops::set_status(
        &other.store(),
        &draft.id,
        ReviewStatus::Closed,
        &reviewer(),
        None,
    )
    .unwrap();
    other.answer(vec![fixture_gh::signed_out()]);
    let error = other.open_ref(&PrRef::Hash(NUMBER)).unwrap_err();
    assert!(
        matches!(error, GithubError::NotAuthenticated { .. }),
        "{error:?}"
    );
    assert_eq!(other.store().list().unwrap().len(), 1);
}
