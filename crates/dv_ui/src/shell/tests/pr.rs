//! A GitHub pull request in the shell (PLAN §9.3–§9.5), against the fixture GitHub and
//! `mock_gh`: opened by its reference, shown from its pins, refreshed, followed when another
//! process refreshes it, and what a `gh` that fails shows.

use std::path::PathBuf;

use dv_core::github::{Gh, GhOptions, PrRef};
use dv_core::review::{AnchorSide, Pins, ReviewStatus, pins};
use dv_core::{DiffSpec, GitRepo, Side};
use dv_test_support::fixture_gh::{self, GhPr};
use dv_test_support::{FixtureGithub, mock_gh};
use gpui::{TestAppContext, VisualTestContext};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

use super::*;
use crate::services::GitHub;

mod threads;

use crate::test_support::{open_shell, register, settle};

/// The fixture GitHub's `dv-fixture/api` with pull request 7 (one commit adding `src/lib.rs`
/// on `feat`), and the `mock_gh` scenario file the app's `gh` answers from.
struct PrFixture {
    github: FixtureGithub,
    pr: GhPr,
    scenario: PathBuf,
    _dir: tempfile::TempDir,
}

impl PrFixture {
    /// The fixture, the app's `GitHub` service running `mock_gh` over it, and its clone
    /// registered with `Locations`.
    fn new(cx: &mut VisualTestContext) -> Self {
        let fixture = Self::without_gh(cx);
        let mut options = GhOptions::new(mock_gh().into_os_string());
        options.env = vec![(
            "DV_MOCK_GH".into(),
            Some(fixture.scenario.clone().into_os_string()),
        )];
        let service = cx.update(|_, cx| cx.services().github.clone());
        service.update(cx, |service, _| {
            *service = GitHub::with_gh(Gh::new(options))
        });
        fixture
    }

    /// The fixture and its clone registered, the app's `GitHub` service left as the app
    /// built it.
    fn without_gh(cx: &mut VisualTestContext) -> Self {
        let github = FixtureGithub::new("api");
        let author = github.author();
        author
            .branch("feat")
            .checkout("feat")
            .write("src/lib.rs", "pub fn answer() -> u32 {\n    42\n}\n");
        author.commit("Add the answer");
        let head = github.open_pr(7, "feat");
        let mut pr = GhPr::new(7, &head);
        pr.files = vec![fixture_gh::file(
            "src/lib.rs",
            "added",
            Some("@@ -0,0 +1,3 @@"),
        )];
        let dir = tempfile::tempdir().unwrap();
        let scenario = dir.path().join("scenario.json");
        register(github.checkout(), cx);
        let fixture = Self {
            github,
            pr,
            scenario,
            _dir: dir,
        };
        fixture.answer(Vec::new());
        fixture
    }

    /// Has `gh` answer about the pull request as it stands, `first` before that.
    fn answer(&self, first: Vec<Value>) {
        let mut calls = first;
        calls.push(fixture_gh::viewer("kylekz"));
        calls.extend(self.pr.calls_in(&self.github));
        std::fs::write(&self.scenario, json!({ "calls": calls }).to_string()).unwrap();
    }

    /// A push to the pull request, which `gh` reports from now on; the new head.
    fn push(&mut self, text: &str) -> String {
        let author = self.github.author();
        author.checkout("feat").write("src/lib.rs", text);
        author.commit("Change the answer");
        self.pr.head = self.github.open_pr(7, "feat");
        self.answer(Vec::new());
        self.pr.head.clone()
    }

    fn location(&self) -> Location {
        Location::Local(self.github.checkout().path().to_path_buf())
    }

    /// Opens pull request 7 through the shell and waits for its files.
    fn open(&self, shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Result<Opened, Failure> {
        let location = self.location();
        let opening = cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.open(location, OpenRequest::Pr(PrRef::Hash(7)), window, cx)
            })
        });
        settle(cx, shell, "the pull request's open", |shell, _| {
            shell.opening().is_none()
        });
        cx.foreground_executor().block_test(opening)
    }
}

