//! The picker's lists as the service reads them (PLAN §9.9), against the fixture GitHub's
//! clone and `mock_gh`: one read of a list at a time, what a failed read keeps, and what a
//! clone that left GitHub or became another repository drops.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use dv_core::github::{Gh, GhOptions, PrRef};
use dv_core::{LocalBackend, LocalBackendOptions};
use dv_test_support::{FixtureGithub, fixture_gh, mock_gh};
use gpui::{Entity, TestAppContext};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

use super::*;

/// The fixture GitHub's clone of `dv-fixture/api`, and a service whose `gh` is `mock_gh`
/// answering from a scenario file, every call logged.
struct Lists {
    github: FixtureGithub,
    service: Entity<GitHub>,
    scenario: PathBuf,
    /// How many times a read asked for the repository's backend.
    reached: Rc<Cell<usize>>,
    _dir: tempfile::TempDir,
}

impl Lists {
    fn new(cx: &mut TestAppContext) -> Self {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let scenario = dir.path().join("scenario.json");
        let mut options = GhOptions::new(mock_gh().into_os_string());
        options.env = vec![
            ("DV_MOCK_GH".into(), Some(scenario.clone().into_os_string())),
            (
                "DV_MOCK_GH_LOG".into(),
                Some(scenario.with_file_name("calls.jsonl").into_os_string()),
            ),
        ];
        let lists = Self {
            github: FixtureGithub::new("api"),
            service: cx.new(|_| GitHub::with_gh(Gh::new(options))),
            scenario,
            reached: Rc::default(),
            _dir: dir,
        };
        lists.answer(open_prs());
        lists
    }

    fn answer(&self, calls: Vec<Value>) {
        std::fs::write(&self.scenario, json!({ "calls": calls }).to_string()).unwrap();
    }

    fn location(&self) -> Location {
        Location::Local(self.github.checkout().path().to_path_buf())
    }

    /// Asks for the list to be read again, the repository's backend opened when the read
    /// asks for it.
    fn ask(&self, cx: &mut TestAppContext) {
        let checkout = self.github.checkout();
        let (path, env) = (checkout.path().to_path_buf(), checkout.env_overlay());
        let reached = self.reached.clone();
        let location = self.location();
        self.service.update(cx, |service, cx| {
            service.refresh_list(
                location,
                move |_| {
                    reached.set(reached.get() + 1);
                    let backend = LocalBackend::open_with(&path, LocalBackendOptions { env })
                        .map(|backend| Arc::new(backend) as Arc<dyn Backend>)
                        .map_err(|error| Failure::new(error.to_string()));
                    Task::ready(backend)
                },
                cx,
            )
        });
    }

