//! The PR picker and a pull request's chosen folder where they meet what moves under them
//! (PLAN §9.9): a clone that became another repository while its list showed, a list asked
//! for again and again, a folder answered after its asker went away, a repository that
//! can't be reached, two clones of one repository, and `Ctrl R` while a text field has the
//! keyboard.

use std::sync::atomic::AtomicUsize;

use dv_test_support::FixtureRepo;
use pretty_assertions::assert_eq;

use super::read::{idle, listed, palette_rows, picker_keys, picker_state};
use super::*;
use crate::palette::Section;
use crate::pr_picker::ListState;
use crate::services::github::ListFailureKind;
use crate::shell::pr::FolderAsker;
use crate::system::{self, SystemRequest};
use crate::test_support::{add_repo, open_shell_wsl};

/// Opens the working review of the repository at `location`, which puts it in view.
fn open_working(shell: &Entity<Shell>, location: Location, cx: &mut VisualTestContext) {
    let opening = cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open(location, OpenRequest::Target(Target::Working), window, cx)
        })
    });
    idle(shell, cx, "the working review");
    cx.foreground_executor().block_test(opening).unwrap();
}

fn folder_requests(cx: &mut VisualTestContext) -> usize {
    cx.update(|_, cx| {
        system::requests(cx)
            .0
            .iter()
            .filter(|request| matches!(request, SystemRequest::PickFolder { .. }))
            .count()
    })
}

fn answer(path: Option<PathBuf>, cx: &mut VisualTestContext) {
    cx.update(|_, cx| system::queue_answer(path, cx)).unwrap();
}

/// PLAN §9.9: a listed row opens the pull request of the repository its list is of. A
/// clone whose remote was pointed at another repository while the list showed is refused
/// by the open's own check, never opened as that repository's pull request of the same
/// number; and the list read next drops the rows of the repository the clone was.
#[gpui::test]
fn a_listed_row_opens_its_lists_repository_whatever_the_clone_became(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let fixture = PrFixture::logged(cx);
    fixture.answer(listed());
    add_repo(fixture.github.checkout(), &shell, cx);
    open_working(&shell, fixture.location(), cx);
    cx.simulate_keystrokes("secondary-g");
    idle(&shell, cx, "the list");
    assert_eq!(picker_keys(&shell, cx), ["pr/7", "pr/12", "pr/9"]);

    fixture.github.checkout().git(&[
        "remote",
        "set-url",
        "origin",
        "https://github.com/acme/other.git",
    ]);
    let asked = fixture.gh_calls().len();
    cx.simulate_keystrokes("enter");
    settle(cx, &shell, "the refused open", |shell, _| {
        shell.opening().is_none()
    });
    shell.read_with(cx, |shell, cx| {
        let banner = shell.banner().expect("why it didn't open");
        assert_eq!(
            banner.failure.message,
            "couldn't open #7: #7 is in dv-fixture/api; open it from a clone of that repository"
        );
        let session = shell.session().expect("the working review stays").read(cx);
        assert!(!session.review().is_pr_review());
    });
    // Nothing was asked about acme/other's pull request 7.
    assert_eq!(fixture.gh_calls().len(), asked);

    // The list read next is another repository's, and here it can't be read: the rows of
    // the repository the clone was are gone.
    fixture.answer(vec![fixture_gh::signed_out()]);
    cx.simulate_keystrokes("secondary-g");
    idle(&shell, cx, "the list of the other repository");
    let ListState::Failed(failed) = picker_state(&shell, cx) else {
        panic!("the old rows still show: {:?}", picker_keys(&shell, cx));
    };
    assert_eq!(failed.kind, ListFailureKind::SignedOut);
    assert_eq!(picker_keys(&shell, cx), Vec::<String>::new());
    shell.read_with(cx, |shell, cx| {
        let list = shell.pr_picker().unwrap().read(cx).list(cx).unwrap();
        assert_eq!(list.prs, None);
        assert_eq!(
            list.repo.as_ref().map(|repo| repo.name_with_owner()),
            Some("acme/other".to_string())
        );
    });
}

