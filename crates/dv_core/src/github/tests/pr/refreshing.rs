//! Refreshing a PR review (PLAN §9.4, §9.5): the mirror written whole, `synced` carried
//! over, one head, what a failure keeps, and the pins.

use serde_json::json;

use super::*;
use crate::review::pins::Advance;
use crate::review::{AnchorSide, ReviewStatus};

const CHANGED: &str = "pub fn answer() -> u32 {\n    43\n}\n";

/// A `gh` of its own answering as the fixture's does, with `first` before that: one that
/// has asked nothing yet, so it doesn't know the viewer.
fn another_gh(fixture: &PrFixture, first: Vec<Value>) -> MockGh {
    let mut calls = first;
    calls.push(fixture_gh::viewer(VIEWER));
    calls.extend(fixture.pr.calls_in(&fixture.github));
    MockGh::new(Value::Array(calls))
}

#[test]
fn a_refresh_reads_everything_gh_holds_into_the_mirror() {
    let mut fixture = PrFixture::new();
    let head = fixture.head();
    fixture.pr.checks = json!([
        {"__typename": "CheckRun", "name": "test", "status": "COMPLETED", "conclusion": "SUCCESS"},
        {"__typename": "CheckRun", "name": "lint", "status": "COMPLETED", "conclusion": "SKIPPED"},
        {"__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "FAILURE"},
        {"__typename": "CheckRun", "name": "deploy", "status": "IN_PROGRESS", "conclusion": ""},
        {"__typename": "StatusContext", "context": "ci/legacy", "state": "PENDING"},
        {"__typename": "StatusContext", "context": "ci/old", "state": "ERROR"},
    ]);
    fixture.pr.is_draft = true;
    fixture.pr.threads = vec![
        fixture_gh::thread(
            "PRRT_1",
            "src/lib.rs",
            2,
            "octocat",
            "Why 42?",
            head.as_str(),
        ),
        json!({
            "id": "PRRT_2", "isResolved": true, "isOutdated": true, "path": "README.md",
            "line": null, "originalLine": 4, "startLine": null, "originalStartLine": 2,
            "diffSide": "LEFT", "startDiffSide": "LEFT", "subjectType": "LINE",
            "viewerCanResolve": false, "viewerCanUnresolve": true, "viewerCanReply": true,
            "comments": fixture_gh::connection(vec![json!({
                "id": "PRRC_2", "fullDatabaseId": 3000000001u64, "author": null,
                "body": "Old.", "createdAt": "2026-10-18T07:00:00Z",
                "url": "https://github.com/dv-fixture/api/pull/7#discussion_r3000000001",
                "diffHunk": "@@ -1,4 +1,4 @@", "outdated": true, "commit": null,
                "originalCommit": {"oid": head.as_str()}, "replyTo": {"id": "PRRC_1"},
                "pullRequestReview": {"id": "PRR_9", "fullDatabaseId": "9", "state": "PENDING"}
            })])
        }),
        json!({
            "id": "PRRT_3", "isResolved": false, "isOutdated": false, "path": "src/lib.rs",
            "line": null, "originalLine": null, "startLine": null, "originalStartLine": null,
            "diffSide": "RIGHT", "startDiffSide": null, "subjectType": "FILE",
            "viewerCanResolve": true, "viewerCanUnresolve": false, "viewerCanReply": true,
            "comments": fixture_gh::connection(Vec::new())
        }),
    ];
    fixture.pr.pending_review = Some(json!({
        "id": "PRR_9", "fullDatabaseId": "9", "body": "<!-- dv:r-x -->",
        "commit": {"oid": head.as_str()}
    }));
    fixture
        .pr
        .files
        .push(fixture_gh::file("assets/logo.png", "added", None));
    fixture.pr.files.push(json!({
        "filename": "docs/new.md", "previous_filename": "docs/old.md", "status": "renamed"
    }));
    fixture.pr.viewed = vec![
        ("src/lib.rs".to_string(), "VIEWED".to_string()),
        ("docs/new.md".to_string(), "DISMISSED".to_string()),
    ];
    fixture.answer(Vec::new());

    let mirror = fixture.open().refreshed.mirror.unwrap();
    assert_eq!(mirror.pr.state, PrState::Draft);
    assert_eq!(mirror.pr.id, "PR_7");
    assert_eq!(mirror.pr.url, "https://github.com/dv-fixture/api/pull/7");
    assert_eq!(mirror.pr.author, "octocat");
    assert_eq!(
        (mirror.pr.base_ref.as_str(), mirror.pr.head_ref.as_str()),
        ("main", "feat")
    );
    assert_eq!(
        mirror.pr.updated_at,
        parse_timestamp("2026-10-19T08:30:00Z").unwrap()
    );
    assert_eq!(mirror.pr.review_decision, None);
    assert_eq!(
        mirror.checks,
        Checks {
            passing: 2,
            failing: 2,
            pending: 2
        }
    );

    assert_eq!(mirror.threads.len(), 3);
    let line = &mirror.threads[0];
    assert_eq!(
        (line.id.as_str(), line.path.as_str()),
        ("PRRT_1", "src/lib.rs")
    );
    assert_eq!(
        line.subject,
        ThreadSubject::Lines(ThreadLines {
            side: AnchorSide::New,
            line: Some(2),
            original_line: Some(2),
            start: None,
        })
    );
    assert!(line.viewer_can_resolve && !line.viewer_can_unresolve && line.viewer_can_reply);
    let comment = &line.comments[0];
    assert_eq!(comment.database_id, 1001);
    assert_eq!(comment.author, "octocat");
    assert_eq!(comment.body, "Why 42?");
    assert_eq!(comment.commit.as_ref(), Some(&head));
    assert_eq!(
        comment.review,
        Some(CommentReview {
            id: "PRR_1".to_string(),
            database_id: 2001,
            state: ReviewState::Commented,
        })
    );

    let outdated = &mirror.threads[1];
    assert!(outdated.resolved && outdated.outdated);
    assert_eq!(
        outdated.subject,
        ThreadSubject::Lines(ThreadLines {
            side: AnchorSide::Old,
            line: None,
            original_line: Some(4),
            start: Some(ThreadStart {
                side: AnchorSide::Old,
                line: None,
                original_line: Some(2),
            }),
        })
    );
    let comment = &outdated.comments[0];
    assert_eq!(comment.author, "ghost");
    assert_eq!(comment.database_id, 3_000_000_001);
    assert_eq!(comment.reply_to.as_deref(), Some("PRRC_1"));
    assert_eq!(comment.commit, None);
    assert_eq!(mirror.threads[2].subject, ThreadSubject::File);

    // The viewer's pending review, and its comment among the threads.
    assert_eq!(
        mirror.pending_review,
        Some(PendingReview {
            id: "PRR_9".to_string(),
            database_id: 9,
            body: "<!-- dv:r-x -->".to_string(),
            commit: Some(head.clone()),
        })
    );
    assert_eq!(mirror.pending_comments().count(), 1);

    // GitHub's patches, and each file's Viewed state with nothing synced yet.
    let paths: Vec<_> = mirror.files.iter().map(|file| file.path.as_str()).collect();
    assert_eq!(paths, ["src/lib.rs", "assets/logo.png", "docs/new.md"]);
    assert!(
        mirror.files[0]
            .patch
            .as_deref()
            .unwrap()
            .starts_with("@@ -0,0 +1,3 @@")
    );
    assert_eq!(mirror.files[1].patch, None);
    assert_eq!(
        mirror.files[2].previous_path.as_deref(),
        Some("docs/old.md")
    );
    assert_eq!(mirror.files[2].status, FileStatus::Renamed);
    let viewed = |path: &str| mirror.viewed[path];
    assert_eq!(
        viewed("src/lib.rs"),
        ViewedFile {
            state: ViewedState::Viewed,
            synced: None
        }
    );
    assert_eq!(viewed("assets/logo.png").state, ViewedState::Unviewed);
    assert_eq!(viewed("docs/new.md").state, ViewedState::Dismissed);

    // What was asked: the threads with §9.6's fields, the files 100 a page, the head last.
    let asked = fixture.asked();
    assert_eq!(
        asked.last().unwrap(),
        "pr view 7 --json headRefOid --repo=github.com/dv-fixture/api"
    );
    assert!(
        asked.contains(
            &"api repos/dv-fixture/api/pulls/7/files?per_page=100 --hostname=github.com --paginate --slurp"
                .to_string()
        ),
        "{asked:?}"
    );
    let requests: Vec<String> = fixture
        .mock
        .calls()
        .iter()
        .filter_map(|call| call["stdin"].as_str().map(str::to_string))
        .collect();
    let threads = requests
        .iter()
        .find(|request| request.contains("reviewThreads(first"))
        .unwrap();
    for field in [
        "id isResolved isOutdated path line originalLine startLine originalStartLine diffSide \
         startDiffSide subjectType viewerCanResolve viewerCanUnresolve viewerCanReply",
        "id fullDatabaseId author { login } body createdAt url diffHunk outdated commit { oid } \
         originalCommit { oid } replyTo { id } pullRequestReview { id fullDatabaseId state }",
        "\"owner\":\"dv-fixture\"",
        "\"repo\":\"api\"",
        "\"number\":7",
    ] {
        assert!(threads.contains(field), "{field}: {threads}");
    }
    let pending = requests
        .iter()
        .find(|request| request.contains("states: [PENDING]"))
        .unwrap();
    assert!(pending.contains("\"login\":\"kylekz\""), "{pending}");
}

#[test]
fn synced_is_carried_over_from_the_mirror_a_refresh_replaces() {
    let mut fixture = PrFixture::new();
    fixture.pr.files.push(fixture_gh::file(
        "README.md",
        "modified",
        Some("@@ -1 +1 @@"),
    ));
    fixture.answer(Vec::new());
    let opened = fixture.open();
    let mut mirror = *opened.refreshed.mirror.unwrap();
    mirror.viewed.get_mut("src/lib.rs").unwrap().synced = Some(true);
    mirror.viewed.get_mut("README.md").unwrap().synced = Some(false);
    fixture.mirrors().write(&mirror).unwrap();

    // GitHub's own state changes, and a file leaves the pull request.
    fixture.pr.files.pop();
    fixture.pr.viewed = vec![("src/lib.rs".to_string(), "DISMISSED".to_string())];
    fixture.answer(Vec::new());
    let refreshed = fixture.refresh(&opened.review);
    let mirror = refreshed.mirror.unwrap();
    assert_eq!(
        mirror.viewed.into_iter().collect::<Vec<_>>(),
        [(
            "src/lib.rs".to_string(),
            ViewedFile {
                state: ViewedState::Dismissed,
                synced: Some(true)
            }
        )]
    );
    assert!(mirror.read_at >= opened.review.created_at);

    // With no mirror to carry from, there is no base.
    let dir = fixture.github.checkout().path().join(".git/dv");
    std::fs::write(dir.join(fixture.key().path()), "{\"v\": 99}").unwrap();
    let mirror = fixture.refresh(&opened.review).mirror.unwrap();
    assert_eq!(mirror.viewed["src/lib.rs"].synced, None);
}

#[test]
fn a_head_that_moves_under_the_read_starts_it_over_once() {
    let mut fixture = PrFixture::new();
    let opened = fixture.open();
    let old = fixture.head();
    let mut before = fixture.pr.clone();
    before.files = vec![fixture_gh::file(
        "src/lib.rs",
        "added",
        Some("@@ the old patch"),
    )];
    // The author's next commit is made, and reaches GitHub only while the refresh reads the
    // threads (below).
    let author = fixture.github.author();
    author.checkout("feat").write("src/lib.rs", CHANGED);
    let head = oid(&author.commit("Change the answer"));
    fixture.pr.head = head.to_string();
    fixture.pr.files = vec![fixture_gh::file(
        "src/lib.rs",
        "added",
        Some("@@ the new patch"),
    )];
    // The first pass sees the old pull request, files and all, and is slow to get its
    // threads; only its last look at the head finds the push.
    let mut first: Vec<Value> = before
        .calls("api")
        .into_iter()
        .skip(1)
        .map(|mut entry| {
            entry["times"] = json!(1);
            entry
        })
        .collect();
    first.remove(3);
    first[1]["delay_ms"] = json!(1500);
    fixture.answer(first);

    let mut push = author.git_command();
    push.arg("push")
        .arg("-q")
        .arg(fixture.github.bare())
        .arg(format!("+{head}:refs/pull/7/head"));
    let refreshed = std::thread::scope(|scope| {
        scope.spawn(|| {
            // `mock_gh` logs a call before it waits to answer it.
            let asked_for_threads = || {
                fixture.mock.calls().iter().any(|call| {
                    call["stdin"]
                        .as_str()
                        .is_some_and(|request| request.contains("reviewThreads(first"))
                })
            };
            while !asked_for_threads() {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(push.status().unwrap().success());
        });
        refresh(&fixture.backend(), &fixture.mock.gh, &opened.review)
    });
    assert!(refreshed.stale.is_none(), "{:?}", refreshed.stale);
    let mirror = refreshed.mirror.unwrap();
    // The files and the head describe one commit: the new one.
    assert_eq!(mirror.pr.head_oid, head);
    assert_eq!(mirror.files[0].patch.as_deref(), Some("@@ the new patch"));
    assert_eq!(refreshed.pins.unwrap().head, head);
    assert_eq!(refreshed.moved_from.unwrap().head, old);
    let views = fixture
        .asked()
        .iter()
        .filter(|call| call.starts_with("pr view 7 --json id,"))
        .count();
    assert_eq!(views, 2);
}

#[test]
fn a_head_that_moves_twice_under_the_read_keeps_the_last_mirror() {
    let fixture = PrFixture::new();
    let opened = fixture.open();
    let last = *opened.refreshed.mirror.unwrap();
    // Whenever the head is looked at again, it is somewhere else.
    fixture.answer(vec![json!({
        "argv": ["pr", "view", "7", "--json", "headRefOid", "**"],
        "stdout": {"headRefOid": "f".repeat(40)}
    })]);
    let refreshed = fixture.refresh(&opened.review);
    assert!(
        matches!(refreshed.stale, Some(GithubError::Moved { number: NUMBER })),
        "{:?}",
        refreshed.stale
    );
    assert_eq!(refreshed.mirror.as_deref(), Some(&last));
    assert_eq!(
        fixture.mirrors().read(&fixture.key()).unwrap(),
        MirrorRead::Found(Box::new(last))
    );
}

#[test]
fn a_failed_refresh_keeps_the_last_mirror_and_says_why() {
    let mut fixture = PrFixture::new();
    let opened = fixture.open();
    let last = *opened.refreshed.mirror.unwrap();
    let before = fixture.pins(&opened.review);

    // GitHub can't be reached at all: nothing moves.
    fixture.answer(vec![json!({
        "argv": ["pr", "**"],
        "stderr": "error connecting to api.github.com\ncheck your internet connection or https://githubstatus.com\n",
        "exit": 1
    })]);
    let refreshed = fixture.refresh(&opened.review);
    let stale = refreshed.stale.unwrap();
    assert!(
        matches!(stale, GithubError::Unavailable { .. }),
        "{stale:?}"
    );
    assert_eq!(stale.kind(), "unavailable");
    assert_eq!(refreshed.mirror.as_deref(), Some(&last));
    assert_eq!(refreshed.pins, Some(before.clone()));
    assert_eq!(refreshed.moved_from, None);

    // The pull request and its commits can be read, the threads can't. One head: without a
    // mirror written for the new head the pins stay where the last mirror's is, though the
    // fetch already brought the new commits.
    let head = fixture.push(CHANGED);
    fixture.answer(vec![json!({
        "argv": ["api", "graphql", "**"],
        "stdin_contains": "reviewThreads(first",
        "stdout": "HTTP/2.0 200 OK\n\r\n{\"errors\":[{\"type\":\"RATE_LIMITED\",\"message\":\"API rate limit exceeded for user ID 1.\"}]}",
        "stderr": "gh: API rate limit exceeded for user ID 1.\n",
        "exit": 1
    })]);
    let refreshed = fixture.refresh(&opened.review);
    let stale = refreshed.stale.unwrap();
    assert!(
        matches!(stale, GithubError::RateLimited { .. }),
        "{stale:?}"
    );
    assert_eq!(refreshed.mirror.as_deref(), Some(&last));
    assert_eq!(refreshed.pins, Some(before.clone()));
    assert_eq!(refreshed.moved_from, None);
    assert_eq!(fixture.pins(&opened.review), before);
    assert_eq!(last.pr.head_oid, before.head);
    assert_eq!(
        fixture.mirrors().read(&fixture.key()).unwrap(),
        MirrorRead::Found(Box::new(last.clone()))
    );
    assert!(fixture.refs("refs/dv/pr/").contains(&(
        "refs/dv/pr/dv-fixture/api/7/head".to_string(),
        head.to_string()
    )));

    // A mirror that can't be written moves nothing either.
    fixture.answer(Vec::new());
    let file = fixture
        .github
        .checkout()
        .path()
        .join(".git/dv")
        .join(fixture.key().path());
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
    let refreshed = fixture.refresh(&opened.review);
    assert!(
        matches!(refreshed.stale, Some(GithubError::Store(_))),
        "{:?}",
        refreshed.stale
    );
    assert_eq!(fixture.pins(&opened.review), before);
    std::fs::remove_dir(&file).unwrap();

    // Read whole again, the mirror and the pins move together.
    let refreshed = fixture.refresh(&opened.review);
    assert!(refreshed.stale.is_none(), "{:?}", refreshed.stale);
    assert_eq!(refreshed.mirror.unwrap().pr.head_oid, head);
    assert_eq!(refreshed.pins.unwrap().head, head);
    assert_eq!(refreshed.moved_from, Some(before));
}

#[test]
fn the_latest_mirror_stands_in_when_the_viewer_cant_be_read() {
    let fixture = PrFixture::new();
    let opened = fixture.open();
    let mine = *opened.refreshed.mirror.unwrap();
    // Another login read the pull request more recently (the distro's gh, say).
    let theirs = Mirror {
        key: MirrorKey::new(&fixture.repo(), NUMBER, "octocat"),
        read_at: mine.read_at + 60_000,
        ..mine.clone()
    };
    fixture.mirrors().write(&theirs).unwrap();
    let before = fixture.pins(&opened.review);

    // A gh that is signed out can't say who is asking.
    let signed_out = another_gh(&fixture, vec![fixture_gh::signed_out()]);
    let refreshed = refresh(&fixture.backend(), &signed_out.gh, &opened.review);
    let stale = refreshed.stale.unwrap();
    assert!(
        matches!(stale, GithubError::NotAuthenticated { .. }),
        "{stale:?}"
    );
    assert_eq!(refreshed.mirror.as_deref(), Some(&theirs));
    assert_eq!(refreshed.pins, Some(before));
    // The viewer was the one thing asked.
    assert_eq!(signed_out.calls().len(), 1);

    // With no mirror of anyone's, there is none to show.
    let other = PrFixture::new();
    let review = other.open().review;
    std::fs::remove_file(
        other
            .github
            .checkout()
            .path()
            .join(".git/dv")
            .join(other.key().path()),
    )
    .unwrap();
    let signed_out = another_gh(&other, vec![fixture_gh::signed_out()]);
    let refreshed = refresh(&other.backend(), &signed_out.gh, &review);
    assert_eq!(refreshed.mirror, None);
    assert!(refreshed.stale.is_some());
}

#[test]
fn the_viewer_is_asked_once_per_gh() {
    let fixture = PrFixture::new();
    let opened = fixture.open();
    fixture.refresh(&opened.review);
    fixture.refresh(&opened.review);
    let asked = fixture.asked();
    assert_eq!(
        asked
            .iter()
            .filter(|call| call.starts_with("api user "))
            .count(),
        1,
        "{asked:?}"
    );
    // A failure isn't remembered: signing in is seen without a restart.
    let fresh = another_gh(
        &fixture,
        vec![{
            let mut once = fixture_gh::signed_out();
            once["times"] = json!(1);
            once
        }],
    );
    assert!(fresh.gh.viewer("github.com").is_err());
    assert_eq!(fresh.gh.viewer("github.com").unwrap(), VIEWER);
    assert_eq!(fresh.gh.viewer("github.com").unwrap(), VIEWER);
    assert_eq!(fresh.calls().len(), 2);
}

#[test]
fn a_refresh_that_cant_tell_the_repository_keeps_the_stored_mirror() {
    // A repository gh can't vouch for any more (its remote renamed away, gh gone for a
    // GitHub Enterprise host): the mirror read before still stands, stale.
    let fixture = PrFixture::new();
    let opened = fixture.open();
    let last = *opened.refreshed.mirror.unwrap();
    fixture
        .github
        .checkout()
        .git(&["remote", "rename", "origin", "github"]);
    fixture.github.checkout().config(
        "remote.other.url",
        "https://github.com/dv-fixture/elsewhere.git",
    );
    let refreshed = fixture.refresh(&opened.review);
    assert!(
        matches!(refreshed.stale, Some(GithubError::NoRepository { .. })),
        "{:?}",
        refreshed.stale
    );
    assert_eq!(refreshed.mirror.as_deref(), Some(&last));
    assert_eq!(refreshed.pins, Some(fixture.pins(&opened.review)));
    assert_eq!(refreshed.moved_from, None);
}

#[test]
fn a_local_review_and_another_clones_review_refresh_nothing() {
    let fixture = PrFixture::new();
    let local = fixture
        .store()
        .create(crate::target::Target::Working, reviewer())
        .unwrap();
    let refreshed = fixture.refresh(&local);
    assert!(refreshed.mirror.is_none() && refreshed.stale.is_none() && refreshed.pins.is_none());

    // A PR review whose repository the clone's remote no longer names.
    let opened = fixture.open();
    fixture
        .github
        .checkout()
        .config("remote.origin.url", "https://github.com/dv-fixture/web.git");
    let refreshed = fixture.refresh(&opened.review);
    assert!(
        matches!(
            &refreshed.stale,
            Some(GithubError::OtherRepository { number: 7, repository }) if repository == "dv-fixture/api"
        ),
        "{:?}",
        refreshed.stale
    );
    assert_eq!(refreshed.pins, Some(fixture.pins(&opened.review)));
}

#[test]
fn only_a_draft_follows_the_head() {
    let mut fixture = PrFixture::new();
    let opened = fixture.open();
    let before = fixture.pins(&opened.review);
    crate::review::ops::set_status(
        &fixture.store(),
        &opened.review.id,
        ReviewStatus::Closed,
        &reviewer(),
        None,
    )
    .unwrap();
    let closed = fixture.store().load(&opened.review.id).unwrap().review;
    let head = fixture.push(CHANGED);
    let refreshed = fixture.refresh(&closed);
    // The mirror moved on; the closed draft shows what it showed.
    assert_eq!(refreshed.mirror.unwrap().pr.head_oid, head);
    assert_eq!(refreshed.pins, Some(before.clone()));
    assert_eq!(refreshed.moved_from, None);
    assert_eq!(fixture.pins(&closed), before);
}

#[test]
fn the_pins_move_by_compare_and_swap() {
    let mut fixture = PrFixture::new();
    let opened = fixture.open();
    let id = &opened.review.id;
    let git = fixture.git();
    let read = fixture.pins(&opened.review);
    assert_eq!(
        pins::advance(&git, id, Some(&read), &read).unwrap(),
        Advance::Unchanged
    );

    // Another process refreshes the draft to a new head behind this one's back.
    let theirs = Pins {
        head: fixture.push(CHANGED),
        base: read.base.clone(),
    };
    fixture.github.checkout().git(&[
        "fetch",
        "-q",
        "origin",
        "+refs/pull/7/head:refs/dv/pr/dv-fixture/api/7/head",
    ]);
    fixture.github.checkout().git(&[
        "update-ref",
        &pins::head_ref(id),
        theirs.head.as_str(),
        read.head.as_str(),
    ]);

    // This one, still holding what it read, loses: nothing is written, and their pins stand.
    let mine = Pins {
        head: read.base.clone(),
        base: read.base.clone(),
    };
    assert_eq!(
        pins::advance(&git, id, Some(&read), &mine).unwrap(),
        Advance::Lost(theirs.clone())
    );
    assert_eq!(fixture.pins(&opened.review), theirs);
    // From where they stand now, the move goes through, both pins at once.
    assert_eq!(
        pins::advance(&git, id, Some(&theirs), &mine).unwrap(),
        Advance::Moved {
            from: Some(theirs.clone())
        }
    );
    assert_eq!(fixture.pins(&opened.review), mine);

    // Pins that exist can't be made again, and a pin read as missing must still be.
    assert_eq!(
        pins::advance(&git, id, None, &theirs).unwrap(),
        Advance::Lost(mine.clone())
    );
    assert_eq!(fixture.pins(&opened.review), mine);

    // Two refreshes racing from one read: one moves the pins, the other finds them moved.
    let from = mine;
    let outcomes: Vec<Advance> = std::thread::scope(|scope| {
        let racers: Vec<_> = [&theirs, &read]
            .into_iter()
            .map(|to| {
                let (git, from) = (fixture.git(), from.clone());
                scope.spawn(move || pins::advance(&git, id, Some(&from), to).unwrap())
            })
            .collect();
        racers
            .into_iter()
            .map(|racer| racer.join().unwrap())
            .collect()
    });
    let now = fixture.pins(&opened.review);
    let moved = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, Advance::Moved { .. }))
        .count();
    assert_eq!(moved, 1, "{outcomes:?}");
    assert!(
        outcomes.contains(&Advance::Lost(now.clone())),
        "{outcomes:?}"
    );
    assert!(now == theirs || now == read);
}

