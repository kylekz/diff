//! What dv keeps for a pull request goes with its last review (PLAN §7).

use super::*;
use crate::review::{ListEntry, Verdict, ops};
use crate::target::Target;

/// The names of the clone's refs under `refs/dv/`.
fn dv_refs(fixture: &PrFixture) -> Vec<String> {
    fixture
        .refs("refs/dv/")
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

#[test]
fn deleting_a_pull_requests_last_review_removes_its_refs_and_mirrors() {
    let mut fixture = PrFixture::new();
    let store = fixture.store();
    let first = fixture.open().review;
    ops::record_submission(
        &store,
        &first.id,
        9001,
        "https://example.invalid/r",
        Verdict::Comment,
    )
    .unwrap();
    fixture.push("pub fn answer() -> u32 {\n    43\n}\n");
    let second = fixture.open().review;
    // Another login's mirror of the same pull request, and a file that is nobody's mirror.
    let mine = match fixture.mirrors().read(&fixture.key()).unwrap() {
        MirrorRead::Found(mirror) => *mirror,
        other => panic!("{other:?}"),
    };
    let theirs = MirrorKey::new(&fixture.repo(), NUMBER, "octocat");
    fixture
        .mirrors()
        .write(&Mirror {
            key: theirs.clone(),
            ..mine
        })
        .unwrap();
    let pr_dir = fixture
        .github
        .checkout()
        .path()
        .join(".git/dv/github/github.com/dv-fixture/api/pr-7");
    std::fs::write(pr_dir.join("notes copy.txt"), "mine").unwrap();
    // A local review keeps nothing of a pull request's.
    let local = store.create(Target::Working, reviewer()).unwrap();

    let kept = [
        "refs/dv/pr/dv-fixture/api/7/base".to_string(),
        "refs/dv/pr/dv-fixture/api/7/head".to_string(),
    ];
    let pins_of = |id: &crate::review::ReviewId| {
        [
            format!("refs/dv/reviews/{id}/pr-base"),
            format!("refs/dv/reviews/{id}/pr-head"),
        ]
    };
    let mut expected: Vec<String> = kept.to_vec();
    expected.extend(pins_of(&first.id));
    expected.extend(pins_of(&second.id));
    expected.sort();
    assert_eq!(dv_refs(&fixture), expected);

    // One of two reviews goes: its pins with it, the pull request's own refs and mirrors stay.
    assert!(store.delete(&first.id).unwrap());
    let mut expected: Vec<String> = kept.to_vec();
    expected.extend(pins_of(&second.id));
    assert_eq!(dv_refs(&fixture), expected);
    assert!(matches!(
        fixture.mirrors().read(&fixture.key()).unwrap(),
        MirrorRead::Found(_)
    ));
    assert!(store.delete(&local.id).unwrap());
    assert_eq!(dv_refs(&fixture), expected);

    // The last one goes: nothing of the pull request is left but the stray file.
    assert!(store.delete(&second.id).unwrap());
    assert_eq!(dv_refs(&fixture), Vec::<String>::new());
    for key in [fixture.key(), theirs] {
        assert_eq!(
            fixture.mirrors().read(&key).unwrap(),
            MirrorRead::Refetch(Refetch::Missing)
        );
    }
    let left: Vec<_> = std::fs::read_dir(&pr_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left, ["notes copy.txt"]);
    // The user's own refs were never touched.
    assert_eq!(
        fixture
            .refs("refs/remotes/")
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>(),
        ["refs/remotes/origin/HEAD", "refs/remotes/origin/main"]
    );
}

#[test]
fn a_delete_that_was_interrupted_is_finished_by_the_next() {
    let fixture = PrFixture::new();
    let store = fixture.store();
    let review = fixture.open().review;
    // The log went, and the delete stopped there.
    std::fs::remove_file(
        fixture
            .github
            .checkout()
            .path()
            .join(format!(".git/dv/reviews/{}.jsonl", review.id)),
    )
    .unwrap();
    assert_eq!(dv_refs(&fixture).len(), 4);
    // Deleting again says there was no review, and finishes.
    assert!(!store.delete(&review.id).unwrap());
    assert_eq!(dv_refs(&fixture), Vec::<String>::new());
    assert_eq!(
        fixture.mirrors().read(&fixture.key()).unwrap(),
        MirrorRead::Refetch(Refetch::Missing)
    );
}

#[test]
fn one_pull_requests_last_review_leaves_another_pull_requests_alone() {
    let fixture = PrFixture::new();
    let review = fixture.open().review;
    // Pull request 8, the same commit, read by someone.
    let head = fixture.github.open_pr(8, "feat");
    let mut other = GhPr::new(8, &head);
    other.files = fixture.pr.files.clone();
    let mut calls = vec![fixture_gh::viewer(VIEWER)];
    calls.extend(other.calls("api"));
    fixture.mock.answer(Value::Array(calls));
    let eighth = fixture.open_ref(&PrRef::Hash(8)).unwrap().review;

    fixture.store().delete(&review.id).unwrap();
    assert_eq!(
        dv_refs(&fixture),
        [
            "refs/dv/pr/dv-fixture/api/8/base".to_string(),
            "refs/dv/pr/dv-fixture/api/8/head".to_string(),
            format!("refs/dv/reviews/{}/pr-base", eighth.id),
            format!("refs/dv/reviews/{}/pr-head", eighth.id),
        ]
    );
    assert!(matches!(
        fixture
            .mirrors()
            .read(&MirrorKey::new(&fixture.repo(), 8, VIEWER))
            .unwrap(),
        MirrorRead::Found(_)
    ));
    assert_eq!(
        fixture.mirrors().read(&fixture.key()).unwrap(),
        MirrorRead::Refetch(Refetch::Missing)
    );
}

#[test]
fn nothing_is_swept_while_a_log_doesnt_load() {
    // The log that doesn't load may be the review a pull request's refs are kept for.
    let fixture = PrFixture::new();
    let store = fixture.store();
    let review = fixture.open().review;
    let local = store.create(Target::Working, reviewer()).unwrap();
    let reviews = fixture.github.checkout().path().join(".git/dv/reviews");
    let broken = reviews.join("r-01k6zzzzzzzzzzzzzzzzzzzzzz.jsonl");
    std::fs::write(&broken, "not a review\n").unwrap();
    assert!(
        store
            .list()
            .unwrap()
            .iter()
            .any(|entry| matches!(entry, ListEntry::Err { .. }))
    );

    assert!(store.delete(&review.id).unwrap());
    let kept = [
        "refs/dv/pr/dv-fixture/api/7/base".to_string(),
        "refs/dv/pr/dv-fixture/api/7/head".to_string(),
    ];
    assert_eq!(dv_refs(&fixture), kept, "the review's own pins went");
    assert!(matches!(
        fixture.mirrors().read(&fixture.key()).unwrap(),
        MirrorRead::Found(_)
    ));

    // Once every log loads again, the next deletion sweeps what nobody reviews.
    std::fs::remove_file(&broken).unwrap();
    assert!(store.delete(&local.id).unwrap());
    assert_eq!(dv_refs(&fixture), Vec::<String>::new());
    assert_eq!(
        fixture.mirrors().read(&fixture.key()).unwrap(),
        MirrorRead::Refetch(Refetch::Missing)
    );
}

#[test]
fn a_sweep_that_fails_leaves_the_review_deleted() {
    let fixture = PrFixture::new();
    let store = fixture.store();
    let review = fixture.open().review;
    let local = store.create(Target::Working, reviewer()).unwrap();
    // Another git holds the pull request's head ref: the sweep can't delete it.
    let lock = fixture
        .github
        .checkout()
        .path()
        .join(".git/refs/dv/pr/dv-fixture/api/7/head.lock");
    std::fs::write(&lock, "").unwrap();

    assert!(
        store.delete(&review.id).unwrap(),
        "the delete is done all the same"
    );
    assert!(
        store
            .list()
            .unwrap()
            .iter()
            .all(|entry| *entry.id() != review.id)
    );
    assert_eq!(
        dv_refs(&fixture),
        [
            "refs/dv/pr/dv-fixture/api/7/base".to_string(),
            "refs/dv/pr/dv-fixture/api/7/head".to_string(),
        ]
    );
    // Deleting it again says it is gone, and still doesn't fail.
    assert!(!store.delete(&review.id).unwrap());

    // The next deletion, the ref free again, finishes the sweep.
    std::fs::remove_file(&lock).unwrap();
    assert!(store.delete(&local.id).unwrap());
    assert_eq!(dv_refs(&fixture), Vec::<String>::new());
}