/// PLAN §9.9: `Ctrl G` pressed again and again while the list is read asks `gh` once.
#[gpui::test]
fn the_picker_opened_again_while_its_list_is_read_asks_gh_once(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let fixture = PrFixture::logged(cx);
    fixture.answer(listed());
    add_repo(fixture.github.checkout(), &shell, cx);
    open_working(&shell, fixture.location(), cx);
    let github = cx.update(|_, cx| cx.services().github.clone());
    let slow = github.update(cx, |github, _| github.hold());

    // Open, close, open, close, open: `gh` hasn't answered the first read.
    for _ in 0..5 {
        cx.simulate_keystrokes("secondary-g");
    }
    assert_eq!(picker_state(&shell, cx), ListState::Loading);
    assert_eq!(fixture.gh_calls(), Vec::<Vec<String>>::new());
    drop(slow);
    idle(&shell, cx, "the list");
    assert_eq!(picker_state(&shell, cx), ListState::Listed);
    let calls = fixture.gh_calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls.iter().all(|argv| argv[..2] == ["pr", "list"]));
}

/// PLAN §9.9: one folder is chosen at a time, and its answer belongs to the menu that
/// asked, as it was: closed since, or showing another reference, the answer is dropped,
/// whatever it is.
#[gpui::test]
fn a_folder_is_chosen_once_and_an_answer_nobody_waits_for_is_dropped(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let fixture = PrFixture::logged(cx);
    let clone = fixture.github.checkout().path().to_path_buf();
    let reviews = cx.update(|_, cx| cx.services().reviews.clone());
    let reference = PrRef::parse("dv-fixture/api#7").unwrap();
    let ask = |shell: &Entity<Shell>, cx: &mut VisualTestContext, times: usize| {
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                let palette = shell.palette().expect("the palette").entity_id();
                for _ in 0..times {
                    shell.choose_pr_folder(
                        reference.clone(),
                        FolderAsker::Palette(palette),
                        window,
                        cx,
                    );
                }
                assert!(shell.is_choosing_pr_folder());
            })
        });
    };
    let nothing_opened = |shell: &Entity<Shell>, cx: &mut VisualTestContext| {
        shell.read_with(cx, |shell, _| {
            assert!(!shell.is_choosing_pr_folder());
            assert!(shell.session().is_none() && shell.opening().is_none());
            assert!(shell.banner().is_none(), "{:?}", shell.banner());
        });
        reviews.read_with(cx, |reviews, _| assert_eq!(reviews.rows().len(), 0));
    };

    // Asked twice in a row (Enter held down): one folder panel. Its answer, the right
    // clone, comes after the palette closed.
    cx.simulate_keystrokes("secondary-k");
    cx.simulate_input("dv-fixture/api#7");
    assert_eq!(
        palette_rows(&shell, cx)[0],
        (Section::PullRequest, "pr/choose_folder".to_string())
    );
    answer(Some(clone.clone()), cx);
    answer(Some(clone.clone()), cx);
    ask(&shell, cx, 2);
    assert_eq!(folder_requests(cx), 1);
    cx.simulate_keystrokes("escape");
    idle(&shell, cx, "the answer nobody waits for");
    nothing_opened(&shell, cx);
    assert_eq!(folder_requests(cx), 1);
    // The folder was its clone, opened to be looked at: let go of, as a refused one is.
    let locations = cx.update(|_, cx| cx.services().locations.clone());
    assert!(locations.read_with(cx, |locations, _| {
        locations.cached(&Location::Local(clone.clone())).is_none()
    }));

    // The palette opened again is another one: the answer to the first isn't its.
    cx.simulate_keystrokes("secondary-k");
    cx.simulate_input("dv-fixture/api#7");
    ask(&shell, cx, 1);
    cx.simulate_keystrokes("escape");
    cx.simulate_keystrokes("secondary-k");
    cx.simulate_input("dv-fixture/api#7");
    idle(&shell, cx, "the answer to a palette that closed");
    nothing_opened(&shell, cx);
    shell.read_with(cx, |shell, cx| {
        let palette = shell.palette().expect("the new palette").read(cx);
        assert_eq!(palette.notice(), None);
    });

    // Another reference typed meanwhile: a refusal isn't said under a row it isn't about.
    let other = FixtureGithub::new("web");
    register(other.checkout(), cx);
    answer(Some(other.checkout().path().to_path_buf()), cx);
    ask(&shell, cx, 1);
    cx.simulate_input("1");
    idle(&shell, cx, "the answer to another reference");
    nothing_opened(&shell, cx);
    shell.read_with(cx, |shell, cx| {
        let palette = shell.palette().unwrap().read(cx);
        assert_eq!(palette.query(), "dv-fixture/api#71");
        assert_eq!(palette.notice(), None);
    });
    assert_eq!(fixture.gh_calls(), Vec::<Vec<String>>::new());
}

