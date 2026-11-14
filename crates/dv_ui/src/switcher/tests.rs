use dv_core::review::{Author, ReviewId, ReviewStatus, ReviewSummary, ThreadCounts};
use dv_core::{Location, Target};
use gpui::{Entity, TestAppContext, VisualTestContext};
use pretty_assertions::assert_eq;

use super::branches::{Mode, build_bases, build_branches, has_branches};
use super::items::{Entry, ItemKind, RepoSummary, build};
use super::*;
use crate::failure::Failure;
use crate::services::reviews::{RepoRow, ReviewRow, WorkingSummary};
use crate::shell::Shell;
use crate::test_support::{add_repo, fixture_at, open_shell, settle};
use dv_core::Oid;
use dv_core::git::branches::Branch;
use dv_core::label::LabelContext;

fn row(name: &str, path: &str) -> RepoRow {
    RepoRow {
        location: Location::Local(path.into()),
        name: name.to_string(),
        path: path.to_string(),
        loading: false,
        error: None,
        unreachable: None,
        stopped: None,
        retry: false,
        reconnecting: false,
        starting: false,
        toplevel: Some(path.to_string()),
        working: Some(WorkingSummary {
            files: 2,
            additions: 5,
            deletions: 1,
        }),
        reviews: Vec::new(),
        labels: LabelContext {
            head: Some("feat".into()),
            branch: Some("feat".into()),
            default_base: Some("main".into()),
        },
        branches: Vec::new(),
        last_used: 0,
    }
}

fn summary(target: Target, worktree: &str, status: ReviewStatus) -> ReviewSummary {
    ReviewSummary {
        id: ReviewId::generate(),
        target,
        worktree: worktree.to_string(),
        author: Author::reviewer("kyle"),
        created_at: 0,
        updated_at: 0,
        status,
        current_round: 2,
        threads: ThreadCounts {
            open: 3,
            addressed: 0,
            resolved: 1,
            annotation: 0,
        },
        pr: None,
        warnings: 0,
    }
}

fn keys(listing: &items::Listing) -> Vec<&str> {
    listing.items.iter().map(|item| item.key.as_str()).collect()
}

#[test]
fn a_repo_offers_its_targets_beside_their_open_reviews() {
    let mut api = row("api-server", "D:\\code\\api-server");
    let working = summary(
        Target::Working,
        "D:\\code\\api-server",
        ReviewStatus::ChangesRequested,
    );
    let elsewhere = summary(Target::Working, "D:\\code\\api-wt", ReviewStatus::Open);
    let commit = summary(
        Target::Commit {
            rev: "abc1234".into(),
        },
        "D:\\code\\api-server",
        ReviewStatus::Ready,
    );
    let finished = summary(
        Target::Staged,
        "D:\\code\\api-server",
        ReviewStatus::Approved,
    );
    for summary in [&working, &elsewhere, &commit, &finished] {
        api.reviews.push(ReviewRow::Ok {
            summary: Box::new(summary.clone()),
            label: "label".into(),
        });
    }
    let listing = build(&[&api], "");
    assert_eq!(
        keys(&listing),
        [
            "api-server/working".to_string(),
            "api-server/staged".to_string(),
            "api-server/branch".to_string(),
            format!("api-server/review/{}", elsewhere.id),
            format!("api-server/review/{}", commit.id),
            "open_folder".to_string(),
        ]
    );
    // The working row stands for this worktree's open working review; the finished staged
    // review is offered as a new one.
    assert_eq!(
        listing.items[0].review().map(|badge| (
            &badge.id,
            badge.status,
            badge.round,
            badge.open_threads
        )),
        Some((&working.id, ReviewStatus::ChangesRequested, 2, 3))
    );
    assert_eq!(listing.items[1].review(), None);
    assert_eq!(listing.items[2].label(), "Branch feat vs main");
    assert_eq!(
        listing.repos[0].summary,
        RepoSummary::Changes(api.working.unwrap())
    );
    assert_eq!(listing.entries[0], Entry::Repo(0));
    assert_eq!(listing.entry_of(0), Some(1));
    assert_eq!(listing.entry_of(5), None, "Open folder… sits in the footer");
    assert_eq!(
        listing.request(0),
        Some((api.location.clone(), OpenRequest::Target(Target::Working)))
    );
    assert_eq!(
        listing.request(4),
        Some((api.location.clone(), OpenRequest::Review(commit.id.clone())))
    );
    assert_eq!(listing.request(5), None);
    assert_eq!(listing.review_row(&working.id), Some(0));
    assert_eq!(listing.first_of(&api.location), Some(0));
}