/// The open session's two sides, as commits.
fn sides(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> (String, String) {
    shell.read_with(cx, |shell, cx| {
        let session = shell.session().expect("a review is open").read(cx);
        match session.spec() {
            Some(DiffSpec {
                old: Side::Commit(old),
                new: Side::Commit(new),
            }) => (old.to_string(), new.to_string()),
            other => panic!("not two commits: {other:?}"),
        }
    })
}

#[gpui::test]
fn a_pull_request_opens_as_its_draft_and_follows_a_refresh(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let mut fixture = PrFixture::new(cx);
    let main = fixture
        .github
        .checkout()
        .git(&["rev-parse", "main"])
        .trim()
        .to_string();
    let first = fixture.pr.head.clone();

    let opened = fixture.open(&shell, cx).unwrap();
    assert!(opened.created);
    let session = shell.read_with(cx, |shell, _| shell.session().cloned().unwrap());
    session.read_with(cx, |session, _| {
        assert_eq!(session.review().status, ReviewStatus::Draft);
        assert!(session.review().is_pr_review());
        // Named by the pull request, its sides by its branches.
        assert_eq!(session.target_label(), "#7 Add the answer");
        assert_eq!(
            session.side_labels(),
            ("main".to_string(), "feat".to_string())
        );
        let paths: Vec<&str> = session
            .files()
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        assert_eq!(paths, ["src/lib.rs"]);
        // What the open read came with it: nothing is asked again.
        let read = session.pr().expect("the open's refresh");
        assert_eq!(read.stale, None);
        assert_eq!(read.mirror.as_ref().unwrap().pr.title, "Add the answer");
        assert!(!session.is_refreshing_pr());
    });
    assert_eq!(sides(&shell, cx), (main.clone(), first.clone()));

    // A comment on line 2, then a push that puts two lines above it.
    let (store, id) = session.read_with(cx, |session, _| {
        (session.store().clone(), session.review_id().clone())
    });
    let anchor = session
        .read_with(cx, |session, _| {
            session.anchor_for("src/lib.rs", AnchorSide::New, 2, 2)
        })
        .unwrap();
    let reviewer = session.read_with(cx, |session, _| session.reviewer().clone());
    let thread =
        dv_core::review::ops::add_thread(&store, &id, anchor, "Why 42?", reviewer).unwrap();
    let second = fixture.push("// The answer.\n\npub fn answer() -> u32 {\n    42\n}\n");
    settle(cx, &shell, "the thread", |shell, cx| {
        shell
            .session()
            .unwrap()
            .read(cx)
            .review()
            .thread(&thread)
            .is_some()
    });
    // Nothing polls: the review shows the head it was last refreshed to.
    assert_eq!(sides(&shell, cx), (main.clone(), first.clone()));

    session.update(cx, |session, cx| session.refresh_pr(cx));
    assert!(session.read_with(cx, |session, _| session.is_refreshing_pr()));
    settle(cx, &shell, "the refresh", |shell, cx| {
        !shell.session().unwrap().read(cx).is_refreshing_pr()
    });
    assert_eq!(sides(&shell, cx), (main.clone(), second.clone()));
    session.read_with(cx, |session, _| {
        let read = session.pr().unwrap();
        assert_eq!(read.stale, None);
        assert_eq!(read.pins.as_ref().unwrap().head.as_str(), second);
        assert_eq!(read.moved_from.as_ref().unwrap().head.as_str(), first);
        // The thread went with its line, as a branch review's does after a commit.
        let review = session.review();
        let placement = session.placement(review.thread(&thread).unwrap());
        assert_eq!(
            placement.lines(&review.thread(&thread).unwrap().anchor),
            Some((4, 4))
        );
    });

    // Another process refreshes the review (`dv review refresh`): its mirror's file changes
    // in the store, and the session follows the pins it moved.
    let third = fixture.push("// The answer.\n\npub fn answer() -> u32 {\n    42\n}\n// end\n");
    let backend = session.read_with(cx, |session, _| session.backend().clone());
    let gh = cx.update(|_, cx| cx.services().github.read(cx).gh());
    let review = session.read_with(cx, |session, _| session.review().clone());
    let refreshed = dv_core::github::refresh(&backend, &gh, &review);
    assert!(refreshed.stale.is_none(), "{:?}", refreshed.stale);
    // Everything the session follows, in the one condition: its files are listed by one
    // task, and what it keeps of the pull request is read by another.
    settle(cx, &shell, "the other process's refresh", |shell, cx| {
        let session = shell.session().unwrap().read(cx);
        session
            .spec()
            .is_some_and(|spec| spec.new == Side::Commit(third.parse().unwrap()))
            && session.pr().is_some_and(|read| {
                read.mirror
                    .as_ref()
                    .is_some_and(|mirror| mirror.pr.head_oid.as_str() == third)
                    && read
                        .pins
                        .as_ref()
                        .is_some_and(|pins| pins.head.as_str() == third)
            })
    });
    session.read_with(cx, |session, _| {
        let read = session.pr().unwrap();
        assert_eq!(read.mirror.as_ref().unwrap().pr.head_oid.as_str(), third);
        assert_eq!(read.pins.as_ref().unwrap().head.as_str(), third);
        // From the head the session showed, so the strip can say the pull request moved.
        assert_eq!(read.moved_from.as_ref().unwrap().head.as_str(), second);
    });

    // Finishing is submitting to GitHub, which this version doesn't do: no sheet, a notice.
    cx.update(|window, cx| shell.update(cx, |shell, cx| shell.open_sheet(window, cx)));
    shell.read_with(cx, |shell, _| {
        assert!(shell.sheet().is_none());
        let banner = shell.banner().expect("the notice");
        assert_eq!(banner.failure.message, SUBMITTING_LATER);
        assert!(banner.retry.is_none());
    });

    // Opening the pull request again resumes the review that is open.
    let again = fixture.open(&shell, cx).unwrap();
    assert!(!again.created);
    assert_eq!(again.review, id);
    shell.read_with(cx, |shell, _| assert!(shell.banner().is_none()));
}

#[gpui::test]
fn a_pr_review_opened_by_its_id_is_refreshed_as_it_opens(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let mut fixture = PrFixture::new(cx);
    // The review exists (the CLI made it, say), and the pull request moved on since.
    let gh = cx.update(|_, cx| cx.services().github.read(cx).gh());
    let backend: std::sync::Arc<dyn dv_core::Backend> = std::sync::Arc::new(
        dv_core::LocalBackend::open_with(
            fixture.github.checkout().path(),
            dv_core::LocalBackendOptions {
                env: fixture.github.checkout().env_overlay(),
            },
        )
        .unwrap(),
    );
    let opened = dv_core::github::open(&backend, &gh, &PrRef::Hash(7), || {
        dv_core::review::Author::reviewer("dv fixture")
    })
    .unwrap();
    let opened = Opened {
        review: opened.review.id,
        created: opened.created,
    };
    let first = fixture.pr.head.clone();
    let second = fixture.push("pub fn answer() -> u32 {\n    43\n}\n");

    let location = fixture.location();
    let opening = cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open(
                location,
                OpenRequest::Review(opened.review.clone()),
                window,
                cx,
            )
        })
    });
    settle(cx, &shell, "the review and its refresh", |shell, cx| {
        shell.session().is_some_and(|session| {
            let session = session.read(cx);
            session.pr().is_some() && !session.is_refreshing_pr()
        })
    });
    cx.foreground_executor().block_test(opening).unwrap();
    let session = shell.read_with(cx, |shell, _| shell.session().cloned().unwrap());
    let backend = session.read_with(cx, |session, _| session.backend().clone());
    let pinned = pins::read(&GitRepo::new(backend), &opened.review)
        .unwrap()
        .unwrap();
    assert_eq!(pinned.head.as_str(), second);
    session.read_with(cx, |session, _| {
        assert_eq!(
            session
                .pr()
                .unwrap()
                .moved_from
                .as_ref()
                .map(|pins: &Pins| pins.head.to_string()),
            Some(first.clone())
        );
        assert_eq!(
            session.spec().unwrap().new,
            Side::Commit(pinned.head.clone())
        );
    });

    // A refresh that fails keeps the mirror and says why; the review stays as it is.
    fixture.answer(vec![fixture_gh::signed_out()]);
    session.update(cx, |session, cx| session.refresh_pr(cx));
    settle(cx, &shell, "the failed refresh", |shell, cx| {
        !shell.session().unwrap().read(cx).is_refreshing_pr()
    });
    session.read_with(cx, |session, _| {
        let read = session.pr().unwrap();
        assert_eq!(
            read.stale.as_ref().map(|stale| stale.message.as_str()),
            Some("gh isn't signed in to github.com: run `gh auth login`")
        );
        assert_eq!(read.mirror.as_ref().unwrap().pr.head_oid.as_str(), second);
        assert_eq!(
            session.spec().unwrap().new,
            Side::Commit(pinned.head.clone())
        );
        assert!(session.error().is_none());
    });
}

