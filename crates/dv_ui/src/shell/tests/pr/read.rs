//! The pull request in the app (PLAN §9.9, DESIGN §4.5), against the fixture GitHub and
//! `mock_gh`: the strip and its failed refresh, `Ctrl R`, the PR picker and its states, the
//! palette's Pull request section with the folder chosen for an unknown repository, and
//! that nothing typed asks `gh` anything.

use std::time::Duration;

use dv_core::github::PrState;
use dv_core::review::ReviewId;
use dv_test_support::FixtureRepo;
use pretty_assertions::assert_eq;

use super::*;
use crate::palette::{RowKind as PaletteRow, Section};
use crate::pr_picker::{ListState, RowKind as PickerRow};
use crate::services::github::ListFailureKind;
use crate::session::{Commits, MoveKind};
use crate::shell::pr_strip::{PrStrip, ReadLine};
use crate::system::{self, SystemRequest};
use crate::test_support::add_repo;

pub(super) fn strip(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> PrStrip {
    shell.read_with(cx, |shell, cx| shell.pr_strip(cx).expect("the PR strip"))
}

/// Waits for whatever the shell and its session still run.
pub(super) fn idle(shell: &Entity<Shell>, cx: &mut VisualTestContext, what: &str) {
    settle(cx, shell, what, |_, _| true);
}

/// The open pull requests the fixture's `gh` lists: 12, 9 (a draft) and 7, of which 7 asks
/// for the viewer's review.
pub(super) fn listed() -> Vec<Value> {
    fixture_gh::pr_list(
        vec![
            fixture_gh::listed(
                12,
                "Cache the config",
                "hubot",
                false,
                "2026-10-20T08:30:00Z",
            ),
            fixture_gh::listed(
                9,
                "Stream the body",
                "octocat",
                true,
                "2026-10-19T09:00:00Z",
            ),
            fixture_gh::listed(
                7,
                "Add the answer",
                "octocat",
                false,
                "2026-10-19T08:30:00Z",
            ),
        ],
        vec![fixture_gh::listed(
            7,
            "Add the answer",
            "octocat",
            false,
            "2026-10-19T08:30:00Z",
        )],
    )
}

pub(super) fn picker_keys(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Vec<String> {
    shell.read_with(cx, |shell, cx| {
        let picker = shell.pr_picker().expect("the PR picker").read(cx);
        picker.rows().iter().map(|row| row.key.clone()).collect()
    })
}

pub(super) fn picker_state(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> ListState {
    shell.read_with(cx, |shell, cx| {
        shell
            .pr_picker()
            .expect("the PR picker")
            .read(cx)
            .list_state()
            .clone()
    })
}

pub(super) fn palette_rows(
    shell: &Entity<Shell>,
    cx: &mut VisualTestContext,
) -> Vec<(Section, String)> {
    shell.read_with(cx, |shell, cx| {
        let palette = shell.palette().expect("the palette").read(cx);
        palette
            .rows()
            .iter()
            .map(|row| (row.section, row.key.clone()))
            .collect()
    })
}

#[gpui::test]
fn the_strip_shows_the_pull_request_how_far_it_moved_and_a_refresh_that_failed(
    cx: &mut TestAppContext,
) {
    let (_dir, shell, cx) = open_shell(cx);
    let mut fixture = PrFixture::new(cx);
    fixture.pr.checks = json!([
        {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS"},
        {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "FAILURE"},
        {"__typename": "CheckRun", "status": "IN_PROGRESS", "conclusion": ""},
    ]);
    fixture.answer(Vec::new());
    shell.read_with(cx, |shell, cx| assert_eq!(shell.pr_strip(cx), None));
    let first = fixture.pr.head.clone();
    fixture.open(&shell, cx).unwrap();
    idle(&shell, cx, "the strip's facts");

    let shown = strip(&shell, cx);
    assert_eq!(shown.number, 7);
    assert_eq!(shown.title.as_deref(), Some("Add the answer"));
    assert_eq!(shown.author.as_deref(), Some("octocat"));
    assert_eq!(shown.branches.as_deref(), Some("main ← feat"));
    assert_eq!(shown.state, Some(PrState::Open));
    assert_eq!(
        (
            shown.checks.passing,
            shown.checks.failing,
            shown.checks.pending
        ),
        (1, 1, 1)
    );
    assert_eq!(shown.round, Some(1));
    assert_eq!(shown.moved, None);
    assert_eq!(shown.moved_text(), None);
    assert!(matches!(shown.read, ReadLine::Read(_)), "{:?}", shown.read);
    assert!(!shown.refreshing);
    // dv's own addresses, from the mirror's key.
    assert_eq!(
        shown.url.as_deref(),
        Some("https://github.com/dv-fixture/api/pull/7")
    );
    assert_eq!(
        shown.checks_url.as_deref(),
        Some("https://github.com/dv-fixture/api/pull/7/checks")
    );

    // A push, then `Ctrl R`: the refresh goes through the session's queue and the strip
    // says how far the review moved.
    let second = fixture.push("// The answer.\n\npub fn answer() -> u32 {\n    42\n}\n");
    cx.simulate_keystrokes("secondary-r");
    idle(&shell, cx, "the refresh");
    let shown = strip(&shell, cx);
    assert!(!shown.refreshing);
    let moved = shown.moved.clone().expect("the review moved");
    assert_eq!(moved.kind, MoveKind::Followed);
    assert_eq!(
        (moved.from.as_str(), moved.to.as_str()),
        (&*first, &*second)
    );
    assert_eq!(moved.commits, Some(Commits::New(1)));
    assert_eq!(
        shown.moved_text().as_deref(),
        Some("Updated to 1 new commit")
    );

    // A refresh that fails: the strip says so, keeps what was read, and shows the failure
    // whole once asked.
    fixture.answer(vec![fixture_gh::signed_out()]);
    shell.update(cx, |shell, cx| shell.refresh_pull_request(cx));
    idle(&shell, cx, "the failed refresh");
    let shown = strip(&shell, cx);
    let ReadLine::Stale(stale) = &shown.read else {
        panic!("not stale: {:?}", shown.read);
    };
    assert_eq!(
        stale.message,
        "gh isn't signed in to github.com: run `gh auth login`"
    );
    assert_eq!(shown.read.text(0), "Couldn't refresh · details");
    assert_eq!(shown.title.as_deref(), Some("Add the answer"));
    assert!(shown.url.is_some());
    shell.read_with(cx, |shell, cx| assert!(!shell.pr_error_open(cx)));
    shell.update(cx, |shell, cx| shell.toggle_pr_error(cx));
    shell.read_with(cx, |shell, cx| assert!(shell.pr_error_open(cx)));
    cx.run_until_parked();

    // Signed in again: the next refresh clears it, and the failure's banner goes with it.
    fixture.answer(Vec::new());
    cx.simulate_keystrokes("secondary-r");
    idle(&shell, cx, "the refresh that works");
    let shown = strip(&shell, cx);
    assert!(matches!(shown.read, ReadLine::Read(_)), "{:?}", shown.read);
    shell.read_with(cx, |shell, cx| assert!(!shell.pr_error_open(cx)));

    // A closed review keeps its head: once the pull request moves on, GitHub's head and
    // the review's are two commits, and the strip counts what lies between them.
    let session = shell.read_with(cx, |shell, _| shell.session().cloned().unwrap());
    let (store, id, reviewer) = session.read_with(cx, |session, _| {
        (
            session.store().clone(),
            session.review_id().clone(),
            session.reviewer().clone(),
        )
    });
    dv_core::review::ops::set_status(&store, &id, ReviewStatus::Closed, &reviewer, None).unwrap();
    settle(cx, &shell, "the closed review", |shell, cx| {
        shell.session().unwrap().read(cx).is_read_only()
    });
    fixture.push("// The answer.\n\npub fn answer() -> u32 {\n    43\n}\n");
    fixture.push("// The answer.\n\npub fn answer() -> u32 {\n    44\n}\n");
    shell.update(cx, |shell, cx| shell.refresh_pull_request(cx));
    idle(&shell, cx, "the refresh of a closed review");
    let shown = strip(&shell, cx);
    let moved = shown.moved.clone().expect("the pull request moved on");
    assert_eq!(moved.kind, MoveKind::Ahead);
    assert_eq!(moved.from.as_str(), second);
    assert_eq!(moved.commits, Some(Commits::New(2)));
    assert_eq!(
        shown.moved_text().as_deref(),
        Some("2 new commits since this review's head")
    );
    assert!(!shown.draft);
}

#[gpui::test]
fn what_github_wrote_is_drawn_as_one_line_wherever_it_names_the_pull_request(
    cx: &mut TestAppContext,
) {
    let (_dir, shell, cx) = open_shell(cx);
    let mut fixture = PrFixture::new(cx);
    fixture.pr.title = " Fix\nthe \u{202e}ca\tche ".to_string();
    fixture.pr.author = Some("octo\u{2066}cat".to_string());
    fixture.pr.head_branch = "feat\u{202e}x".to_string();
    fixture.pr.fork = Some(Some("mona".to_string()));
    fixture.answer(Vec::new());
    add_repo(fixture.github.checkout(), &shell, cx);
    fixture.open(&shell, cx).unwrap();
    idle(&shell, cx, "the review");

    let shown = strip(&shell, cx);
    let title = shown.title.clone().unwrap();
    assert_eq!(title, "Fix\\nthe \\u{202e}ca\tche");
    assert_eq!(shown.author.as_deref(), Some("octo\\u{2066}cat"));
    // A fork's head is named by its owner.
    assert_eq!(
        shown.branches.as_deref(),
        Some("main ← mona:feat\\u{202e}x")
    );
    shell.read_with(cx, |shell, cx| {
        let session = shell.session().unwrap().read(cx);
        assert_eq!(session.target_label(), format!("#7 {title}"));
        // The stream's side labels are its branches, drawn the same way.
        assert_eq!(
            session.side_labels(),
            ("main".to_string(), "feat\\u{202e}x".to_string())
        );
        assert_eq!(session.target_side_labels(), session.side_labels());
    });
    // The switcher's row names it the same way.
    cx.simulate_keystrokes("secondary-o");
    idle(&shell, cx, "the switcher's rows");
    shell.read_with(cx, |shell, cx| {
        let switcher = shell.switcher().expect("the switcher").read(cx);
        let item = switcher
            .listing()
            .items
            .iter()
            .find(|item| item.kind_key() == "pr")
            .expect("the PR review's row");
        assert_eq!(item.label(), format!("#7 {title}"));
    });
}

#[gpui::test]
fn ctrl_g_lists_the_repositorys_pull_requests_and_typing_asks_gh_nothing(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let fixture = PrFixture::logged(cx);
    fixture.answer(listed());
    add_repo(fixture.github.checkout(), &shell, cx);
    // The repository in view is the open review's: a working review of the clone.
    let location = fixture.location();
    let opening = cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open(
                location.clone(),
                OpenRequest::Target(Target::Working),
                window,
                cx,
            )
        })
    });
    idle(&shell, cx, "the working review");
    cx.foreground_executor().block_test(opening).unwrap();
    assert_eq!(fixture.gh_calls(), Vec::<Vec<String>>::new());
    shell.read_with(cx, |shell, cx| {
        assert_eq!(shell.repo_in_view(cx), Some(location.clone()));
        assert_eq!(shell.pr_strip(cx), None, "a local review has no strip");
    });

    cx.simulate_keystrokes("secondary-g");
    idle(&shell, cx, "the list");
    assert_eq!(picker_state(&shell, cx), ListState::Listed);
    // The one that asks for the viewer's review first.
    assert_eq!(picker_keys(&shell, cx), ["pr/7", "pr/12", "pr/9"]);
    shell.read_with(cx, |shell, cx| {
        let picker = shell.pr_picker().unwrap().read(cx);
        let list = picker.list(cx).unwrap();
        assert_eq!(
            list.repo.as_ref().map(|repo| repo.name_with_owner()),
            Some("dv-fixture/api".to_string())
        );
        assert!(!list.loading);
        let PickerRow::Listed { pr, .. } = &picker.rows()[0].kind else {
            panic!("not a listed row");
        };
        assert!(pr.review_requested);
    });
    let listing = fixture.gh_calls();
    assert_eq!(listing.len(), 2, "{listing:?}");
    assert!(listing.iter().all(|argv| argv[..2] == ["pr", "list"]));

    // Typing filters what is held; a number offers the pull request itself first.
    cx.simulate_input("stream");
    assert_eq!(picker_keys(&shell, cx), ["pr/9"]);
    cx.simulate_keystrokes("backspace backspace backspace backspace backspace backspace");
    cx.simulate_input("nothing like it");
    assert_eq!(picker_keys(&shell, cx), Vec::<String>::new());
    assert_eq!(picker_state(&shell, cx), ListState::NoMatch);
    for _ in 0.."nothing like it".len() {
        cx.simulate_keystrokes("backspace");
    }
    cx.simulate_input("#1");
    assert_eq!(picker_keys(&shell, cx), ["open/repo", "pr/12"]);
    cx.simulate_keystrokes("backspace backspace");
    cx.simulate_input("https://github.com/acme/web/pull/3");
    assert_eq!(picker_keys(&shell, cx), ["choose_folder"]);
    for _ in 0.."https://github.com/acme/web/pull/3".len() {
        cx.simulate_keystrokes("backspace");
    }
    idle(&shell, cx, "the typing");
    assert_eq!(fixture.gh_calls().len(), 2, "typing asked gh");

    // Enter opens the selected pull request in the repository in view.
    assert_eq!(picker_keys(&shell, cx), ["pr/7", "pr/12", "pr/9"]);
    cx.simulate_keystrokes("enter");
    settle(cx, &shell, "the pull request", |shell, cx| {
        shell
            .session()
            .is_some_and(|session| session.read(cx).review().is_pr_review())
    });
    shell.read_with(cx, |shell, cx| {
        assert!(shell.pr_picker().is_none());
        assert!(shell.banner().is_none(), "{:?}", shell.banner());
        assert_eq!(
            shell.session().unwrap().read(cx).target_label(),
            "#7 Add the answer"
        );
    });

    // Opened again, the picker shows the list it read before while it reads it again.
    let before = fixture.gh_calls().len();
    cx.simulate_keystrokes("secondary-g");
    assert_eq!(picker_state(&shell, cx), ListState::Listed);
    assert_eq!(picker_keys(&shell, cx), ["pr/7", "pr/12", "pr/9"]);
    idle(&shell, cx, "the list read again");
    assert_eq!(fixture.gh_calls().len(), before + 2);
    // `Ctrl G` again closes it.
    cx.simulate_keystrokes("secondary-g");
    shell.read_with(cx, |shell, _| assert!(shell.pr_picker().is_none()));
}