/// PLAN §9.9: a folder that isn't taken leaves nothing open; a known repository's backend
/// stays, whatever the answer.
#[gpui::test]
fn a_refused_folder_leaves_nothing_open(cx: &mut TestAppContext) {
    let (dir, shell, cx) = open_shell(cx);
    let fixture = PrFixture::new(cx);
    let reviews = cx.update(|_, cx| cx.services().reviews.clone());
    let locations = cx.update(|_, cx| cx.services().locations.clone());
    let reference = PrRef::parse("acme/web#7").unwrap();
    let check = |location: Location, cx: &mut VisualTestContext| {
        let checking = reviews.update(cx, |reviews, cx| {
            reviews.pr_folder(location, reference.clone(), cx)
        });
        idle(&shell, cx, "the folder's check");
        cx.foreground_executor().block_test(checking)
    };
    let cached = |location: &Location, cx: &mut VisualTestContext| {
        locations.read_with(cx, |locations, _| locations.cached(location).is_some())
    };

    // Another repository's clone: opened to be looked at, then let go of.
    let clone = fixture.location();
    let refused = check(clone.clone(), cx).unwrap_err();
    assert_eq!(
        refused.message,
        format!("`{}` is dv-fixture/api, not acme/web", clone.display_name())
    );
    assert!(!cached(&clone, cx));
    // A repository without GitHub, through a subfolder of it.
    let plain = FixtureRepo::new();
    plain.write("src/a.txt", "a\n");
    plain.commit("first");
    register(&plain, cx);
    let root = Location::Local(plain.path().to_path_buf());
    let subfolder = Location::Local(plain.path().join("src"));
    check(subfolder.clone(), cx).unwrap_err();
    assert!(!cached(&root, cx) && !cached(&subfolder, cx));
    // No repository at all.
    let folder = Location::Local(dir.path().to_path_buf());
    check(folder.clone(), cx).unwrap_err();
    assert!(!cached(&folder, cx));

    // A known repository's backend is the app's to keep.
    add_repo(fixture.github.checkout(), &shell, cx);
    assert!(cached(&clone, cx));
    check(clone.clone(), cx).unwrap_err();
    assert!(cached(&clone, cx));
}

