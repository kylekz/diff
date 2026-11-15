//! What the PR strip says in each state a pull request and its mirror can be in (PLAN
//! §9.4, §9.5, DESIGN §4.5): every way the pull request moved, and a mirror that isn't
//! there, can't be read, or is being read.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dv_core::git::Oid;
use dv_core::github::Checks;
use pretty_assertions::assert_eq;

use super::read::{idle, open_review, reviewed_elsewhere, strip};
use super::*;
use crate::services::github::PrRead;
use crate::session::{Commits, MoveKind, PrMove};
use crate::shell::pr_strip::{PrStrip, ReadLine};

fn oid(digit: char) -> Oid {
    Oid::parse(&digit.to_string().repeat(40)).unwrap()
}

/// DESIGN §4.5: the strip's words for each way a pull request moved, on a draft and on a
/// review that no longer follows it, and for each state of its read.
#[test]
fn the_strip_words_every_move_and_every_read() {
    let words = |kind: MoveKind, commits: Option<Commits>, draft: bool| {
        PrStrip {
            number: 7,
            title: None,
            author: None,
            branches: None,
            state: None,
            checks: Checks::default(),
            round: None,
            moved: Some(PrMove {
                kind,
                from: oid('a'),
                to: oid('b'),
                commits,
            }),
            draft,
            read: ReadLine::NotRead,
            refreshing: false,
            url: None,
            checks_url: None,
        }
        .moved_text()
        .unwrap()
    };
    use Commits::{Earlier, New, Rewritten};
    use MoveKind::{Ahead, Followed};
    assert_eq!(
        [
            words(Ahead, Some(New(1)), true),
            words(Ahead, Some(New(3)), true),
            words(Ahead, Some(Rewritten), true),
            words(Ahead, Some(Earlier), true),
            words(Ahead, None, true),
        ],
        [
            "1 new commit since this draft's head",
            "3 new commits since this draft's head",
            "Force-pushed since this draft's head",
            "GitHub has an earlier commit than this draft's head",
            "The pull request moved since this draft's head",
        ]
    );
    assert_eq!(
        [
            words(Ahead, Some(New(2)), false),
            words(Ahead, Some(Rewritten), false),
            words(Ahead, Some(Earlier), false),
            words(Ahead, None, false),
        ],
        [
            "2 new commits since this review's head",
            "Force-pushed since this review's head",
            "GitHub has an earlier commit than this review's head",
            "The pull request moved since this review's head",
        ]
    );
    // What a refresh did reads the same on any review.
    for draft in [true, false] {
        assert_eq!(
            [
                words(Followed, Some(New(1)), draft),
                words(Followed, Some(New(4)), draft),
                words(Followed, Some(Rewritten), draft),
                words(Followed, Some(Earlier), draft),
                words(Followed, None, draft),
            ],
            [
                "Updated to 1 new commit",
                "Updated to 4 new commits",
                "Updated to a force-pushed head",
                "Updated to an earlier commit",
                "Updated to a new head",
            ]
        );
    }

    assert_eq!(ReadLine::NotRead.text(0), "Not read from GitHub yet");
    assert_eq!(ReadLine::Reading.text(0), "Reading from GitHub…");
    assert_eq!(ReadLine::Read(0).text(120_000), "Read 2m ago");
    assert_eq!(
        ReadLine::Stale(Failure::new("gh failed")).text(0),
        "Couldn't refresh · details"
    );
}