    /// Waits for the read that runs and gives the list as it then stands.
    fn landed(&self, cx: &mut TestAppContext) -> PrList {
        let location = self.location();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            cx.run_until_parked();
            let listing = self
                .service
                .read_with(cx, |service, _| service.is_listing(&location));
            if !listing {
                break;
            }
            assert!(Instant::now() < deadline, "the list was never read");
            std::thread::sleep(Duration::from_millis(10));
        }
        self.service.read_with(cx, |service, _| {
            service.list(&location).cloned().expect("a list")
        })
    }

    fn read(&self, cx: &mut TestAppContext) -> PrList {
        self.ask(cx);
        self.landed(cx)
    }

    /// How many `gh pr list` calls were made so far.
    fn list_calls(&self) -> usize {
        std::fs::read_to_string(self.scenario.with_file_name("calls.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|call| call["argv"][0] == "pr" && call["argv"][1] == "list")
            .count()
    }
}

/// `gh`'s answers for two open pull requests, of which 7 asks for the viewer's review.
fn open_prs() -> Vec<Value> {
    let seven = fixture_gh::listed(
        7,
        "Add the answer",
        "octocat",
        false,
        "2026-10-19T08:30:00Z",
    );
    fixture_gh::pr_list(
        vec![
            fixture_gh::listed(
                12,
                "Cache the config",
                "hubot",
                false,
                "2026-10-20T08:30:00Z",
            ),
            seven.clone(),
        ],
        vec![seven],
    )
}

fn numbers(list: &PrList) -> Option<Vec<(u64, bool)>> {
    list.prs.as_ref().map(|prs| {
        prs.iter()
            .map(|pr| (pr.number, pr.review_requested))
            .collect()
    })
}

fn kind(list: &PrList) -> Option<ListFailureKind> {
    list.failure.as_ref().map(|failure| failure.kind)
}

/// One read of a repository's list at a time: asked for again while one runs (the picker
/// closed and opened, `Ctrl G` held down), nothing starts, the repository isn't even
/// reached for, and `gh` is asked once.
#[gpui::test]
fn a_list_asked_for_while_it_is_read_is_read_once(cx: &mut TestAppContext) {
    let lists = Lists::new(cx);
    for _ in 0..5 {
        lists.ask(cx);
    }
    assert_eq!(lists.reached.get(), 1);
    let location = lists.location();
    lists.service.read_with(cx, |service, _| {
        assert!(service.is_listing(&location));
        assert!(!service.is_settled());
    });
    let list = lists.landed(cx);
    assert_eq!(numbers(&list), Some(vec![(7, true), (12, false)]));
    assert!(!list.loading);
    // The pair of calls a list is: the ones asking for a review, and the open ones.
    assert_eq!(lists.list_calls(), 2);
    assert!(
        lists
            .service
            .read_with(cx, |service, _| service.is_settled())
    );

    // Once it landed, asking reads again.
    lists.read(cx);
    assert_eq!((lists.reached.get(), lists.list_calls()), (2, 4));
}

/// A read that fails keeps the list read before, with when it was read, and says why; the
/// next one that works takes the failure away.
#[gpui::test]
fn a_failed_read_keeps_the_list_read_before(cx: &mut TestAppContext) {
    let lists = Lists::new(cx);
    let first = lists.read(cx);
    assert_eq!(kind(&first), None);
    let read_at = first.read_at.expect("when it was read");

    lists.answer(vec![fixture_gh::signed_out()]);
    let failed = lists.read(cx);
    assert_eq!(kind(&failed), Some(ListFailureKind::SignedOut));
    assert_eq!(
        failed.failure.as_ref().unwrap().failure.message,
        "gh isn't signed in to github.com: run `gh auth login`"
    );
    assert_eq!(failed.prs, first.prs);
    assert_eq!(failed.read_at, Some(read_at));
    assert_eq!(failed.repo, first.repo);

    // A repository that can't be reached where it lives keeps it too.
    let location = lists.location();
    lists.service.update(cx, |service, cx| {
        service.refresh_list(
            location,
            |_| Task::ready(Err(Failure::new("Ubuntu isn't running"))),
            cx,
        )
    });
    let unreachable = lists.landed(cx);
    assert_eq!(kind(&unreachable), Some(ListFailureKind::Unreachable));
    assert_eq!(unreachable.prs, first.prs);

    lists.answer(open_prs());
    let again = lists.read(cx);
    assert_eq!(kind(&again), None);
    assert_eq!(again.prs, first.prs);
    assert!(again.read_at >= Some(read_at));
}

/// A read that can't tell which pull requests ask for the viewer's review lists the open
/// ones unmarked and says why; when the list of the open ones fails, the read failed.
#[gpui::test]
fn a_list_whose_review_requests_cant_be_read_is_unmarked(cx: &mut TestAppContext) {
    let lists = Lists::new(cx);
    let mut calls = open_prs();
    calls[0] = json!({
        "argv": ["pr", "list", "--search", "**"],
        "stderr": "HTTP 422: Validation Failed\n",
        "exit": 1,
    });
    lists.answer(calls);
    let list = lists.read(cx);
    assert_eq!(kind(&list), None);
    assert_eq!(numbers(&list), Some(vec![(12, false), (7, false)]));
    let unmarked = list.unmarked.expect("why none is marked");
    assert!(unmarked.message.contains("422"), "{unmarked:?}");

    lists.answer(open_prs());
    let list = lists.read(cx);
    assert_eq!(list.unmarked, None);
    assert_eq!(numbers(&list), Some(vec![(7, true), (12, false)]));

    let mut calls = open_prs();
    calls[1] = json!({
        "argv": ["pr", "list", "--json", "**"],
        "stderr": "HTTP 502: Bad Gateway\n",
        "exit": 1,
    });
    lists.answer(calls);
    let list = lists.read(cx);
    assert_eq!(kind(&list), Some(ListFailureKind::Other));
    // The list read before stays, marked as it was.
    assert_eq!(numbers(&list), Some(vec![(7, true), (12, false)]));
    assert_eq!(list.unmarked, None);
}

/// A clone that is no longer on GitHub has no list: the rows read before aren't shown
/// under a repository the clone no longer is.
#[gpui::test]
fn a_clone_that_left_github_has_no_list(cx: &mut TestAppContext) {
    let lists = Lists::new(cx);
    let first = lists.read(cx);
    assert!(first.prs.is_some() && first.repo.is_some());

    lists.github.checkout().git(&["remote", "remove", "origin"]);
    let gone = lists.read(cx);
    assert_eq!(kind(&gone), Some(ListFailureKind::NotOnGithub));
    assert_eq!((gone.prs, gone.read_at, gone.repo), (None, None, None));
    // A number alone is all a row could name now.
    assert_eq!(gone.unmarked, None);
}

/// A clone whose remote names another repository than the one listed: the rows read
/// before go, whether the read of the new one works or fails, and a row's reference names
/// the repository its list is of.
#[gpui::test]
fn a_clone_that_became_another_repository_drops_the_rows_read_before(cx: &mut TestAppContext) {
    let lists = Lists::new(cx);
    let first = lists.read(cx);
    assert_eq!(
        first.reference(7),
        PrRef::Url {
            host: "github.com".to_string(),
            owner: "dv-fixture".to_string(),
            repo: "api".to_string(),
            number: 7,
        }
    );

    lists.github.checkout().git(&[
        "remote",
        "set-url",
        "origin",
        "https://github.com/acme/other.git",
    ]);
    // The read of the new repository fails: nothing of the old one shows under it.
    lists.answer(vec![fixture_gh::signed_out()]);
    let failed = lists.read(cx);
    assert_eq!(kind(&failed), Some(ListFailureKind::SignedOut));
    assert_eq!((failed.prs.clone(), failed.read_at), (None, None));
    let repo = failed
        .repo
        .as_ref()
        .expect("the repository the clone is now");
    assert_eq!((repo.owner.as_str(), repo.repo.as_str()), ("acme", "other"));
    assert_eq!(
        failed.reference(7),
        PrRef::Url {
            host: "github.com".to_string(),
            owner: "acme".to_string(),
            repo: "other".to_string(),
            number: 7,
        }
    );

    // And once it works, the list is the new repository's.
    lists.answer(fixture_gh::pr_list(
        vec![fixture_gh::listed(
            3,
            "Another repository's",
            "mona",
            false,
            "2026-10-21T08:30:00Z",
        )],
        Vec::new(),
    ));
    let other = lists.read(cx);
    assert_eq!(kind(&other), None);
    assert_eq!(numbers(&other), Some(vec![(3, false)]));

    // Back to the first repository with a `gh` that fails: the other one's rows go too.
    let url = lists.github.url();
    lists
        .github
        .checkout()
        .git(&["remote", "set-url", "origin", &url]);
    lists.answer(vec![fixture_gh::signed_out()]);
    let back = lists.read(cx);
    assert_eq!((back.prs, back.read_at), (None, None));
    assert_eq!(back.repo.map(|repo| repo.repo), Some("api".to_string()));
}

/// Before any read told which repository the clone is, a row's reference is its number.
#[test]
fn a_list_of_no_known_repository_names_a_pull_request_by_its_number() {
    assert_eq!(PrList::default().reference(7), PrRef::Hash(7));
}