/// PLAN §9.9: the picker's own "Choose its folder", for a reference to a repository dv
/// knows no clone of: a folder that isn't its clone is refused under the row, its clone
/// opens the pull request.
#[gpui::test]
fn the_pickers_choose_its_folder_opens_the_pull_request_in_its_clone(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let fixture = PrFixture::new(cx);
    let plain = crate::test_support::fixture();
    add_repo(&plain, &shell, cx);
    open_working(&shell, Location::Local(plain.path().to_path_buf()), cx);
    let notice = |shell: &Entity<Shell>, cx: &mut VisualTestContext| {
        shell.read_with(cx, |shell, cx| {
            let picker = shell.pr_picker().expect("the picker stays open").read(cx);
            picker.notice().map(str::to_string)
        })
    };

    cx.simulate_keystrokes("secondary-g");
    idle(&shell, cx, "the list that isn't");
    cx.simulate_input("https://github.com/dv-fixture/api/pull/7");
    assert_eq!(picker_keys(&shell, cx), ["choose_folder"]);
    shell.read_with(cx, |shell, cx| {
        // The reference names another repository than the one in view, whose list and
        // name the picker then doesn't show.
        assert!(
            shell
                .pr_picker()
                .unwrap()
                .read(cx)
                .names_another_repository()
        );
    });

    answer(Some(plain.path().to_path_buf()), cx);
    cx.simulate_keystrokes("enter");
    idle(&shell, cx, "the folder that isn't its clone");
    assert_eq!(
        notice(&shell, cx),
        Some(format!(
            "`{}`: this repository has no remotes",
            plain.path().display()
        ))
    );

    answer(Some(fixture.github.checkout().path().to_path_buf()), cx);
    cx.simulate_keystrokes("enter");
    settle(cx, &shell, "the pull request", |shell, cx| {
        shell
            .session()
            .is_some_and(|session| session.read(cx).review().is_pr_review())
    });
    shell.read_with(cx, |shell, cx| {
        assert!(shell.pr_picker().is_none());
        assert!(shell.banner().is_none(), "{:?}", shell.banner());
        assert_eq!(shell.repo_in_view(cx), Some(fixture.location()));
    });
}

/// PLAN §9.9: the picker on a repository in a stopped distro says so and starts nothing:
/// the repository is reached as a listing reaches it.
#[gpui::test]
fn the_picker_on_a_stopped_distro_says_so_and_starts_nothing(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell_wsl(
        cx,
        vec![dv_backend::wsl::Distro {
            name: "Ubuntu".to_string(),
            running: false,
        }],
    );
    let location = Location::Wsl {
        distro: "Ubuntu".to_string(),
        path: "/srv/api".to_string(),
    };
    // The open, which may boot a distro, meets the stand-in: its banner puts the
    // repository in view.
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
    settle(cx, &shell, "the open that can't reach WSL", |shell, _| {
        shell.opening().is_none()
    });
    cx.foreground_executor().block_test(opening).unwrap_err();
    shell.read_with(cx, |shell, cx| {
        assert_eq!(shell.repo_in_view(cx), Some(location.clone()));
    });

    cx.simulate_keystrokes("secondary-g");
    idle(&shell, cx, "the list that can't be read");
    let ListState::Failed(failed) = picker_state(&shell, cx) else {
        panic!("a stopped distro listed something");
    };
    assert_eq!(failed.kind, ListFailureKind::Unreachable);
    assert_eq!(failed.failure.message, "Ubuntu isn't running");
    // Its field still takes a reference, which names the repository in view.
    cx.simulate_input("#7");
    assert_eq!(picker_keys(&shell, cx).len(), 1);
}

/// PLAN §9.9: the picker on a repository whose connection is reconnecting says so and
/// doesn't hurry the connection: no server is started for the list.
#[gpui::test]
fn the_picker_on_a_reconnecting_repository_says_so_and_starts_nothing(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let repo = fixture();
    // An in-process server; once it dies, its next start is half a minute away.
    let started = Arc::new(AtomicUsize::new(0));
    let launcher = Launcher::custom("an in-process server the test kills", {
        let started = started.clone();
        move || {
            started.fetch_add(1, Ordering::SeqCst);
            Launcher::InProcess.start()
        }
    });
    let connection = Connection::with_backoff("loopback", launcher, vec![Duration::from_secs(30)]);
    let locations = cx.update(|_, cx| cx.services().locations.clone());
    locations.update(cx, |locations, _| {
        locations.register(
            repo.path().to_path_buf(),
            Route {
                env: repo.env_overlay(),
                connection: Some(connection.clone()),
            },
        )
    });
    open_working(&shell, Location::Local(repo.path().to_path_buf()), cx);
    connection.kill();
    let wait =
        |what: &str, cx: &mut VisualTestContext, done: &dyn Fn(&mut VisualTestContext) -> bool| {
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            while !done(cx) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "timed out waiting for {what}"
                );
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(20));
            }
        };
    wait("the connection to notice", cx, &|_| {
        matches!(connection.state(), ConnectionState::Reconnecting { .. })
    });
    let starts = started.load(Ordering::SeqCst);

    cx.simulate_keystrokes("secondary-g");
    wait("the list that can't be read", cx, &|cx| {
        picker_state(&shell, cx) != ListState::Loading
    });
    let ListState::Failed(failed) = picker_state(&shell, cx) else {
        panic!("a repository without its server listed something");
    };
    assert_eq!(failed.kind, ListFailureKind::Unreachable);
    assert_eq!(
        failed.failure.message,
        "the connection to the repository is reconnecting"
    );
    assert_eq!(started.load(Ordering::SeqCst), starts);
    assert!(matches!(
        connection.state(),
        ConnectionState::Reconnecting { .. }
    ));
}