/// What the strip says the pull request did, as the session counts it.
fn moved(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> (MoveKind, Option<Commits>, String) {
    idle(shell, cx, "the strip's facts");
    let shown = strip(shell, cx);
    let moved = shown.moved.clone().expect("the pull request moved");
    (moved.kind, moved.commits, shown.moved_text().unwrap())
}

/// PLAN §9.5: each way a pull request's head and the review's can differ, counted from the
/// clone's history: new commits, a force-push, an earlier commit, and a commit the clone
/// doesn't hold.
#[gpui::test]
fn the_strip_tells_a_force_push_an_earlier_commit_and_a_commit_it_cant_count(
    cx: &mut TestAppContext,
) {
    let (_dir, shell, cx) = open_shell(cx);
    let mut fixture = PrFixture::new(cx);
    fixture.open(&shell, cx).unwrap();
    idle(&shell, cx, "the pull request");
    let session = shell.read_with(cx, |shell, _| shell.session().cloned().unwrap());
    let first = Oid::parse(&fixture.pr.head).unwrap();
    // What GitHub is read to hold beside the review's own head: a mirror whose head is
    // `head`, the pins left where they are.
    let github_at = |head: &Oid, cx: &mut VisualTestContext| {
        session.update(cx, |session, cx| {
            let read = session.pr().cloned().unwrap();
            let mut mirror = (**read.mirror.as_ref().unwrap()).clone();
            mirror.pr.head_oid = head.clone();
            session.apply_pr_read(
                PrRead {
                    mirror: Some(Arc::new(mirror)),
                    moved_from: None,
                    ..read
                },
                cx,
            );
        });
    };

    // The author rewrites the commit and pushes it over the old one.
    let author = fixture.github.author();
    author.checkout("feat");
    author.git(&["commit", "-q", "--amend", "-m", "Add the answer, reworded"]);
    fixture.pr.head = fixture.github.open_pr(7, "feat");
    fixture.answer(Vec::new());
    let rewritten = Oid::parse(&fixture.pr.head).unwrap();
    cx.simulate_keystrokes("secondary-r");
    assert_eq!(
        moved(&shell, cx),
        (
            MoveKind::Followed,
            Some(Commits::Rewritten),
            "Updated to a force-pushed head".to_string()
        )
    );
    // GitHub read at the commit before the rewrite, the review at the one after.
    github_at(&first, cx);
    assert_eq!(
        moved(&shell, cx),
        (
            MoveKind::Ahead,
            Some(Commits::Rewritten),
            "Force-pushed since this draft's head".to_string()
        )
    );

    // A commit on top, followed.
    let second = Oid::parse(&fixture.push("pub fn answer() -> u32 {\n    43\n}\n")).unwrap();
    cx.simulate_keystrokes("secondary-r");
    assert_eq!(
        moved(&shell, cx),
        (
            MoveKind::Followed,
            Some(Commits::New(1)),
            "Updated to 1 new commit".to_string()
        )
    );
    // What was read of GitHub is older than the head the review shows.
    github_at(&rewritten, cx);
    assert_eq!(
        moved(&shell, cx),
        (
            MoveKind::Ahead,
            Some(Commits::Earlier),
            "GitHub has an earlier commit than this draft's head".to_string()
        )
    );
    // A head the clone doesn't hold can't be counted.
    github_at(&oid('1'), cx);
    assert_eq!(
        moved(&shell, cx),
        (
            MoveKind::Ahead,
            None,
            "The pull request moved since this draft's head".to_string()
        )
    );

    // The author takes the last commit back: the refresh follows to the earlier one.
    let author = fixture.github.author();
    author.checkout("feat");
    author.git(&["reset", "-q", "--hard", "HEAD~1"]);
    fixture.pr.head = fixture.github.open_pr(7, "feat");
    fixture.answer(Vec::new());
    assert_eq!(fixture.pr.head, rewritten.as_str());
    cx.simulate_keystrokes("secondary-r");
    let (kind, commits, text) = moved(&shell, cx);
    assert_eq!(
        (kind, commits, text.as_str()),
        (
            MoveKind::Followed,
            Some(Commits::Earlier),
            "Updated to an earlier commit"
        )
    );
    let shown = strip(&shell, cx).moved.unwrap();
    assert_eq!((shown.from, shown.to), (second, rewritten));

    // A refresh that moved the review from a commit the clone no longer holds.
    session.update(cx, |session, cx| {
        let read = session.pr().cloned().unwrap();
        let from = read.pins.clone().map(|pins| dv_core::review::Pins {
            head: oid('2'),
            ..pins
        });
        session.apply_pr_read(
            PrRead {
                moved_from: from,
                ..read
            },
            cx,
        );
    });
    assert_eq!(
        moved(&shell, cx),
        (
            MoveKind::Followed,
            None,
            "Updated to a new head".to_string()
        )
    );
}

/// Every stored mirror's files under `repo`'s `.git/dv/github`.
fn mirror_files(repo: &dv_test_support::FixtureRepo) -> Vec<PathBuf> {
    fn walk(dir: &std::path::Path, found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, found);
            } else {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(&repo.path().join(".git/dv/github"), &mut found);
    found
}

/// Runs the app until the open review's strip has its round, `gh` held or not.
fn strip_once_listed(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> PrStrip {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        cx.executor().advance_clock(Duration::from_millis(400));
        cx.run_until_parked();
        let shown = shell.read_with(cx, |shell, cx| shell.pr_strip(cx));
        if let Some(shown) = shown.filter(|shown| shown.round.is_some()) {
            return shown;
        }
        assert!(Instant::now() < deadline, "the strip never showed");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// PLAN §9.4: a PR review whose mirror isn't there (never read here, or removed) or can't
/// be read shows its number and round alone and says it is reading; when that read fails
/// it says so, with nothing made up in the pull request's place.
fn a_review_without_a_mirror_it_can_read(cx: &mut TestAppContext, unreadable: bool) {
    let (_dir, shell, cx) = open_shell(cx);
    let mut fixture = PrFixture::new(cx);
    let review = reviewed_elsewhere(&mut fixture, cx);
    let files = mirror_files(fixture.github.checkout());
    assert!(!files.is_empty());
    for file in files {
        if unreadable {
            std::fs::write(&file, "{ not a mirror").unwrap();
        } else {
            std::fs::remove_file(&file).unwrap();
        }
    }
    let github = cx.update(|_, cx| cx.services().github.clone());
    let slow = github.update(cx, |github, _| github.hold());
    fixture.answer(vec![fixture_gh::signed_out()]);

    let opening = open_review(&fixture, &review, &shell, cx);
    let shown = strip_once_listed(&shell, cx);
    assert_eq!((shown.number, shown.round), (7, Some(1)), "{unreadable}");
    assert_eq!(
        (&shown.title, &shown.author, &shown.branches, shown.state),
        (&None, &None, &None, None)
    );
    assert_eq!((&shown.url, &shown.checks_url), (&None, &None));
    assert_eq!(shown.moved, None);
    assert_eq!(shown.read, ReadLine::Reading);
    assert!(shown.refreshing);
    shell.read_with(cx, |shell, cx| {
        let session = shell.session().unwrap().read(cx);
        assert_eq!(session.target_label(), "#7");
        assert!(session.pr().is_none());
        // With no mirror to tell a host by, its target says it is this pull request: a
        // listed row's URL refreshes it here, and opens nothing beside the refresh.
        let url = PrRef::Url {
            host: "github.com".to_string(),
            owner: "dv-fixture".to_string(),
            repo: "api".to_string(),
            number: 7,
        };
        assert!(session.is_open_pr(&fixture.location(), &url));
    });

    // `gh` answers that it is signed out: nothing was ever read.
    drop(slow);
    settle(cx, &shell, "the failed refresh", |shell, cx| {
        !shell.session().unwrap().read(cx).is_refreshing_pr()
    });
    cx.foreground_executor().block_test(opening).unwrap();
    let shown = strip(&shell, cx);
    assert!(matches!(shown.read, ReadLine::Stale(_)), "{:?}", shown.read);
    assert_eq!((&shown.title, &shown.url), (&None, &None));
    assert!(!shown.refreshing);

    // Signed in, the refresh reads it and writes the mirror again.
    fixture.answer(Vec::new());
    cx.simulate_keystrokes("secondary-r");
    idle(&shell, cx, "the refresh");
    let shown = strip(&shell, cx);
    assert_eq!(shown.title.as_deref(), Some("Add the answer"));
    assert!(matches!(shown.read, ReadLine::Read(_)), "{:?}", shown.read);
}

#[gpui::test]
fn a_review_whose_mirror_is_gone_shows_its_number_and_says_it_is_reading(cx: &mut TestAppContext) {
    a_review_without_a_mirror_it_can_read(cx, false);
}

#[gpui::test]
fn a_review_whose_mirror_cant_be_read_shows_its_number_and_says_it_is_reading(
    cx: &mut TestAppContext,
) {
    a_review_without_a_mirror_it_can_read(cx, true);
}

/// PLAN §9.5, §9.9: a PR review's round is its place among its pull request's reviews in
/// the store, so it follows the store: an earlier review deleted from the CLI makes the
/// open one the first, in the strip and the status bar, without a refresh.
#[gpui::test]
fn the_round_follows_an_earlier_review_deleted_elsewhere(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let fixture = PrFixture::new(cx);
    let first = fixture.open(&shell, cx).unwrap().review;
    let session = shell.read_with(cx, |shell, _| shell.session().cloned().unwrap());
    let (store, reviewer) = session.read_with(cx, |session, _| {
        (session.store().clone(), session.reviewer().clone())
    });
    dv_core::review::ops::set_status(&store, &first, ReviewStatus::Closed, &reviewer, None)
        .unwrap();
    settle(cx, &shell, "the closed review", |shell, cx| {
        shell.session().unwrap().read(cx).is_read_only()
    });
    // The pull request opened again: its second review.
    let second = fixture.open(&shell, cx).unwrap();
    assert!(second.created);
    idle(&shell, cx, "the second review");
    assert_eq!(strip(&shell, cx).round, Some(2));
    shell.read_with(cx, |shell, cx| {
        assert_eq!(
            shell.status_line(cx).review.as_deref(),
            Some("Draft · round 2 · no comments")
        );
    });

    store.delete(&first).unwrap();
    settle(cx, &shell, "the round to follow the store", |shell, cx| {
        shell
            .pr_strip(cx)
            .is_some_and(|strip| strip.round == Some(1))
    });
    shell.read_with(cx, |shell, cx| {
        assert_eq!(
            shell.status_line(cx).review.as_deref(),
            Some("Draft · round 1 · no comments")
        );
    });
}