#[gpui::test]
fn the_picker_says_why_it_has_no_list_and_its_field_still_opens(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);

    // No repository in view.
    cx.simulate_keystrokes("secondary-g");
    assert_eq!(picker_state(&shell, cx), ListState::NoRepository);
    cx.simulate_input("#7");
    assert_eq!(picker_keys(&shell, cx), Vec::<String>::new());
    cx.simulate_keystrokes("escape");
    shell.read_with(cx, |shell, _| assert!(shell.pr_picker().is_none()));

    // A repository that isn't on GitHub.
    let plain = crate::test_support::fixture();
    add_repo(&plain, &shell, cx);
    let opening = cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open(
                Location::Local(plain.path().to_path_buf()),
                OpenRequest::Target(Target::Working),
                window,
                cx,
            )
        })
    });
    idle(&shell, cx, "the local review");
    cx.foreground_executor().block_test(opening).unwrap();
    cx.simulate_keystrokes("secondary-g");
    idle(&shell, cx, "the list that isn't");
    let ListState::Failed(failed) = picker_state(&shell, cx) else {
        panic!("a repository without GitHub listed something");
    };
    assert_eq!(failed.kind, ListFailureKind::NotOnGithub);
    assert_eq!(failed.failure.message, "this repository has no remotes");
    cx.simulate_keystrokes("escape");

    // gh signed out: the reason and what to do, and a reference still opens.
    let fixture = PrFixture::new(cx);
    fixture.answer(vec![fixture_gh::signed_out()]);
    add_repo(fixture.github.checkout(), &shell, cx);
    let opening = cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open(
                fixture.location(),
                OpenRequest::Target(Target::Working),
                window,
                cx,
            )
        })
    });
    idle(&shell, cx, "the clone's review");
    cx.foreground_executor().block_test(opening).unwrap();
    cx.simulate_keystrokes("secondary-g");
    idle(&shell, cx, "the list gh refuses");
    let ListState::Failed(failed) = picker_state(&shell, cx) else {
        panic!("a signed-out gh listed something");
    };
    assert_eq!(failed.kind, ListFailureKind::SignedOut);
    assert_eq!(
        failed.failure.message,
        "gh isn't signed in to github.com: run `gh auth login`"
    );
    cx.simulate_input("7");
    assert_eq!(
        picker_keys(&shell, cx),
        [format!("open/{}", fixture.location().display_name())]
    );
    cx.simulate_keystrokes("escape");

    // gh missing.
    let service = cx.update(|_, cx| cx.services().github.clone());
    service.update(cx, |service, _| *service = GitHub::without_gh());
    cx.simulate_keystrokes("secondary-g");
    idle(&shell, cx, "the list without gh");
    let ListState::Failed(failed) = picker_state(&shell, cx) else {
        panic!("no gh listed something");
    };
    assert_eq!(failed.kind, ListFailureKind::GhMissing);
    cx.simulate_keystrokes("escape");

    // No open pull requests.
    let fixture = PrFixture::new(cx);
    fixture.answer(fixture_gh::pr_list(Vec::new(), Vec::new()));
    register(fixture.github.checkout(), cx);
    let opening = cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open(
                fixture.location(),
                OpenRequest::Target(Target::Working),
                window,
                cx,
            )
        })
    });
    idle(&shell, cx, "another clone's review");
    cx.foreground_executor().block_test(opening).unwrap();
    cx.simulate_keystrokes("secondary-g");
    idle(&shell, cx, "the empty list");
    assert_eq!(picker_state(&shell, cx), ListState::Empty);
}