#[gpui::test]
fn a_gh_that_fails_is_the_opens_banner(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let fixture = PrFixture::new(cx);
    fixture.answer(vec![fixture_gh::signed_out()]);
    let failure = fixture.open(&shell, cx).unwrap_err();
    // Named by its pull request: the banner may show over another one's review.
    assert_eq!(
        failure.message,
        "couldn't open #7: gh isn't signed in to github.com: run `gh auth login`"
    );
    assert!(failure.detail.as_ref().unwrap().contains("exit code 4"));
    shell.read_with(cx, |shell, cx| {
        let banner = shell.banner().expect("the open's banner");
        assert_eq!(banner.failure, failure);
        // Its Retry opens the pull request again.
        assert!(matches!(
            &banner.retry,
            Some((_, OpenRequest::Pr(PrRef::Hash(7))))
        ));
        assert!(shell.session().is_none());
        // The repository did open (the fixture's clone, a folder named `repo`): it reads as
        // any repository with no review open, and nothing says it can't be opened.
        let status = shell.status_line(cx);
        assert_eq!(status.location, "Local · repo · no review open");
        assert_eq!(status.health, Health::Idle);
    });

    // What a retry can't fix offers none: another repository's pull request.
    let location = fixture.location();
    let elsewhere = PrRef::Short {
        owner: "acme".into(),
        repo: "web".into(),
        number: 3,
    };
    let opening = cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open(location, OpenRequest::Pr(elsewhere), window, cx)
        })
    });
    settle(cx, &shell, "the refused open", |shell, _| {
        shell.opening().is_none()
    });
    let refused = cx.foreground_executor().block_test(opening).unwrap_err();
    assert_eq!(
        refused.message,
        "couldn't open #3: #3 is in acme/web; open it from a clone of that repository"
    );
    shell.read_with(cx, |shell, cx| {
        let banner = shell.banner().expect("the refusal's banner");
        assert_eq!(banner.failure, refused);
        assert!(banner.retry.is_none());
        assert_eq!(
            shell.status_line(cx).location,
            "Local · repo · no review open"
        );
    });
    let location = fixture.location();
    let opening = cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open(location, OpenRequest::Pr(PrRef::Hash(7)), window, cx)
        })
    });
    settle(cx, &shell, "the open that fails again", |shell, _| {
        shell.opening().is_none()
    });
    cx.foreground_executor().block_test(opening).unwrap_err();

    // Signed in again, Retry opens it.
    fixture.answer(Vec::new());
    cx.update(|window, cx| shell.update(cx, |shell, cx| shell.retry(window, cx)));
    settle(cx, &shell, "the retry", |shell, _| {
        shell.opening().is_none() && shell.session().is_some()
    });
    shell.read_with(cx, |shell, cx| {
        assert!(shell.banner().is_none());
        assert_eq!(
            shell.session().unwrap().read(cx).target_label(),
            "#7 Add the answer"
        );
    });
}