/// PLAN §9.9: two clones of one repository. A reference that names the repository offers
/// each by its folder, and the pull request opens in the one picked; the other is left
/// alone.
#[gpui::test]
fn a_pull_request_of_a_repository_with_two_clones_opens_in_the_one_picked(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let fixture = PrFixture::new(cx);
    let first = fixture.github.checkout();
    // A second clone of the same GitHub repository, of the fixture's bare one.
    let second = FixtureRepo::new();
    let bare = format!("file://{}", fixture.github.bare().display());
    second.git(&["remote", "add", "origin", &fixture.github.url()]);
    second.config(&format!("url.{bare}.insteadOf"), &fixture.github.url());
    second.git(&["fetch", "origin"]);
    second.git(&["checkout", "-q", "--detach", "origin/main"]);
    add_repo(first, &shell, cx);
    add_repo(&second, &shell, cx);
    let reviews = cx.update(|_, cx| cx.services().reviews.clone());
    let (one, two) = (
        Location::Local(first.path().to_path_buf()),
        Location::Local(second.path().to_path_buf()),
    );
    reviews.read_with(cx, |reviews, _| {
        for location in [&one, &two] {
            let github = reviews.row(location).unwrap().github.clone();
            assert_eq!(
                github.map(|github| github.name_with_owner()),
                Some("dv-fixture/api".to_string()),
                "{location:?}"
            );
        }
    });

    // The list is read for the first clone alone; the title it holds is the pull
    // request's, so the row of either clone shows it.
    fixture.answer(listed());
    open_working(&shell, one.clone(), cx);
    cx.simulate_keystrokes("secondary-g");
    idle(&shell, cx, "the first clone's list");
    cx.simulate_keystrokes("escape");

    cx.simulate_keystrokes("secondary-k");
    cx.simulate_input("dv-fixture/api#7");
    shell.read_with(cx, |shell, cx| {
        let palette = shell.palette().unwrap().read(cx);
        let titles: Vec<Option<&str>> = palette
            .rows()
            .iter()
            .filter_map(|row| match &row.kind {
                crate::palette::RowKind::Pr { title, .. } => Some(title.as_deref()),
                _ => None,
            })
            .collect();
        assert_eq!(titles, [Some("Add the answer"), Some("Add the answer")]);
    });
    let rows: Vec<String> = palette_rows(&shell, cx)
        .into_iter()
        .filter(|(section, _)| *section == Section::PullRequest)
        .map(|(_, key)| key)
        .collect();
    let (first_row, second_row) = (
        format!("pr/{}", one.display_name()),
        format!("pr/{}", two.display_name()),
    );
    let mut sorted = rows.clone();
    sorted.sort();
    let mut expected = vec![first_row, second_row.clone()];
    expected.sort();
    assert_eq!(sorted, expected);
    // The second clone's row.
    for _ in 0..rows.iter().position(|row| *row == second_row).unwrap() {
        cx.simulate_keystrokes("down");
    }
    shell.read_with(cx, |shell, cx| {
        let palette = shell.palette().unwrap().read(cx);
        assert_eq!(palette.selected_key(), Some(second_row.as_str()));
    });
    cx.simulate_keystrokes("enter");
    settle(cx, &shell, "the pull request", |shell, cx| {
        shell
            .session()
            .is_some_and(|session| session.read(cx).review().is_pr_review())
    });
    shell.read_with(cx, |shell, cx| {
        assert!(shell.banner().is_none(), "{:?}", shell.banner());
        assert_eq!(shell.repo_in_view(cx), Some(two.clone()));
    });
    // Its review is that clone's; the first clone has none of the pull request.
    let pr_reviews = |location: &Location, cx: &mut VisualTestContext| {
        reviews.update(cx, |reviews, cx| reviews.refresh(location.clone(), cx));
        idle(&shell, cx, "the listing");
        reviews.read_with(cx, |reviews, _| {
            reviews
                .row(location)
                .unwrap()
                .reviews
                .iter()
                .filter(|review| {
                    matches!(
                        review,
                        crate::services::reviews::ReviewRow::Ok { pr: Some(_), .. }
                    )
                })
                .count()
        })
    };
    assert_eq!((pr_reviews(&one, cx), pr_reviews(&two, cx)), (0, 1));
}