#[gpui::test]
fn a_pasted_reference_is_the_palettes_first_row_without_asking_gh(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let fixture = PrFixture::logged(cx);
    add_repo(fixture.github.checkout(), &shell, cx);
    let plain = crate::test_support::fixture();
    add_repo(&plain, &shell, cx);
    // The clone's GitHub name was read beside its listing, and is remembered.
    let location = fixture.location();
    let reviews = cx.update(|_, cx| cx.services().reviews.clone());
    reviews.read_with(cx, |reviews, _| {
        let github = reviews.row(&location).unwrap().github.clone().unwrap();
        assert_eq!(
            (&*github.host, &*github.owner, &*github.repo),
            ("github.com", "dv-fixture", "api")
        );
        assert_eq!(
            reviews
                .known()
                .get(&location)
                .unwrap()
                .github
                .as_ref()
                .map(|known| &known.repo),
            Some(&github)
        );
        let plain = Location::Local(plain.path().to_path_buf());
        assert_eq!(reviews.row(&plain).unwrap().github, None);
    });

    // The local repository's review is open: another repository than the pull request's.
    let opening = cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open(
                Location::Local(plain.path().to_path_buf()),
                OpenRequest::Target(Target::Working),
                window,
                cx,
            )
        })
    });
    idle(&shell, cx, "the local review");
    cx.foreground_executor().block_test(opening).unwrap();

    cx.simulate_keystrokes("secondary-k");
    // A bare number is a search.
    cx.simulate_input("7");
    assert!(
        palette_rows(&shell, cx)
            .iter()
            .all(|(section, _)| *section != Section::PullRequest)
    );
    cx.simulate_keystrokes("backspace");
    // Its URL, as pasted from the browser: the known clone, first, whatever the case.
    let url = "https://GitHub.com/DV-Fixture/API/pull/7/files";
    cx.simulate_input(url);
    let rows = palette_rows(&shell, cx);
    // Two known repositories share the folder's name, so each is keyed by its path.
    let clone_key = format!("pr/{}", location.display_name());
    assert_eq!(rows[0], (Section::PullRequest, clone_key.clone()));
    assert_eq!(
        rows.iter()
            .filter(|(section, _)| *section == Section::PullRequest)
            .count(),
        1
    );
    shell.read_with(cx, |shell, cx| {
        let palette = shell.palette().unwrap().read(cx);
        assert_eq!(palette.selected_key(), Some(clone_key.as_str()));
        let PaletteRow::Pr { name, clone, .. } = &palette.rows()[0].kind else {
            panic!("not a pull request's row");
        };
        assert_eq!(name, "DV-Fixture/API#7");
        assert_eq!(*clone, None, "one clone needs no name");
    });
    for _ in 0..url.len() {
        cx.simulate_keystrokes("backspace");
    }
    // `#n` names the repository in view, which isn't on GitHub: the open says so.
    cx.simulate_input("#7");
    assert_eq!(
        palette_rows(&shell, cx)[0],
        (
            Section::PullRequest,
            format!("pr/{}", plain.path().display())
        )
    );
    cx.simulate_keystrokes("backspace backspace");
    cx.simulate_input("dv-fixture/api#7");
    idle(&shell, cx, "the typing");
    assert_eq!(
        fixture.gh_calls(),
        Vec::<Vec<String>>::new(),
        "typing asked gh"
    );

    // Enter opens it there: the window moves to that repository.
    cx.simulate_keystrokes("enter");
    settle(cx, &shell, "the pull request", |shell, cx| {
        shell
            .session()
            .is_some_and(|session| session.read(cx).review().is_pr_review())
    });
    shell.read_with(cx, |shell, cx| {
        assert!(shell.palette().is_none());
        assert!(shell.banner().is_none(), "{:?}", shell.banner());
        assert_eq!(shell.repo_in_view(cx), Some(location.clone()));
    });
    assert!(!fixture.gh_calls().is_empty());
}