#[gpui::test]
fn the_apps_own_gh_is_no_gh_in_a_test_or_an_automation_run(cx: &mut TestAppContext) {
    // `SetupConfig::sandboxed` without `DV_GH`: nothing automated reaches the real gh.
    if crate::services::setup::names_gh(std::env::var_os("DV_GH").as_deref()) {
        return;
    }
    let (_dir, shell, cx) = open_shell(cx);
    let fixture = PrFixture::without_gh(cx);
    let failure = fixture.open(&shell, cx).unwrap_err();
    assert!(
        failure
            .message
            .starts_with("couldn't open #7: gh isn't installed"),
        "{}",
        failure.message
    );
    shell.read_with(cx, |shell, _| assert!(shell.banner().is_some()));
}

#[gpui::test]
fn a_refresh_elsewhere_is_followed_once_its_pins_move(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let mut fixture = PrFixture::new(cx);
    fixture.open(&shell, cx).unwrap();
    let session = shell.read_with(cx, |shell, _| shell.session().cloned().unwrap());
    let first = fixture.pr.head.clone();
    let second = fixture.push("pub fn answer() -> u32 {\n    43\n}\n");

    // Another process's refresh, stopped between its two writes (PLAN §9.5): the new head
    // fetched and the mirror written for it, the pins not moved yet.
    fixture.github.checkout().git(&[
        "fetch",
        "--no-write-fetch-head",
        "origin",
        "+refs/pull/7/head:refs/dv/pr/dv-fixture/api/7/head",
    ]);
    let (backend, id) = session.read_with(cx, |session, _| {
        (session.backend().clone(), session.review_id().clone())
    });
    let mut mirror = session.read_with(cx, |session, _| {
        dv_core::github::Mirror::clone(session.pr().unwrap().mirror.as_ref().unwrap())
    });
    mirror.pr.head_oid = second.parse().unwrap();
    mirror.read_at += 1;
    dv_core::github::MirrorStore::new(backend.clone())
        .write(&mirror)
        .unwrap();
    // The session reads the mirror and waits for the pins: no clock runs here, so it waits
    // until the pins move.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        cx.run_until_parked();
        if !session.read_with(cx, |session, _| session.is_settled()) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the mirror's change never reached the session"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(sides(&shell, cx).1, first);

    let repo = GitRepo::new(backend);
    let pinned = pins::read(&repo, &id).unwrap().unwrap();
    let moved = Pins {
        head: second.parse().unwrap(),
        base: pinned.base.clone(),
    };
    pins::advance(&repo, &id, Some(&pinned), &moved).unwrap();
    settle(cx, &shell, "the moved pins", |shell, cx| {
        let session = shell.session().unwrap().read(cx);
        session
            .spec()
            .is_some_and(|spec| spec.new == Side::Commit(moved.head.clone()))
            && session.pr().is_some_and(|read| {
                read.pins.as_ref() == Some(&moved)
                    && read.moved_from.as_ref() == Some(&pinned)
                    && read.mirror.as_ref().is_some_and(|shown| **shown == mirror)
            })
    });
}