/// DESIGN §5: `Ctrl R` refreshes the open PR review wherever the keyboard is, a text field
/// included: the composer keeps its text and its focus.
#[gpui::test]
fn ctrl_r_refreshes_from_inside_a_text_field(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let mut fixture = PrFixture::logged(cx);
    fixture.open(&shell, cx).unwrap();
    idle(&shell, cx, "the pull request");
    let pane = shell.read_with(cx, |shell, _| shell.diff_pane().clone());
    cx.update(|window, cx| shell.read(cx).focus_handle(cx).focus(window, cx));
    // A comment on the first line of the file, its composer open with text in it.
    cx.simulate_keystrokes("c");
    idle(&shell, cx, "the composer");
    assert!(pane.read_with(cx, |pane, _| pane.composer().is_some()));
    cx.simulate_input("half a thought");

    let second = fixture.push("pub fn answer() -> u32 {\n    43\n}\n");
    let asked = fixture.gh_calls().len();
    cx.simulate_keystrokes("secondary-r");
    idle(&shell, cx, "the refresh");
    assert!(fixture.gh_calls().len() > asked, "nothing was asked");
    shell.read_with(cx, |shell, cx| {
        let session = shell.session().unwrap().read(cx);
        assert_eq!(
            session
                .pr()
                .unwrap()
                .mirror
                .as_ref()
                .unwrap()
                .pr
                .head_oid
                .as_str(),
            second
        );
    });
    pane.read_with(cx, |pane, cx| {
        assert!(pane.composer().is_some());
        assert_eq!(pane.composer_text(cx).as_deref(), Some("half a thought"));
    });
}

/// PLAN §9.9: whatever a title holds and however long it is, each row that names the pull
/// request draws one line of it, no longer than a row hands its layout: the picker's
/// listed row and its reference row, the strip, the title bar's label and the switcher's
/// row.
#[gpui::test]
fn a_title_of_any_length_is_one_bounded_line_wherever_it_shows(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let mut fixture = PrFixture::new(cx);
    let hostile = format!(" Fix\nthe \u{202e}cache\r{} ", "x".repeat(5_000));
    let drawn = crate::thread::drawn(&hostile);
    assert!(
        drawn.starts_with("Fix\\nthe \\u{202e}cache\\rxxx"),
        "{drawn}"
    );
    assert_eq!(drawn.chars().count(), crate::thread::DRAWN_CHARS + 1);
    assert!(drawn.ends_with('…'));
    let login = format!("octo\u{202e}{}", "c".repeat(400));
    let drawn_login = crate::thread::drawn(&login);

    fixture.pr.title = hostile.clone();
    fixture.pr.author = Some(login.clone());
    let row = fixture_gh::listed(7, &hostile, &login, false, "2026-10-19T08:30:00Z");
    fixture.answer(fixture_gh::pr_list(vec![row.clone()], vec![row]));
    add_repo(fixture.github.checkout(), &shell, cx);
    open_working(&shell, fixture.location(), cx);

    cx.simulate_keystrokes("secondary-g");
    idle(&shell, cx, "the list");
    shell.read_with(cx, |shell, cx| {
        let picker = shell.pr_picker().unwrap().read(cx);
        let crate::pr_picker::RowKind::Listed { pr, .. } = &picker.rows()[0].kind else {
            panic!("not a listed row");
        };
        assert_eq!((&pr.title, &pr.author), (&drawn, &drawn_login));
    });
    // The reference row takes its title from the list.
    cx.simulate_input("#7");
    shell.read_with(cx, |shell, cx| {
        let picker = shell.pr_picker().unwrap().read(cx);
        let crate::pr_picker::RowKind::Reference { title, .. } = &picker.rows()[0].kind else {
            panic!("not the reference's row");
        };
        assert_eq!(title.as_ref(), Some(&drawn));
    });

    cx.simulate_keystrokes("enter");
    settle(cx, &shell, "the pull request", |shell, cx| {
        shell
            .session()
            .is_some_and(|session| session.read(cx).review().is_pr_review())
    });
    idle(&shell, cx, "the strip");
    shell.read_with(cx, |shell, cx| {
        let strip = shell.pr_strip(cx).unwrap();
        assert_eq!(strip.title.as_ref(), Some(&drawn));
        assert_eq!(strip.author.as_ref(), Some(&drawn_login));
        // The title bar's label.
        assert_eq!(
            shell.session().unwrap().read(cx).target_label(),
            format!("#7 {drawn}")
        );
    });
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
        assert_eq!(item.label(), format!("#7 {drawn}"));
    });
}