#[test]
fn rows_say_why_a_repo_has_nothing_to_offer() {
    let mut wsl = row("web", "Ubuntu:/home/kyle/web");
    wsl.location = Location::Wsl {
        distro: "Ubuntu".into(),
        path: "/home/kyle/web".into(),
    };
    wsl.unreachable = Some("Ubuntu isn't running".into());
    wsl.stopped = Some("Ubuntu".into());
    wsl.error = Some(Failure::new("Ubuntu isn't running"));
    wsl.working = None;
    // A distro being started: its Start row stays, inert, and nothing else is offered yet.
    let mut starting = row("site", "Ubuntu:/srv/site");
    starting.location = Location::Wsl {
        distro: "Ubuntu".into(),
        path: "/srv/site".into(),
    };
    starting.starting = true;
    starting.stopped = Some("Ubuntu".into());
    starting.working = None;
    // A connection that failed: why, and Retry.
    let mut failed = row("docs", "Debian:/srv/docs");
    failed.location = Location::Wsl {
        distro: "Debian".into(),
        path: "/srv/docs".into(),
    };
    failed.unreachable = Some("the connection to dv failed: no build".into());
    failed.error = Some(Failure::new("Debian can't be opened"));
    failed.retry = true;
    failed.working = None;
    // Its Retry picked: connecting.
    let mut retrying = failed.clone();
    retrying.name = "notes".into();
    retrying.path = "Debian:/srv/notes".into();
    retrying.location = Location::Wsl {
        distro: "Debian".into(),
        path: "/srv/notes".into(),
    };
    retrying.starting = true;
    // A connection that is reconnecting: it says so, and keeps the rows it had; without a
    // listing before, there are none.
    let mut reconnecting = row("cache", "D:\\cache");
    reconnecting.reconnecting = true;
    let mut reconnecting_unlisted = row("fresh", "D:\\fresh");
    reconnecting_unlisted.reconnecting = true;
    reconnecting_unlisted.working = None;
    let mut clean = row("clean", "D:\\clean");
    clean.working = Some(WorkingSummary::default());
    // On the default branch there is no branch to compare.
    clean.labels.branch = Some("main".into());
    let mut loading = row("new", "D:\\new");
    loading.working = None;
    // Its `.git` gone since it was opened: the read error, and no rows, not even its reviews.
    let mut broken = row("broken", "D:\\broken");
    broken.working = None;
    broken.error = Some(Failure::new("git status failed"));
    broken.reviews.push(ReviewRow::Ok {
        summary: Box::new(summary(
            Target::Commit { rev: "abc".into() },
            "D:\\broken",
            ReviewStatus::Open,
        )),
        label: "abc".into(),
    });
    // A folder that is no longer a repository: a read error, without the path again.
    let mut gone = row("gone", r"D:\gone");
    gone.working = None;
    gone.unreachable = Some(r"not a git repository: D:\gone".into());
    gone.error = Some(Failure::new(r"not a git repository: D:\gone"));

    let listing = build(
        &[
            &gone,
            &wsl,
            &clean,
            &loading,
            &broken,
            &starting,
            &failed,
            &retrying,
            &reconnecting,
            &reconnecting_unlisted,
        ],
        "",
    );
    let summaries: Vec<&RepoSummary> = listing.repos.iter().map(|repo| &repo.summary).collect();
    assert_eq!(
        summaries,
        [
            &RepoSummary::Error("not a git repository".into()),
            &RepoSummary::Stopped("Ubuntu".into()),
            &RepoSummary::Clean,
            &RepoSummary::Loading,
            &RepoSummary::Error("git status failed".into()),
            &RepoSummary::Starting("Starting Ubuntu…".into()),
            &RepoSummary::Unreachable("the connection to dv failed: no build".into()),
            &RepoSummary::Starting("Debian · connecting…".into()),
            &RepoSummary::Unreachable("reconnecting…".into()),
            &RepoSummary::Unreachable("reconnecting…".into()),
        ]
    );
    assert_eq!(listing.repos[1].distro.as_deref(), Some("Ubuntu"));
    assert_eq!(
        listing.repos[1].shown_path, "/home/kyle/web",
        "the chip names the distro; the path is the distro's"
    );
    // A stopped distro's repository offers to start it, and nothing else.
    let start = listing.first_of(&wsl.location).expect("its Start row");
    assert_eq!(listing.items[start].key, "web/start");
    assert_eq!(listing.items[start].label(), "Start Ubuntu");
    assert_eq!(listing.items[start].kind_key(), "start");
    assert_eq!(listing.start(start), Some(wsl.location.clone()));
    assert_eq!(listing.request(start), None, "it opens no review");
    assert!(
        !keys(&listing)
            .iter()
            .any(|key| key.starts_with("web/") && *key != "web/start"),
        "{:?}",
        keys(&listing)
    );
    let busy = listing.first_of(&starting.location).expect("its Start row");
    assert_eq!(listing.items[busy].key, "site/start");
    assert_eq!(listing.items[busy].label(), "Start Ubuntu");
    assert!(listing.items[busy].is_starting());
    assert_eq!(listing.start(busy), None, "picking it again does nothing");
    assert_eq!(listing.first_ready_of(&starting.location), None);
    assert!(!listing.items[start].is_starting());
    assert_eq!(listing.first_ready_of(&wsl.location), Some(start));
    // A failed connection offers Retry, and nothing else.
    let retry = listing.first_of(&failed.location).expect("its Retry row");
    assert_eq!(listing.items[retry].key, "docs/retry");
    assert_eq!(listing.items[retry].label(), "Retry");
    assert_eq!(listing.items[retry].kind_key(), "retry");
    assert_eq!(listing.start(retry), Some(failed.location.clone()));
    assert_eq!(listing.request(retry), None, "it opens no review");
    assert!(
        !keys(&listing)
            .iter()
            .any(|key| key.starts_with("docs/") && *key != "docs/retry"),
        "{:?}",
        keys(&listing)
    );
    let busy = listing.first_of(&retrying.location).expect("its Retry row");
    assert_eq!(listing.items[busy].key, "notes/retry");
    assert!(listing.items[busy].is_starting());
    assert_eq!(listing.start(busy), None, "picking it again does nothing");
    assert!(
        !keys(&listing)
            .iter()
            .any(|key| key.starts_with("notes/") && *key != "notes/retry"),
        "{:?}",
        keys(&listing)
    );
    // Reconnecting, the rows of the last listing stay.
    let kept = listing
        .first_of(&reconnecting.location)
        .expect("the rows it had");
    assert_eq!(listing.items[kept].key, "cache/working");
    assert_eq!(listing.first_of(&reconnecting_unlisted.location), None);
    assert_eq!(
        listing.first_of(&broken.location),
        None,
        "nothing to open in a repo that can't be read"
    );
    assert!(super::items::target_offers(&broken).is_empty());
    assert!(
        listing.first_of(&loading.location).is_some(),
        "still loading"
    );
    assert!(
        !keys(&listing).contains(&"clean/branch"),
        "{:?}",
        keys(&listing)
    );
    assert!(keys(&listing).contains(&"clean/staged"));
}