#[test]
fn a_draft_pinned_again_says_which_head_it_showed() {
    let mut fixture = PrFixture::new();
    let opened = fixture.open();
    let shown = fixture.pins(&opened.review);
    // Its pins removed by hand, it shows its target's commits; then the pull request moves.
    let id = &opened.review.id;
    for name in [pins::head_ref(id), pins::base_ref(id)] {
        fixture.github.checkout().git(&["update-ref", "-d", &name]);
    }
    let head = fixture.push(CHANGED);
    let refreshed = fixture.refresh(&opened.review);
    assert!(refreshed.stale.is_none(), "{:?}", refreshed.stale);
    assert_eq!(refreshed.pins.as_ref().map(|pins| &pins.head), Some(&head));
    // Moved from the head the review resolved to without them, not from nothing.
    assert_eq!(refreshed.moved_from, Some(shown));
}

#[test]
fn a_pr_review_without_pins_resolves_as_its_target() {
    let fixture = PrFixture::new();
    let opened = fixture.open();
    let git = fixture.git();
    let (id, target) = (&opened.review.id, &opened.review.target);
    let pinned = pins::resolve(&git, id, target).unwrap();
    assert_eq!(pinned, fixture.pins(&opened.review).spec());
    // Its refs removed by hand: the commits it was made at, which the fetch still holds.
    for name in [pins::head_ref(id), pins::base_ref(id)] {
        fixture.github.checkout().git(&["update-ref", "-d", &name]);
    }
    assert_eq!(pins::read(&git, id).unwrap(), None);
    assert_eq!(pins::resolve(&git, id, target).unwrap(), pinned);
    // One pin alone is none; the next refresh of the draft pins both again.
    fixture
        .github
        .checkout()
        .git(&["update-ref", &pins::head_ref(id), fixture.main().as_str()]);
    assert_eq!(pins::read(&git, id).unwrap(), None);
    let refreshed = fixture.refresh(&opened.review);
    assert!(refreshed.stale.is_none(), "{:?}", refreshed.stale);
    assert_eq!(
        refreshed.pins.as_ref().map(Pins::spec),
        Some(pinned.clone())
    );
    assert_eq!(pins::resolve(&git, id, target).unwrap(), pinned);
    // Pinned again at the commits it showed, nothing moved.
    assert_eq!(refreshed.moved_from, None);
    // A local review's sides are its target's.
    let local = fixture
        .store()
        .create(
            crate::target::Target::Commit { rev: "HEAD".into() },
            reviewer(),
        )
        .unwrap();
    assert_eq!(
        pins::resolve(&git, &local.id, &local.target).unwrap(),
        local.target.resolve(&git).unwrap()
    );
}