/// PLAN §9.9: a clone's remembered GitHub name is a hint for which row to offer, never
/// what opens. A clone pointed at another repository since its name was read is still
/// offered by the name remembered, and the open's own check refuses it; the next listing
/// remembers the new name.
#[gpui::test]
fn a_remembered_name_gone_stale_ends_as_another_repository(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let fixture = PrFixture::logged(cx);
    let checkout = fixture.github.checkout();
    add_repo(checkout, &shell, cx);
    let location = fixture.location();

    cx.simulate_keystrokes("secondary-k");
    cx.simulate_input("dv-fixture/api#7");
    assert_eq!(
        palette_rows(&shell, cx)[0],
        (Section::PullRequest, "pr/repo".to_string())
    );
    // The clone is pointed at another repository while its row shows.
    checkout.git(&[
        "remote",
        "set-url",
        "origin",
        "https://github.com/acme/other.git",
    ]);
    cx.simulate_keystrokes("enter");
    settle(cx, &shell, "the refused open", |shell, _| {
        shell.opening().is_none()
    });
    shell.read_with(cx, |shell, _| {
        assert_eq!(
            shell.banner().expect("why it didn't open").failure.message,
            "couldn't open #7: #7 is in dv-fixture/api; open it from a clone of that repository"
        );
        assert!(shell.session().is_none());
    });
    // No pull request was asked for: the check is the clone's remotes.
    assert!(
        fixture
            .gh_calls()
            .iter()
            .all(|argv| argv[0] != "pr" && argv[..2] != ["api", "graphql"]),
        "{:?}",
        fixture.gh_calls()
    );

    // Listed again, the clone is remembered as what it is now.
    let reviews = cx.update(|_, cx| cx.services().reviews.clone());
    reviews.update(cx, |reviews, cx| reviews.refresh(location.clone(), cx));
    idle(&shell, cx, "the listing");
    reviews.read_with(cx, |reviews, _| {
        assert_eq!(
            reviews
                .row(&location)
                .unwrap()
                .github
                .as_ref()
                .map(|github| github.name_with_owner()),
            Some("acme/other".to_string())
        );
    });
    cx.simulate_keystrokes("secondary-k");
    cx.simulate_input("dv-fixture/api#7");
    assert_eq!(
        palette_rows(&shell, cx)[0],
        (Section::PullRequest, "pr/choose_folder".to_string())
    );
}