#[test]
fn the_filter_keeps_rows_holding_every_word() {
    let api = row("api-server", "D:\\code\\api-server");
    let web = row("web", "D:\\code\\web");
    let listing = build(&[&api, &web], "API stag");
    assert_eq!(keys(&listing), ["api-server/staged", "open_folder"]);
    let listing = build(&[&api, &web], "web");
    assert_eq!(
        keys(&listing),
        ["web/working", "web/staged", "web/branch", "open_folder"]
    );
    let listing = build(&[&api, &web], "nothing like it");
    assert!(listing.entries.is_empty());
    assert_eq!(keys(&listing), ["open_folder"], "Open folder… always stays");
    // Repos that share a name are told apart by their paths.
    let twin = row("api-server", "D:\\other\\api-server");
    let listing = build(&[&api, &twin], "staged");
    assert_eq!(
        keys(&listing),
        [
            "D:\\code\\api-server/staged",
            "D:\\other\\api-server/staged",
            "open_folder"
        ]
    );
}

fn switcher(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Option<Entity<Switcher>> {
    shell.read_with(cx, |shell, _| shell.switcher().cloned())
}

fn selected(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Option<String> {
    let switcher = switcher(shell, cx)?;
    switcher.read_with(cx, |switcher, _| {
        switcher.selected_key().map(str::to_string)
    })
}

#[gpui::test]
fn ctrl_o_filters_moves_and_opens_by_keyboard(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let repos = tempfile::tempdir().unwrap();
    let api = fixture_at(&repos.path().join("api-server"));
    let web = fixture_at(&repos.path().join("web"));
    add_repo(&api, &shell, cx);
    add_repo(&web, &shell, cx);
    let api_name = "api-server";

    cx.simulate_keystrokes("secondary-o");
    settle(cx, &shell, "the rows", |_, _| true);
    let view = switcher(&shell, cx).expect("Ctrl O opens the switcher");
    assert!(cx.update(|window, cx| view.read(cx).is_focused(window, cx)));
    let first = selected(&shell, cx).unwrap();
    assert!(first.ends_with("/working"), "{first}");

    // ↓ moves, ↑ wraps back past the top to "Open folder…".
    cx.simulate_keystrokes("down");
    assert!(selected(&shell, cx).unwrap().ends_with("/staged"));
    cx.simulate_keystrokes("up up");
    assert_eq!(selected(&shell, cx).as_deref(), Some("open_folder"));

    // Typing filters to one repository's staged row.
    cx.simulate_input(&format!("{api_name} staged"));
    settle(cx, &shell, "the filter", |_, _| true);
    view.read_with(cx, |view, _| {
        let keys: Vec<&str> = view
            .listing()
            .items
            .iter()
            .map(|item| item.key.as_str())
            .collect();
        assert_eq!(keys, [format!("{api_name}/staged").as_str(), "open_folder"]);
        assert_eq!(view.query(), format!("{api_name} staged"));
    });
    assert_eq!(selected(&shell, cx), Some(format!("{api_name}/staged")));

    // Enter opens the staged review of that repository and closes the switcher.
    cx.simulate_keystrokes("enter");
    settle(cx, &shell, "the review", |shell, _| {
        shell.session().is_some()
    });
    shell.read_with(cx, |shell, cx| {
        assert!(shell.switcher().is_none());
        let session = shell.session().unwrap().read(cx);
        assert_eq!(session.review().target, Target::Staged);
        assert_eq!(
            session.backend().repo().location,
            Location::Local(api.path().to_path_buf())
        );
    });

    // Opened again, it selects the open review's row; Esc closes it and the keyboard goes
    // back to the shell.
    cx.simulate_keystrokes("secondary-o");
    settle(cx, &shell, "the rows", |_, _| true);
    assert_eq!(selected(&shell, cx), Some(format!("{api_name}/staged")));
    cx.simulate_keystrokes("escape");
    assert!(switcher(&shell, cx).is_none());
    let focus = shell.read_with(cx, |shell, cx| shell.focus_handle(cx));
    assert!(cx.update(|window, _| focus.is_focused(window)));
}

#[gpui::test]
fn open_selects_the_named_repo_and_open_folder_adds_one(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let repos = tempfile::tempdir().unwrap();
    let api = fixture_at(&repos.path().join("api-server"));
    let web = fixture_at(&repos.path().join("web"));
    add_repo(&api, &shell, cx);
    add_repo(&web, &shell, cx);
    let web_location = Location::Local(web.path().to_path_buf());
    let web_name = "web";
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_switcher(Some(&web_location), window, cx)
        })
    });
    settle(cx, &shell, "the rows", |_, _| true);
    assert_eq!(selected(&shell, cx), Some(format!("{web_name}/working")));

    // "Open folder…" goes through the scripted picker; the folder's repo is added and
    // selected.
    let third = fixture_at(&repos.path().join("third"));
    crate::test_support::register(&third, cx);
    cx.update(|_, cx| crate::system::queue_answer(Some(third.path().to_path_buf()), cx))
        .unwrap();
    let view = switcher(&shell, cx).unwrap();
    view.update(cx, |view, cx| assert!(view.select_key("open_folder", cx)));
    cx.simulate_keystrokes("enter");
    settle(cx, &shell, "the new repo", |shell, cx| {
        shell.switcher().is_some_and(|switcher| {
            switcher
                .read(cx)
                .selected_key()
                .is_some_and(|key| key == "third/working")
        })
    });
    let (requests, _) = cx.update(|_, cx| crate::system::requests(cx));
    assert_eq!(
        requests.last().map(crate::system::SystemRequest::kind),
        Some("pick_folder")
    );
    let known = cx.update(|_, cx| {
        crate::ActiveServices::services(cx)
            .reviews
            .read(cx)
            .rows()
            .len()
    });
    assert_eq!(known, 3);
}