#[gpui::test]
fn a_pull_request_of_an_unknown_repository_opens_through_its_chosen_folder(
    cx: &mut TestAppContext,
) {
    let (dir, shell, cx) = open_shell(cx);
    let fixture = PrFixture::new(cx);
    let location = fixture.location();
    let reviews = cx.update(|_, cx| cx.services().reviews.clone());
    let palette_notice = |shell: &Entity<Shell>, cx: &mut VisualTestContext| {
        shell.read_with(cx, |shell, cx| {
            shell
                .palette()
                .expect("the palette stays open")
                .read(cx)
                .notice()
                .map(str::to_string)
        })
    };
    let answer = |path: Option<PathBuf>, cx: &mut VisualTestContext| {
        cx.update(|_, cx| system::queue_answer(path, cx)).unwrap();
    };

    cx.simulate_keystrokes("secondary-k");
    cx.simulate_input("dv-fixture/api#7");
    assert_eq!(
        palette_rows(&shell, cx)[0],
        (Section::PullRequest, "pr/choose_folder".to_string())
    );
    shell.read_with(cx, |shell, cx| {
        let PaletteRow::ChooseFolder { repository, .. } =
            &shell.palette().unwrap().read(cx).rows()[0].kind
        else {
            panic!("not the folder's row");
        };
        assert_eq!(repository, "dv-fixture/api");
    });

    // The picker cancelled: nothing happens.
    answer(None, cx);
    cx.simulate_keystrokes("enter");
    idle(&shell, cx, "the cancelled picker");
    assert_eq!(palette_notice(&shell, cx), None);
    cx.update(|_, cx| {
        assert!(matches!(
            system::requests(cx).0.last(),
            Some(SystemRequest::PickFolder { prompt }) if prompt == "Choose its folder"
        ));
    });

    // A folder that is no repository.
    answer(Some(dir.path().to_path_buf()), cx);
    cx.simulate_keystrokes("enter");
    idle(&shell, cx, "the folder that is no repository");
    assert_eq!(
        palette_notice(&shell, cx),
        Some(format!("`{}` isn't a git repository", dir.path().display()))
    );

    // A repository without GitHub, and a clone of another repository.
    let plain = FixtureRepo::new();
    plain.write("a.txt", "a\n");
    plain.commit("first");
    register(&plain, cx);
    answer(Some(plain.path().to_path_buf()), cx);
    cx.simulate_keystrokes("enter");
    idle(&shell, cx, "the folder without GitHub");
    assert_eq!(
        palette_notice(&shell, cx),
        Some(format!(
            "`{}`: this repository has no remotes",
            plain.path().display()
        ))
    );
    let other = FixtureGithub::new("web");
    register(other.checkout(), cx);
    answer(Some(other.checkout().path().to_path_buf()), cx);
    cx.simulate_keystrokes("enter");
    idle(&shell, cx, "another repository's clone");
    assert_eq!(
        palette_notice(&shell, cx),
        Some(format!(
            "`{}` is dv-fixture/web, not dv-fixture/api",
            other.checkout().path().display()
        ))
    );
    // Nothing was added, and typing takes the reason away.
    reviews.read_with(cx, |reviews, _| assert_eq!(reviews.rows().len(), 0));
    cx.simulate_keystrokes("backspace");
    assert_eq!(palette_notice(&shell, cx), None);
    cx.simulate_input("7");

    // Its clone, through a subfolder: it becomes a known repository and the pull request
    // opens there.
    answer(Some(fixture.github.checkout().path().join("src")), cx);
    std::fs::create_dir_all(fixture.github.checkout().path().join("src")).unwrap();
    cx.simulate_keystrokes("enter");
    settle(cx, &shell, "the pull request", |shell, cx| {
        shell
            .session()
            .is_some_and(|session| session.read(cx).review().is_pr_review())
    });
    shell.read_with(cx, |shell, cx| {
        assert!(shell.palette().is_none());
        assert!(shell.banner().is_none(), "{:?}", shell.banner());
        assert_eq!(shell.repo_in_view(cx), Some(location.clone()));
    });
    reviews.read_with(cx, |reviews, _| {
        let rows = reviews.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].location, location);
        assert_eq!(
            rows[0]
                .github
                .as_ref()
                .map(|github| github.name_with_owner()),
            Some("dv-fixture/api".to_string())
        );
    });

    // Known now: the same reference names its clone.
    cx.simulate_keystrokes("secondary-k");
    cx.simulate_input("dv-fixture/api#7");
    assert_eq!(
        palette_rows(&shell, cx)[0],
        (Section::PullRequest, "pr/repo".to_string())
    );
}