#[gpui::test]
fn a_review_that_is_here_opens_under_a_banner_when_github_cant_be_read(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let fixture = PrFixture::new(cx);
    let opened = fixture.open(&shell, cx).unwrap();
    let head = fixture.pr.head.clone();

    fixture.answer(vec![fixture_gh::signed_out()]);
    let again = fixture.open(&shell, cx).unwrap();
    assert!(!again.created);
    assert_eq!(again.review, opened.review);
    let signed_out = "gh isn't signed in to github.com: run `gh auth login`";
    shell.read_with(cx, |shell, cx| {
        // The banner says why, and its Retry asks GitHub again.
        let banner = shell.banner().expect("the unread pull request's banner");
        assert_eq!(
            banner.failure.message,
            format!("couldn't read #7 from GitHub: {signed_out}")
        );
        assert!(matches!(
            &banner.retry,
            Some((_, OpenRequest::Pr(PrRef::Hash(7))))
        ));
        // The review as it was last read: its mirror, stale, and its sides.
        let session = shell.session().expect("the review").read(cx);
        let read = session.pr().unwrap();
        assert_eq!(
            read.stale.as_ref().map(|stale| stale.message.as_str()),
            Some(signed_out)
        );
        assert_eq!(read.mirror.as_ref().unwrap().pr.title, "Add the answer");
        assert_eq!(read.pins.as_ref().unwrap().head.as_str(), head);
    });
    assert_eq!(sides(&shell, cx).1, head);

    // A refresh that found no mirror at all leaves the one shown.
    let session = shell.read_with(cx, |shell, _| shell.session().cloned().unwrap());
    session.update(cx, |session, cx| {
        let pins = session.pr().unwrap().pins.clone();
        session.apply_pr_read(
            crate::services::github::PrRead {
                stale: Some(Failure::new("nothing could be read")),
                pins,
                ..Default::default()
            },
            cx,
        );
    });
    session.read_with(cx, |session, _| {
        let read = session.pr().unwrap();
        assert_eq!(read.mirror.as_ref().unwrap().pr.title, "Add the answer");
        assert_eq!(
            read.stale.as_ref().map(|stale| stale.message.as_str()),
            Some("nothing could be read")
        );
    });

    // Signed in again, Retry reads it.
    fixture.answer(Vec::new());
    cx.update(|window, cx| shell.update(cx, |shell, cx| shell.retry(window, cx)));
    settle(cx, &shell, "the retry", |shell, _| {
        shell.opening().is_none()
    });
    shell.read_with(cx, |shell, cx| {
        assert!(shell.banner().is_none());
        let session = shell.session().unwrap().read(cx);
        assert_eq!(session.pr().unwrap().stale, None);
    });
}