#[gpui::test]
fn the_source_picker_switches_the_target_by_keyboard(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let repos = tempfile::tempdir().unwrap();
    let api = fixture_at(&repos.path().join("api-server"));
    api.branch("feat");
    api.checkout("feat");
    add_repo(&api, &shell, cx);
    let location = Location::Local(api.path().to_path_buf());
    let task = cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open(
                location.clone(),
                OpenRequest::Target(Target::Working),
                window,
                cx,
            )
        })
    });
    cx.run_until_parked();
    settle(cx, &shell, "the review", |shell, _| {
        shell.session().is_some()
    });
    let working = cx.foreground_executor().block_test(task).unwrap();

    cx.update(|window, cx| shell.update(cx, |shell, cx| shell.open_source_picker(window, cx)));
    settle(cx, &shell, "the repo's row", |_, _| true);
    let picker = shell.read_with(cx, |shell, _| shell.source_picker().cloned().unwrap());
    picker.read_with(cx, |picker, _| {
        let offers: Vec<(&str, &str, Option<&ReviewId>)> = picker
            .offers()
            .iter()
            .map(|offer| {
                (
                    offer.kind.key(),
                    offer.label.as_str(),
                    offer.review.as_ref().map(|badge| &badge.id),
                )
            })
            .collect();
        assert_eq!(
            offers,
            [
                ("working", "Uncommitted changes", Some(&working.review)),
                ("staged", "Staged changes", None),
                ("branch", "Branch feat vs main", None),
            ]
        );
        assert_eq!(
            picker.selected().map(|offer| offer.kind.key()),
            Some("working")
        );
    });
    // "Compare branches…" follows the targets: the repository has a branch to compare.
    assert!(picker.read_with(cx, |picker, _| picker.offers_compare()));
    cx.simulate_keystrokes("down down down");
    assert!(picker.read_with(cx, |picker, _| picker.compare_selected()));
    cx.simulate_keystrokes("down");
    picker.read_with(cx, |picker, _| {
        assert_eq!(
            picker.selected().map(|offer| offer.kind.key()),
            Some("working"),
            "wraps"
        )
    });
    cx.simulate_keystrokes("up up enter");
    settle(cx, &shell, "the branch review", |shell, cx| {
        shell.session().is_some_and(|session| {
            matches!(session.read(cx).review().target, Target::Branch { .. })
        })
    });
    shell.read_with(cx, |shell, cx| {
        assert!(shell.source_picker().is_none());
        assert_eq!(
            shell.session().unwrap().read(cx).target_label(),
            "feat vs main"
        );
    });
}

#[gpui::test]
fn a_start_keeps_its_row_and_the_selection_and_enter_again_does_nothing(cx: &mut TestAppContext) {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use dv_backend::{Connection, ConnectionState, Launcher};
    use gpui::Keystroke;

    use crate::services::{ActiveServices as _, Route};
    use crate::test_support::fixture;

    let (_dir, shell, cx) = open_shell(cx);
    let repo = fixture();
    // A host the test stops, as the user stops a distro; respawns it may block.
    let up = Arc::new(AtomicBool::new(true));
    let blocked = Arc::new(AtomicBool::new(false));
    let launcher = Launcher::custom("an in-process server on a host the test stops", {
        let (up, blocked) = (up.clone(), blocked.clone());
        move || {
            if blocked.load(Ordering::SeqCst) {
                return Err(std::io::Error::other("blocked by the test"));
            }
            up.store(true, Ordering::SeqCst);
            Launcher::InProcess.start()
        }
    })
    .with_host_check({
        let up = up.clone();
        move |_| up.load(Ordering::SeqCst)
    });
    let connection =
        Connection::with_backoff("loopback", launcher, vec![Duration::from_millis(20)]);
    let (locations, reviews) = cx.update(|_, cx| {
        (
            cx.services().locations.clone(),
            cx.services().reviews.clone(),
        )
    });
    locations.update(cx, |locations, _| {
        locations.register(
            repo.path().to_path_buf(),
            Route {
                env: repo.env_overlay(),
                connection: Some(connection.clone()),
            },
        )
    });
    let location = Location::Local(repo.path().to_path_buf());
    let added = reviews.update(cx, |reviews, cx| reviews.add_repo(location.clone(), cx));
    settle(cx, &shell, "the repo's row", |_, _| true);
    assert!(cx.foreground_executor().block_test(added).is_ok());
    let window = cx.update(|window, _| window.window_handle());

    for (case, row) in [("stopped", "start"), ("failed", "retry")] {
        // Stopped by the user; or failed, its respawns blocked.
        match case {
            "stopped" => up.store(false, Ordering::SeqCst),
            _ => blocked.store(true, Ordering::SeqCst),
        }
        connection.kill();
        settle(cx, &shell, "the connection down", |_, _| {
            matches!(
                connection.state(),
                ConnectionState::Stopped | ConnectionState::Failed { .. }
            )
        });
        blocked.store(false, Ordering::SeqCst);
        cx.simulate_keystrokes("secondary-o");
        settle(cx, &shell, "the Start or Retry row", |shell, cx| {
            shell.switcher().is_some_and(|switcher| {
                switcher
                    .read(cx)
                    .selected_key()
                    .is_some_and(|key| key.ends_with(&format!("/{row}")))
            })
        });
        let picked = selected(&shell, cx).unwrap();
        let key = picked.trim_end_matches(&format!("/{row}")).to_string();
        let view = switcher(&shell, cx).unwrap();
        let events = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update(|_, cx| {
            let events = events.clone();
            cx.subscribe(&view, move |_, event: &SwitcherEvent, _| {
                events.borrow_mut().push(event.clone())
            })
        });

        // Enter starts it. Before the start lands its row stays, inert, with the selection,
        // and Enter again picks nothing: no second start, never the folder picker.
        cx.dispatch_keystroke(window, Keystroke::parse("enter").unwrap());
        assert_eq!(
            *events.borrow(),
            [SwitcherEvent::Start {
                location: location.clone()
            }],
            "{case}"
        );
        let item = view.read_with(cx, |view, _| {
            let listing = view.listing();
            listing
                .position(&picked)
                .map(|ix| listing.items[ix].clone())
        });
        assert!(item.expect("the row stays").is_starting(), "{case}");
        assert_eq!(selected(&shell, cx).as_deref(), Some(picked.as_str()));
        cx.dispatch_keystroke(window, Keystroke::parse("enter").unwrap());
        assert_eq!(events.borrow().len(), 1, "{case}");
        assert_eq!(selected(&shell, cx).as_deref(), Some(picked.as_str()));

        // Connected: the repo lists, and its first row has the selection.
        let working = format!("{key}/working");
        settle(cx, &shell, "the repo's rows", |shell, cx| {
            shell
                .switcher()
                .is_some_and(|switcher| switcher.read(cx).selected_key() == Some(working.as_str()))
        });
        assert_eq!(events.borrow().len(), 1, "{case}");
        let (requests, _) = cx.update(|_, cx| crate::system::requests(cx));
        assert!(requests.is_empty(), "{case}: {requests:?}");
        assert_eq!(connection.state(), ConnectionState::Connected);
        cx.simulate_keystrokes("escape");
    }
}