/// A review of the fixture's pull request made outside the app (the CLI's `review new
/// --pr`), its mirror stored: one thread by `octo` on line 2.
pub(super) fn reviewed_elsewhere(fixture: &mut PrFixture, cx: &mut VisualTestContext) -> ReviewId {
    fixture.pr.threads = vec![super::threads::on_line(
        fixture,
        "PRRT_open",
        2,
        "octo",
        "Why 42?",
    )];
    fixture.answer(Vec::new());
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
    dv_core::github::open(&backend, &gh, &PrRef::Hash(7), || {
        dv_core::review::Author::reviewer("dv fixture")
    })
    .unwrap()
    .review
    .id
}

pub(super) fn open_review(
    fixture: &PrFixture,
    review: &ReviewId,
    shell: &Entity<Shell>,
    cx: &mut VisualTestContext,
) -> gpui::Task<Result<Opened, Failure>> {
    let (location, review) = (fixture.location(), review.clone());
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open(location, OpenRequest::Review(review), window, cx)
        })
    })
}

/// PLAN §9.4: a PR review opened from the switcher shows what was last read of its pull
/// request (the strip, the title, GitHub's threads) while its first refresh runs, and the
/// strip says it is reading.
#[gpui::test]
fn a_pr_review_opened_by_its_id_shows_its_stored_mirror_while_github_is_read(
    cx: &mut TestAppContext,
) {
    let (_dir, shell, cx) = open_shell(cx);
    let mut fixture = PrFixture::new(cx);
    let review = reviewed_elsewhere(&mut fixture, cx);
    // The pull request was renamed since; `gh` takes its time to say so.
    fixture.pr.title = "Add the answer, again".to_string();
    fixture.answer(Vec::new());
    let github = cx.update(|_, cx| cx.services().github.clone());
    let answer = github.update(cx, |github, _| github.hold());

    let opening = open_review(&fixture, &review, &shell, cx);
    // Everything but `gh`, which hasn't answered.
    for _ in 0..50 {
        cx.executor().advance_clock(Duration::from_millis(400));
        cx.run_until_parked();
        let placed = shell.read_with(cx, |shell, cx| {
            shell
                .session()
                .is_some_and(|session| !session.read(cx).github_threads().is_empty())
        });
        if placed {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let shown = strip(&shell, cx);
    assert_eq!(shown.title.as_deref(), Some("Add the answer"));
    assert_eq!(shown.branches.as_deref(), Some("main ← feat"));
    assert_eq!(shown.read, ReadLine::Reading);
    assert_eq!(shown.read.text(0), "Reading from GitHub…");
    assert!(shown.refreshing);
    shell.read_with(cx, |shell, cx| {
        let session = shell.session().unwrap().read(cx);
        let read = session.pr().expect("the stored mirror");
        assert!(read.is_stored());
        assert_eq!(session.target_label(), "#7 Add the answer");
        assert_eq!(session.github_threads().len(), 1);
        assert!(session.is_refreshing_pr());
    });

    // `gh` answers: the pull request as GitHub has it now.
    drop(answer);
    settle(cx, &shell, "the refresh", |shell, cx| {
        !shell.session().unwrap().read(cx).is_refreshing_pr()
    });
    cx.foreground_executor().block_test(opening).unwrap();
    let shown = strip(&shell, cx);
    assert_eq!(shown.title.as_deref(), Some("Add the answer, again"));
    assert!(matches!(shown.read, ReadLine::Read(_)), "{:?}", shown.read);
    shell.read_with(cx, |shell, cx| {
        let read = shell.session().unwrap().read(cx).pr().cloned().unwrap();
        assert!(read.fresh && !read.is_stored());
    });
}

/// PLAN §9.4: when that first refresh fails, the stored mirror stays, stale: the strip
/// says "Couldn't refresh" over the pull request as it was last read.
#[gpui::test]
fn a_pr_review_opened_by_its_id_keeps_its_stored_mirror_when_github_cant_be_read(
    cx: &mut TestAppContext,
) {
    let (_dir, shell, cx) = open_shell(cx);
    let mut fixture = PrFixture::new(cx);
    let review = reviewed_elsewhere(&mut fixture, cx);
    fixture.answer(vec![fixture_gh::signed_out()]);

    let opening = open_review(&fixture, &review, &shell, cx);
    settle(cx, &shell, "the review and its refresh", |shell, cx| {
        shell.session().is_some_and(|session| {
            let session = session.read(cx);
            session.pr().is_some() && !session.is_refreshing_pr()
        })
    });
    cx.foreground_executor().block_test(opening).unwrap();
    let shown = strip(&shell, cx);
    assert_eq!(shown.title.as_deref(), Some("Add the answer"));
    let ReadLine::Stale(failure) = &shown.read else {
        panic!("not stale: {:?}", shown.read);
    };
    assert_eq!(
        failure.message,
        "gh isn't signed in to github.com: run `gh auth login`"
    );
    assert_eq!(shown.read.text(0), "Couldn't refresh · details");
    shell.read_with(cx, |shell, cx| {
        let session = shell.session().unwrap().read(cx);
        assert_eq!(session.target_label(), "#7 Add the answer");
        assert_eq!(session.github_threads().len(), 1);
        // Opened by its id, the review shows under its strip alone: no banner.
        assert!(shell.banner().is_none());
    });
}

/// PLAN §9.3, DESIGN §4.5: one failed read, one banner. The open's "couldn't read #7 from
/// GitHub" is the review's failed read whole, so the strip's "details" shows no second
/// banner beside it; and it goes once GitHub is read, whoever asked.
#[gpui::test]
fn a_failed_read_shows_in_one_banner_which_goes_once_github_is_read(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let fixture = PrFixture::new(cx);
    let review = fixture.open(&shell, cx).unwrap().review;
    let banners = |shell: &Entity<Shell>, cx: &mut VisualTestContext| {
        cx.update(|_, cx| {
            shell.update(cx, |shell, cx| {
                (
                    shell.banner().map(|banner| banner.failure.message.clone()),
                    shell.pr_banner(cx).is_some(),
                    shell.pr_error_open(cx),
                )
            })
        })
    };
    let unread = "couldn't read #7 from GitHub: gh isn't signed in to github.com: run `gh auth \
                  login`";

    // Pasted again while `gh` is signed out: the open's banner, and the strip's line.
    fixture.answer(vec![fixture_gh::signed_out()]);
    fixture.open(&shell, cx).unwrap();
    assert!(matches!(strip(&shell, cx).read, ReadLine::Stale(_)));
    shell.read_with(cx, |shell, _| {
        let about = shell.banner().unwrap().unread.as_ref().unwrap();
        assert_eq!(about.review, review);
    });
    // It stands for the strip's details: shown as open, and no banner of the strip's.
    assert_eq!(banners(&shell, cx), (Some(unread.to_string()), false, true));
    // "details" hides the one that is up; again, and the strip's own shows, alone.
    shell.update(cx, |shell, cx| shell.toggle_pr_error(cx));
    assert_eq!(banners(&shell, cx), (None, false, false));
    shell.update(cx, |shell, cx| shell.toggle_pr_error(cx));
    assert_eq!(banners(&shell, cx), (None, true, true));
    // Pasted once more with the strip's banner up: the open's takes its place.
    fixture.open(&shell, cx).unwrap();
    assert_eq!(banners(&shell, cx), (Some(unread.to_string()), false, true));

    // GitHub fails another way under `Ctrl R`: the banner that said `gh` is signed out goes
    // with that reason, and the strip's own says the one that holds now.
    fixture.answer(vec![json!({
        "argv": ["**"],
        "stderr": "gh: Server Error (HTTP 502)\n",
        "exit": 1
    })]);
    cx.simulate_keystrokes("secondary-r");
    idle(&shell, cx, "the refresh that fails another way");
    let ReadLine::Stale(now) = strip(&shell, cx).read else {
        panic!("not stale");
    };
    assert_ne!(
        now.message,
        "gh isn't signed in to github.com: run `gh auth login`"
    );
    assert_eq!(banners(&shell, cx), (None, true, true));
    // Pasted again while it fails that way: the open's banner says that reason.
    fixture.open(&shell, cx).unwrap();
    let again = format!("couldn't read #7 from GitHub: {}", now.message);
    assert_eq!(banners(&shell, cx), (Some(again), false, true));

    // `gh` works again, `Ctrl R` reads GitHub: the banner that said it couldn't be read goes.
    fixture.answer(Vec::new());
    cx.simulate_keystrokes("secondary-r");
    idle(&shell, cx, "the refresh that works");
    assert!(matches!(strip(&shell, cx).read, ReadLine::Read(_)));
    assert_eq!(banners(&shell, cx), (None, false, false));

    // A banner about something else isn't the pull request's to take away.
    fixture.answer(vec![fixture_gh::signed_out()]);
    let other = cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open(
                fixture.location(),
                OpenRequest::Pr(PrRef::Hash(9)),
                window,
                cx,
            )
        })
    });
    settle(cx, &shell, "the open that fails", |shell, _| {
        shell.opening().is_none()
    });
    cx.foreground_executor().block_test(other).unwrap_err();
    fixture.answer(Vec::new());
    cx.simulate_keystrokes("secondary-r");
    idle(&shell, cx, "another refresh");
    shell.read_with(cx, |shell, _| {
        let banner = shell.banner().expect("the other open's banner stays");
        assert!(banner.failure.message.starts_with("couldn't open #9"));
        assert_eq!(banner.unread, None);
    });
}

/// PLAN §9.4: a refresh that read the pull request and then couldn't move the draft says
/// that, not that it couldn't refresh: what shows of GitHub is what it holds now.
#[test]
fn a_fresh_read_whose_review_didnt_move_says_so() {
    let unmoved = ReadLine::Unmoved(Failure::new("the pins moved under the refresh"));
    assert_eq!(unmoved.key(), "unmoved");
    assert_eq!(unmoved.text(0), "Couldn't move the review · details");
    assert!(unmoved.failure().is_some());
    assert!(ReadLine::Read(0).failure().is_none());
}