/// PLAN §9.1: whether a host is GitHub's is asked of `gh` once a minute at most, however
/// often the repositories are listed.
#[gpui::test]
fn a_remotes_host_is_asked_about_once_however_often_its_repository_is_listed(
    cx: &mut TestAppContext,
) {
    let (_dir, shell, cx) = open_shell(cx);
    let fixture = PrFixture::logged(cx);
    let plain = crate::test_support::fixture();
    plain.git(&[
        "remote",
        "add",
        "origin",
        "https://git.example.com/acme/api.git",
    ]);
    add_repo(&plain, &shell, cx);
    let location = Location::Local(plain.path().to_path_buf());
    let asked = |fixture: &PrFixture| {
        fixture
            .gh_calls()
            .iter()
            .filter(|argv| argv[..2] == ["auth", "status"])
            .count()
    };
    assert_eq!(asked(&fixture), 1, "{:?}", fixture.gh_calls());
    let reviews = cx.update(|_, cx| cx.services().reviews.clone());
    for _ in 0..3 {
        reviews.update(cx, |reviews, cx| reviews.refresh(location.clone(), cx));
        idle(&shell, cx, "the listing");
    }
    assert_eq!(asked(&fixture), 1, "{:?}", fixture.gh_calls());
    reviews.read_with(cx, |reviews, _| {
        assert_eq!(reviews.row(&location).unwrap().github, None);
    });
}

/// PLAN §9.6: the open pull request is refreshed by its session however it is named again:
/// by its URL, which is what a listed row opens, also before the review has a mirror to
/// tell its host by, and from a subfolder of its repository, the stream staying as it is.
#[gpui::test]
fn the_open_pull_request_is_refreshed_by_its_url_and_from_a_subfolder(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let fixture = PrFixture::logged(cx);
    let root = fixture.location();
    let folder = fixture.github.checkout().path().join("docs");
    std::fs::create_dir_all(&folder).unwrap();
    let subfolder = Location::Local(folder);
    let url = |host: &str, owner: &str| PrRef::Url {
        host: host.to_string(),
        owner: owner.to_string(),
        repo: "API".to_string(),
        number: 7,
    };
    let open = |location: &Location, reference: PrRef, cx: &mut VisualTestContext| {
        let location = location.clone();
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.open(location, OpenRequest::Pr(reference), window, cx)
            })
        })
    };

    // Opened through the subfolder: its review is the repository's.
    let opening = open(&subfolder, PrRef::Hash(7), cx);
    idle(&shell, cx, "the pull request");
    assert!(
        cx.foreground_executor()
            .block_test(opening)
            .unwrap()
            .created
    );
    let session = shell.read_with(cx, |shell, _| shell.session().cloned().unwrap());
    session.read_with(cx, |session, _| {
        assert_eq!(session.origin(), &root);
        assert!(session.is_open_pr(&root, &url("github.com", "DV-Fixture")));
        // Another host than the one it was read from, or another repository, isn't it.
        assert!(!session.is_open_pr(&root, &url("ghe.example.com", "dv-fixture")));
        assert!(!session.is_open_pr(&root, &url("github.com", "someone-else")));
        assert!(!session.is_open_pr(&subfolder, &PrRef::Hash(7)));
    });

    // Again through the subfolder, and by its URL: the same session refreshes, and what
    // is opening is the review's own repository, so the stream stays.
    for (location, reference) in [
        (&subfolder, PrRef::Hash(7)),
        (&root, url("github.com", "dv-fixture")),
    ] {
        let asked = fixture.gh_calls().len();
        let opening = open(location, reference, cx);
        shell.read_with(cx, |shell, cx| {
            assert_eq!(
                shell.opening().map(|opening| &opening.location),
                Some(&root)
            );
            let shown = shell.session().unwrap();
            assert_eq!(shown.entity_id(), session.entity_id());
            assert!(shown.read(cx).is_refreshing_pr());
        });
        idle(&shell, cx, "the refresh");
        let again = cx.foreground_executor().block_test(opening).unwrap();
        assert!(!again.created);
        // One refresh's calls, and no open's beside them.
        let calls = fixture.gh_calls();
        let views = calls[asked..]
            .iter()
            .filter(|argv| argv[..2] == ["pr", "view"] && argv[4] != "headRefOid")
            .count();
        assert_eq!(views, 1, "{:?}", &calls[asked..]);
    }
}