#[gpui::test]
fn an_open_switcher_follows_a_connection_going_down_and_coming_back(cx: &mut TestAppContext) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use dv_backend::{Connection, ConnectionState, Launcher};

    use crate::services::{ActiveServices as _, Route};
    use crate::test_support::fixture;

    let (_dir, shell, cx) = open_shell(cx);
    let repo = fixture();
    let blocked = Arc::new(AtomicBool::new(false));
    let launcher = Launcher::custom("an in-process server tests block", {
        let blocked = blocked.clone();
        move || {
            if blocked.load(Ordering::SeqCst) {
                return Err(std::io::Error::other("blocked by the test"));
            }
            Launcher::InProcess.start()
        }
    });
    let connection =
        Connection::with_backoff("loopback", launcher, vec![Duration::from_millis(1500)]);
    let (locations, reviews) = cx.update(|_, cx| {
        (
            cx.services().locations.clone(),
            cx.services().reviews.clone(),
        )
    });
    locations.update(cx, |locations, _| {
        locations.register(
            repo.path().to_path_buf(),
            Route {
                env: repo.env_overlay(),
                connection: Some(connection.clone()),
            },
        )
    });
    let location = Location::Local(repo.path().to_path_buf());
    let added = reviews.update(cx, |reviews, cx| reviews.add_repo(location.clone(), cx));
    settle(cx, &shell, "the repo's row", |_, _| true);
    assert!(cx.foreground_executor().block_test(added).is_ok());
    cx.simulate_keystrokes("secondary-o");
    settle(cx, &shell, "the rows", |_, _| true);
    let listing = |cx: &mut VisualTestContext| {
        switcher(&shell, cx)
            .expect("open")
            .read_with(cx, |switcher, _| switcher.listing().clone())
    };
    let key = listing(cx).repos[0].key.clone();
    let working = format!("{key}/working");
    assert!(keys(&listing(cx)).contains(&working.as_str()));

    // Its connection dies: the open switcher hears it, says so, and keeps the rows.
    connection.kill();
    settle(cx, &shell, "the reconnecting row", |shell, cx| {
        shell.switcher().is_some_and(|switcher| {
            switcher.read(cx).listing().repos[0].summary
                == RepoSummary::Unreachable("reconnecting…".into())
        })
    });
    assert!(keys(&listing(cx)).contains(&working.as_str()));
    // Back: listed as before.
    settle(cx, &shell, "the row back", |shell, cx| {
        shell.switcher().is_some_and(|switcher| {
            matches!(
                switcher.read(cx).listing().repos[0].summary,
                RepoSummary::Changes(_)
            )
        })
    });

    // Down for good: Retry, which connects again.
    blocked.store(true, Ordering::SeqCst);
    connection.kill();
    let retry = format!("{key}/retry");
    settle(cx, &shell, "the Retry row", |shell, cx| {
        shell
            .switcher()
            .is_some_and(|switcher| switcher.read(cx).listing().position(&retry).is_some())
    });
    blocked.store(false, Ordering::SeqCst);
    let picked = switcher(&shell, cx)
        .unwrap()
        .update(cx, |switcher, cx| switcher.select_key(&retry, cx));
    assert!(picked);
    cx.simulate_keystrokes("enter");
    settle(cx, &shell, "the row back", |shell, cx| {
        shell.switcher().is_some_and(|switcher| {
            let listing = switcher.read(cx).listing();
            listing.position(&working).is_some() && listing.position(&retry).is_none()
        })
    });
    assert_eq!(connection.state(), ConnectionState::Connected);
}

fn branch(name: &str, stack_base: Option<&str>) -> Branch {
    Branch {
        name: name.to_string(),
        tip: Oid::parse(&"a".repeat(40)).unwrap(),
        committed_at: 0,
        stack_base: stack_base.map(str::to_string),
    }
}

/// A repository with the stack `main → feat-a → feat-b` and an unrelated `side`.
fn stacked_row() -> RepoRow {
    let mut api = row("api-server", "D:\\code\\api-server");
    api.branches = vec![
        branch("feat-b", Some("feat-a")),
        branch("side", None),
        branch("feat-a", None),
        branch("main", None),
    ];
    api
}

fn details(listing: &items::Listing) -> Vec<(&str, Option<String>)> {
    listing
        .items
        .iter()
        .map(|item| (item.key.as_str(), item.detail()))
        .collect()
}

#[test]
fn a_repo_with_branches_offers_compare_branches_last() {
    let api = stacked_row();
    let listing = build(&[&api], "");
    assert_eq!(
        keys(&listing),
        [
            "api-server/working",
            "api-server/staged",
            "api-server/branch",
            "api-server/compare",
            "open_folder",
        ]
    );
    assert_eq!(listing.items[3].label(), "Compare branches…");
    assert_eq!(listing.request(3), None, "it leads to the branch picker");
    // Only the default branch: nothing to compare.
    let mut lone = row("web", "D:\\code\\web");
    lone.branches = vec![branch("main", None)];
    assert!(!has_branches(&lone));
    assert!(!keys(&build(&[&lone], "")).contains(&"web/compare"));
}

#[test]
fn the_branch_picker_offers_each_branch_against_its_stack_base() {
    let mut api = stacked_row();
    let stacked = summary(
        Target::Branch {
            branch: "feat-b".into(),
            base: Some("feat-a".into()),
        },
        "D:\\code\\api-server",
        ReviewStatus::ChangesRequested,
    );
    api.reviews.push(ReviewRow::Ok {
        summary: Box::new(stacked.clone()),
        label: "feat-b vs feat-a".into(),
    });
    let listing = build_branches(&[&api], &api.location, "");
    // Newest first, the default branch left out.
    assert_eq!(
        details(&listing),
        [
            ("api-server/pick/feat-b", Some("vs feat-a".to_string())),
            ("api-server/pick/side", Some("vs main".to_string())),
            ("api-server/pick/feat-a", Some("vs main".to_string())),
        ]
    );
    assert_eq!(
        listing.items[0].review().map(|badge| &badge.id),
        Some(&stacked.id)
    );
    assert_eq!(listing.items[1].review(), None);
    assert_eq!(listing.request(0), None, "a branch leads to its bases");
    assert_eq!(listing.repos.len(), 1);
    // The filter keeps the branches holding every word.
    assert_eq!(
        keys(&build_branches(&[&api], &api.location, "feat")),
        ["api-server/pick/feat-b", "api-server/pick/feat-a"]
    );
}

#[test]
fn the_bases_list_the_stack_base_then_the_default_branch_then_the_rest() {
    let api = stacked_row();
    let listing = build_bases(&[&api], &api.location, "feat-b", "");
    assert_eq!(
        details(&listing),
        [
            ("api-server/base/feat-a", Some("stack base".to_string())),
            ("api-server/base/main", Some("default branch".to_string())),
            ("api-server/base/side", None),
        ]
    );
    // Against its stack base the target names the base; against the default branch it is the
    // target with no base, the switcher's own Branch row's and `dv review new --branch`'s.
    assert_eq!(
        listing.request(0),
        Some((
            api.location.clone(),
            OpenRequest::Target(Target::Branch {
                branch: "feat-b".into(),
                base: Some("feat-a".into()),
            })
        ))
    );
    assert_eq!(
        listing.request(1),
        Some((
            api.location.clone(),
            OpenRequest::Target(Target::Branch {
                branch: "feat-b".into(),
                base: None,
            })
        ))
    );
    // A branch with no stack base starts with the default branch.
    let listing = build_bases(&[&api], &api.location, "feat-a", "");
    assert_eq!(
        keys(&listing),
        [
            "api-server/base/main",
            "api-server/base/feat-b",
            "api-server/base/side"
        ]
    );
    assert!(matches!(
        listing.items[0].kind,
        ItemKind::Base {
            note: Some("default branch"),
            ..
        }
    ));
}

/// A repository with the stack `main → feat-a → feat-b`, `feat-b` checked out.
fn stacked_fixture(path: &std::path::Path) -> dv_test_support::FixtureRepo {
    let repo = fixture_at(path);
    repo.commit("wip");
    repo.branch("feat-a")
        .checkout("feat-a")
        .write("a.txt", "a\n");
    repo.commit("a1");
    repo.branch("feat-b")
        .checkout("feat-b")
        .write("b.txt", "b\n");
    repo.commit("b1");
    repo
}

fn mode(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Option<Mode> {
    let switcher = switcher(shell, cx)?;
    switcher.read_with(cx, |switcher, _| Some(switcher.mode().clone()))
}

#[gpui::test]
fn the_branch_picker_opens_a_stacked_branch_against_its_base(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let repos = tempfile::tempdir().unwrap();
    let api = stacked_fixture(&repos.path().join("api-server"));
    add_repo(&api, &shell, cx);
    let location = Location::Local(api.path().to_path_buf());

    cx.simulate_keystrokes("secondary-o");
    settle(cx, &shell, "the rows", |_, _| true);
    let view = switcher(&shell, cx).expect("Ctrl O opens the switcher");
    assert!(view.update(cx, |view, cx| view.select_key("api-server/compare", cx)));

    // Enter on "Compare branches…" lists the branches, the checked-out one stacked on feat-a.
    cx.simulate_keystrokes("enter");
    assert_eq!(
        mode(&shell, cx),
        Some(Mode::Branches {
            location: location.clone()
        })
    );
    view.read_with(cx, |view, _| {
        assert_eq!(
            details(view.listing()),
            [
                ("api-server/pick/feat-b", Some("vs feat-a".to_string())),
                ("api-server/pick/feat-a", Some("vs main".to_string())),
            ]
        );
    });
    assert_eq!(
        selected(&shell, cx).as_deref(),
        Some("api-server/pick/feat-b")
    );
    // Ctrl Shift Backspace removes repositories from their own list only.
    cx.simulate_keystrokes("secondary-shift-backspace");
    settle(cx, &shell, "nothing removed", |_, _| true);
    let rows = cx.update(|_, cx| {
        crate::ActiveServices::services(cx)
            .reviews
            .read(cx)
            .rows()
            .len()
    });
    assert_eq!(rows, 1);

    // Enter lists its bases, the stack base first; Esc steps back to the branches, and again
    // to the repositories, where "Compare branches…" keeps the selection.
    cx.simulate_keystrokes("enter");
    assert_eq!(
        mode(&shell, cx),
        Some(Mode::Bases {
            location: location.clone(),
            branch: "feat-b".into()
        })
    );
    assert_eq!(
        selected(&shell, cx).as_deref(),
        Some("api-server/base/feat-a")
    );
    cx.simulate_keystrokes("escape");
    assert_eq!(
        selected(&shell, cx).as_deref(),
        Some("api-server/pick/feat-b")
    );
    cx.simulate_keystrokes("escape");
    assert_eq!(mode(&shell, cx), Some(Mode::Repos));
    assert_eq!(selected(&shell, cx).as_deref(), Some("api-server/compare"));

    // Into the picker again, the filter narrows the branches, and the pair opens.
    cx.simulate_keystrokes("enter");
    cx.simulate_input("feat-b");
    settle(cx, &shell, "the filter", |_, _| true);
    cx.simulate_keystrokes("enter enter");
    settle(cx, &shell, "the stacked review", |shell, _| {
        shell.session().is_some()
    });
    shell.read_with(cx, |shell, cx| {
        assert!(shell.switcher().is_none());
        let session = shell.session().unwrap().read(cx);
        assert_eq!(
            session.review().target,
            Target::Branch {
                branch: "feat-b".into(),
                base: Some("feat-a".into()),
            }
        );
        assert_eq!(session.target_label(), "feat-b vs feat-a");
        // Only feat-b's own commit, not feat-a's.
        let files: Vec<&str> = session
            .target_files()
            .iter()
            .map(|entry| entry.path.as_str())
            .collect();
        assert_eq!(files, ["b.txt"]);
    });
}

#[gpui::test]
fn the_source_picker_opens_the_branch_picker_and_esc_closes_it(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let repos = tempfile::tempdir().unwrap();
    let api = stacked_fixture(&repos.path().join("api-server"));
    add_repo(&api, &shell, cx);
    let location = Location::Local(api.path().to_path_buf());
    let task = cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open(
                location.clone(),
                OpenRequest::Target(Target::Working),
                window,
                cx,
            )
        })
    });
    cx.run_until_parked();
    settle(cx, &shell, "the review", |shell, _| {
        shell.session().is_some()
    });
    cx.foreground_executor().block_test(task).unwrap();

    cx.update(|window, cx| shell.update(cx, |shell, cx| shell.open_source_picker(window, cx)));
    settle(cx, &shell, "the repo's row", |_, _| true);
    cx.simulate_keystrokes("up enter");
    assert!(shell.read_with(cx, |shell, _| shell.source_picker().is_none()));
    assert_eq!(
        mode(&shell, cx),
        Some(Mode::Branches {
            location: location.clone()
        })
    );
    // Opened on the picker, Esc in its first step closes the switcher.
    cx.simulate_keystrokes("escape");
    assert!(switcher(&shell, cx).is_none());
}

#[gpui::test]
fn a_repo_is_removed_by_key_or_menu_but_not_the_open_reviews(cx: &mut TestAppContext) {
    use crate::services::ActiveServices as _;
    let (_dir, shell, cx) = open_shell(cx);
    let repos = tempfile::tempdir().unwrap();
    let api = fixture_at(&repos.path().join("api-server"));
    let web = fixture_at(&repos.path().join("web"));
    let docs = fixture_at(&repos.path().join("docs"));
    for repo in [&api, &web, &docs] {
        add_repo(repo, &shell, cx);
    }
    let api_location = Location::Local(api.path().to_path_buf());
    let task = cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open(
                api_location.clone(),
                OpenRequest::Target(Target::Working),
                window,
                cx,
            )
        })
    });
    cx.run_until_parked();
    settle(cx, &shell, "the review", |shell, _| {
        shell.session().is_some()
    });
    cx.foreground_executor().block_test(task).unwrap();
    let known = |shell: &Entity<Shell>, cx: &mut VisualTestContext| {
        shell.read_with(cx, |_, cx| {
            let mut names: Vec<String> = cx
                .services()
                .reviews
                .read(cx)
                .rows()
                .iter()
                .map(|row| row.name.clone())
                .collect();
            names.sort();
            names
        })
    };

    cx.simulate_keystrokes("secondary-o");
    settle(cx, &shell, "the rows", |_, _| true);
    let view = switcher(&shell, cx).expect("Ctrl O opens the switcher");
    // The open review's repository: the key does nothing, and its menu's item is disabled.
    assert_eq!(selected(&shell, cx).as_deref(), Some("api-server/working"));
    cx.simulate_keystrokes("secondary-shift-backspace");
    assert_eq!(known(&shell, cx), ["api-server", "docs", "web"]);
    view.update(cx, |view, cx| {
        assert_eq!(view.removable(), None);
        view.open_menu(
            api_location.clone(),
            gpui::point(gpui::px(10.), gpui::px(10.)),
            cx,
        );
        view.remove(&api_location, cx);
        assert!(view.menu().is_none(), "the menu closes");
    });
    cx.run_until_parked();
    assert_eq!(known(&shell, cx), ["api-server", "docs", "web"]);

    // From the filter field, Ctrl Shift Backspace removes the selected row's repository; the
    // selection moves to the next repository's first row.
    let order: Vec<String> = view.read_with(cx, |view, _| {
        view.listing()
            .repos
            .iter()
            .map(|repo| repo.key.clone())
            .collect()
    });
    let (middle, after) = (order[1].clone(), order[2].clone());
    assert!(view.update(cx, |view, cx| {
        view.select_key(&format!("{middle}/staged"), cx)
    }));
    assert!(cx.update(|window, cx| view.read(cx).is_focused(window, cx)));
    cx.simulate_keystrokes("secondary-shift-backspace");
    settle(cx, &shell, "the removal", |_, _| true);
    let mut left = vec!["api-server".to_string(), after.clone()];
    left.sort();
    assert_eq!(known(&shell, cx), left);
    assert_eq!(selected(&shell, cx), Some(format!("{after}/working")));

    // The menu's Remove from list does the same; Esc closes an open menu first.
    let after_location = view.read_with(cx, |view, _| {
        view.listing()
            .repos
            .iter()
            .find(|repo| repo.key == after)
            .unwrap()
            .location
            .clone()
    });
    view.update(cx, |view, cx| {
        view.open_menu(
            after_location.clone(),
            gpui::point(gpui::px(10.), gpui::px(10.)),
            cx,
        )
    });
    cx.simulate_keystrokes("escape");
    assert!(view.read_with(cx, |view, _| view.menu().is_none()));
    assert!(
        switcher(&shell, cx).is_some(),
        "Esc closed the menu, not the switcher"
    );
    view.update(cx, |view, cx| view.remove(&after_location, cx));
    settle(cx, &shell, "the removal", |_, _| true);
    assert_eq!(known(&shell, cx), ["api-server"]);
}

#[gpui::test]
fn a_review_is_closed_from_its_rows_menu_or_by_key_once_asked(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let repos = tempfile::tempdir().unwrap();
    let api = stacked_fixture(&repos.path().join("api-server"));
    add_repo(&api, &shell, cx);
    let location = Location::Local(api.path().to_path_buf());
    // Three open reviews: of another branch (a row of its own), of the staged changes, and
    // of the working tree, which is the one left open in the window.
    for target in [
        Target::Branch {
            branch: "feat-a".into(),
            base: None,
        },
        Target::Staged,
        Target::Working,
    ] {
        let task = cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.open(location.clone(), OpenRequest::Target(target), window, cx)
            })
        });
        cx.run_until_parked();
        settle(cx, &shell, "the review", |shell, _| {
            shell.session().is_some()
        });
        cx.foreground_executor().block_test(task).unwrap();
    }
    let review_of = |shell: &Entity<Shell>, cx: &mut VisualTestContext, key: &str| {
        let view = switcher(shell, cx).unwrap();
        view.read_with(cx, |view, _| {
            let listing = view.listing();
            listing
                .position(key)
                .and_then(|ix| listing.items[ix].review().cloned())
        })
    };

    cx.simulate_keystrokes("secondary-o");
    settle(cx, &shell, "the rows", |_, _| true);
    let view = switcher(&shell, cx).expect("Ctrl O opens the switcher");
    let other = view.read_with(cx, |view, _| {
        keys(view.listing())
            .into_iter()
            .find(|key| key.starts_with("api-server/review/"))
            .expect("the other branch's review has a row")
            .to_string()
    });

    // Ctrl Shift W only asks: Esc keeps the review and the switcher, and so does moving on.
    assert!(view.update(cx, |view, cx| view.select_key("api-server/staged", cx)));
    assert!(view.read_with(cx, |view, _| view.closable()));
    cx.simulate_keystrokes("secondary-shift-w");
    assert_eq!(
        view.read_with(cx, |view, _| view.closing().map(str::to_string)),
        Some("api-server/staged".to_string())
    );
    cx.simulate_keystrokes("escape");
    assert_eq!(
        view.read_with(cx, |view, _| view.closing().is_some()),
        false
    );
    assert!(switcher(&shell, cx).is_some(), "Esc kept the switcher");
    cx.simulate_keystrokes("secondary-shift-w down");
    assert_eq!(
        view.read_with(cx, |view, _| view.closing().is_some()),
        false
    );
    settle(cx, &shell, "nothing", |_, _| true);
    assert!(review_of(&shell, cx, "api-server/staged").is_some());

    // Asked, Enter closes it: the row stays, selected, and offers a new review.
    assert!(view.update(cx, |view, cx| view.select_key("api-server/staged", cx)));
    cx.simulate_keystrokes("secondary-shift-w enter");
    settle(cx, &shell, "the close", |shell, cx| {
        shell.switcher().is_some_and(|view| {
            let view = view.read(cx);
            view.listing()
                .position("api-server/staged")
                .is_some_and(|ix| view.listing().items[ix].review().is_none())
        })
    });
    assert_eq!(selected(&shell, cx).as_deref(), Some("api-server/staged"));
    assert!(shell.read_with(cx, |shell, _| shell.session().is_some()));

    // A row without a review has nothing to close: the key does nothing there, nor a menu.
    cx.simulate_keystrokes("secondary-shift-w");
    view.update(cx, |view, cx| {
        assert!(!view.closable());
        assert_eq!(view.closing(), None);
        view.open_row_menu(
            "api-server/staged",
            gpui::point(gpui::px(10.), gpui::px(10.)),
            cx,
        );
        assert!(view.menu().is_none());
    });

    // Another target's review, from its row's menu (Esc closes the menu first): its row
    // goes, and the selection moves to the next one.
    view.update(cx, |view, cx| {
        view.open_row_menu(&other, gpui::point(gpui::px(10.), gpui::px(10.)), cx);
        assert_eq!(
            view.menu().map(|menu| menu.target.clone()),
            Some(MenuTarget::Review { row: other.clone() })
        );
    });
    assert_eq!(selected(&shell, cx), Some(other.clone()));
    cx.simulate_keystrokes("escape");
    assert!(view.read_with(cx, |view, _| view.menu().is_none()));
    assert!(switcher(&shell, cx).is_some(), "Esc closed the menu only");
    let next = view.read_with(cx, |view, _| {
        let listing = view.listing();
        listing.items[listing.position(&other).unwrap() + 1]
            .key
            .clone()
    });
    view.update(cx, |view, cx| view.close_review(&other, cx));
    settle(cx, &shell, "the close", |shell, cx| {
        shell
            .switcher()
            .is_some_and(|view| view.read(cx).listing().position(&other).is_none())
    });
    assert_eq!(selected(&shell, cx), Some(next));

    // The open review closes the same way; the window shows it closed.
    view.update(cx, |view, cx| view.close_review("api-server/working", cx));
    settle(cx, &shell, "the open review closed", |shell, cx| {
        shell
            .session()
            .is_some_and(|session| session.read(cx).review().status == ReviewStatus::Closed)
    });
    assert!(review_of(&shell, cx, "api-server/working").is_none());
    assert_eq!(
        view.read_with(cx, |view, _| view.close_failure().is_some()),
        false
    );
}
