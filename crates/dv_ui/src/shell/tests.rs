use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use dv_backend::{Connection, Launcher};
use dv_core::{Location, Target};
use gpui::{Modifiers, MouseButton, TestAppContext, VisualTestContext, point};
use pretty_assertions::assert_eq;

use super::status_bar::Health;
use super::*;
use crate::services::{Mode, Route};
use crate::test_support::{add_repo, bounds, fixture, open_shell, settle};
use crate::theme::{ActiveTheme as _, Appearance};

mod pending;

#[gpui::test]
fn the_empty_shell_shows_the_no_review_state(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    cx.run_until_parked();
    cx.update(|window, cx| {
        let view = shell.read(cx);
        assert!(view.panel_visible());
        assert_eq!(view.empty_state(cx), Some(EmptyState::NoReview));
        assert_eq!(
            view.applied_material(),
            crate::appearance::default_material()
        );
        let status = view.status_line(cx);
        assert_eq!(status.health, Health::Idle);
        assert_eq!(status.location, "No repo");
        assert!(!status.finish);
        assert!(
            view.main_focus(cx).is_focused(window),
            "the shell has the keyboard, so Ctrl O works at once"
        );
    });
}

#[gpui::test]
fn a_material_change_reaches_the_window(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let settings = cx.update(|_, cx| cx.services().settings.clone());
    for material in [
        Material::Acrylic,
        Material::Opaque,
        Material::Vibrancy,
        Material::Mica,
    ] {
        settings.update(cx, |settings, cx| settings.set_material(material, cx));
        cx.run_until_parked();
        shell.read_with(cx, |shell, _| {
            assert_eq!(shell.applied_material(), material)
        });
    }
    settings.update(cx, |settings, cx| settings.set_mode(Mode::Light, cx));
    cx.run_until_parked();
    cx.update(|_, cx| assert_eq!(cx.dv_theme().appearance, Appearance::Light));
}

#[gpui::test]
fn the_window_opens_at_its_size_with_the_material(cx: &mut TestAppContext) {
    let (_dir, _shell, cx) = open_shell(cx);
    let options = cx.update(|_, cx| window_options(size(px(1440.), px(900.)), cx));
    assert_eq!(
        options.window_background,
        crate::appearance::default_material().window_background(gpui_appearance::Backing::Solid)
    );
    assert_eq!(options.window_min_size, Some(size(px(1100.), px(700.))));
    let Some(WindowBounds::Windowed(bounds)) = options.window_bounds else {
        panic!("a windowed window");
    };
    assert_eq!(bounds.size, size(px(1440.), px(900.)));
}

#[gpui::test]
fn ctrl_b_toggles_the_panel(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    assert_eq!(bounds(cx, "panel").map(|b| b.size.width), Some(px(280.)));
    cx.simulate_keystrokes("secondary-b");
    assert!(!shell.read_with(cx, |shell, _| shell.panel_visible()));
    assert_eq!(bounds(cx, "panel"), None);
    assert_eq!(
        bounds(cx, "main").map(|b| (b.origin.x, b.size.width)),
        Some((px(0.), px(1440.))),
        "the main area takes the whole width"
    );
    cx.simulate_keystrokes("secondary-b");
    assert!(shell.read_with(cx, |shell, _| shell.panel_visible()));
    assert_eq!(bounds(cx, "panel").map(|b| b.size.width), Some(px(280.)));
}

fn panel_width(cx: &mut VisualTestContext) -> f32 {
    cx.update(|_, cx| cx.services().settings.read(cx).panel_width())
}

#[gpui::test]
fn dragging_the_panel_edge_resizes_within_limits_and_is_saved(cx: &mut TestAppContext) {
    let (dir, shell, cx) = open_shell(cx);
    let handle = bounds(cx, "panel-resize-handle").expect("the handle");
    let start = handle.center();
    let drag_to = |cx: &mut VisualTestContext, x: f32| {
        cx.simulate_mouse_move(point(px(x), start.y), MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
    };
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
    // The first move past the threshold starts the drag; later moves resize.
    drag_to(cx, 300.);
    drag_to(cx, 330.);
    assert_eq!(panel_width(cx), 330.);
    drag_to(cx, 900.);
    assert_eq!(panel_width(cx), 420., "clamped to the maximum");
    drag_to(cx, 10.);
    assert_eq!(panel_width(cx), 220., "clamped to the minimum");
    drag_to(cx, 300.);
    cx.simulate_mouse_up(
        point(px(300.), start.y),
        MouseButton::Left,
        Modifiers::none(),
    );
    assert_eq!(bounds(cx, "panel").map(|b| b.size.width), Some(px(300.)));
    settle(cx, &shell, "the settings to be saved", |_, _| true);
    let saved = std::fs::read_to_string(dir.path().join("settings.json")).unwrap();
    assert!(saved.contains("\"panel_width\": 300.0"), "{saved}");

    // A double click on the edge restores the default.
    let handle = bounds(cx, "panel-resize-handle").expect("the handle");
    cx.simulate_event(gpui::MouseDownEvent {
        position: handle.center(),
        modifiers: Modifiers::none(),
        button: MouseButton::Left,
        click_count: 2,
        first_mouse: false,
    });
    cx.simulate_event(gpui::MouseUpEvent {
        position: handle.center(),
        modifiers: Modifiers::none(),
        button: MouseButton::Left,
        click_count: 2,
    });
    assert_eq!(panel_width(cx), 280.);
}

fn open_review(
    shell: &Entity<Shell>,
    location: Location,
    request: OpenRequest,
    cx: &mut VisualTestContext,
) -> Result<Opened, Failure> {
    let task = cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.open(location, request, window, cx))
    });
    cx.run_until_parked();
    settle(cx, shell, "the open to land", |shell, _| {
        shell.opening().is_none()
    });
    cx.foreground_executor().block_test(task)
}

#[gpui::test]
fn an_opened_review_fills_every_region(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let repo = fixture();
    add_repo(&repo, &shell, cx);
    let location = Location::Local(repo.path().to_path_buf());
    let opened = open_review(
        &shell,
        location.clone(),
        OpenRequest::Target(Target::Working),
        cx,
    )
    .expect("the review opens");
    assert!(opened.created);
    settle(cx, &shell, "the file list", |shell, cx| {
        shell
            .session()
            .is_some_and(|session| session.read(cx).files().len() == 3)
    });
    cx.update(|_, cx| {
        let view = shell.read(cx);
        assert_eq!(view.empty_state(cx), None);
        assert_eq!(view.banner().map(|b| b.failure.clone()), None);
        let status = view.status_line(cx);
        assert_eq!(status.health, Health::Live);
        assert_eq!(status.location, "Local · watching for changes");
        assert_eq!(
            status.review.as_deref(),
            Some("Open · round 1 · no comments")
        );
        assert!(status.finish);
        assert_eq!(
            view.file_list().read(cx).shown_paths(cx),
            [
                "README.md",
                "src/routes/users.test.ts",
                "src/routes/users.ts"
            ]
        );
        assert_eq!(
            view.diff_pane().read(cx).current_file(),
            Some("README.md"),
            "the stream starts at the first file"
        );
    });

    // Resuming the open review keeps its session.
    let session = shell.read_with(cx, |shell, _| shell.session().cloned().unwrap());
    let again = open_review(&shell, location, OpenRequest::Target(Target::Working), cx).unwrap();
    assert_eq!(again.review, opened.review);
    assert!(!again.created);
    shell.read_with(cx, |shell, _| {
        assert_eq!(
            shell.session().map(Entity::entity_id),
            Some(session.entity_id())
        )
    });
}

/// Presses `keys`, then draws, so focus changes take effect as they would on screen.
fn press(cx: &mut VisualTestContext, keys: &str) {
    cx.simulate_keystrokes(keys);
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
}

#[gpui::test]
fn closing_an_overlay_never_strands_the_keyboard_on_a_hidden_filter(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let repo = fixture();
    add_repo(&repo, &shell, cx);
    open_review(
        &shell,
        Location::Local(repo.path().to_path_buf()),
        OpenRequest::Target(Target::Working),
        cx,
    )
    .unwrap();
    settle(cx, &shell, "the files", |shell, cx| {
        shell
            .session()
            .is_some_and(|session| session.read(cx).files().len() == 3)
    });
    let current = |cx: &mut VisualTestContext| {
        shell.read_with(cx, |shell, cx| {
            shell
                .diff_pane()
                .read(cx)
                .current_file()
                .map(str::to_string)
        })
    };
    assert_eq!(current(cx).as_deref(), Some("README.md"));
    // The switcher opens from the panel's filter, which the keyboard is to go back to...
    let filter = shell.read_with(cx, |shell, cx| shell.file_list().read(cx).filter_focus(cx));
    cx.update(|window, cx| filter.focus(window, cx));
    press(cx, "secondary-o");
    assert!(shell.read_with(cx, |shell, _| shell.switcher().is_some()));
    // ...but the panel goes away meanwhile.
    press(cx, "secondary-b");
    assert!(!shell.read_with(cx, |shell, _| shell.panel_visible()));
    press(cx, "escape");
    assert!(shell.read_with(cx, |shell, _| shell.switcher().is_none()));
    let main = shell.read_with(cx, |shell, cx| shell.main_focus(cx));
    assert!(
        cx.update(|window, _| main.is_focused(window)),
        "the keyboard is back on the main area"
    );
    // So the stream's and the shell's keys work.
    press(cx, "j");
    assert_eq!(current(cx).as_deref(), Some("src/routes/users.test.ts"));
    press(cx, "secondary-b");
    assert!(shell.read_with(cx, |shell, _| shell.panel_visible()));

    // Whatever else leaves the keyboard on an element that isn't drawn hands it to the main
    // area as the next frame is drawn.
    press(cx, "secondary-b");
    cx.update(|window, cx| filter.focus(window, cx));
    press(cx, "j");
    assert!(cx.update(|window, _| main.is_focused(window)));
    press(cx, "j");
    assert_eq!(current(cx).as_deref(), Some("src/routes/users.ts"));
}

#[gpui::test]
fn a_clean_working_tree_says_there_is_nothing_to_review(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let repo = dv_test_support::FixtureRepo::new();
    repo.write("a.txt", "a\n");
    repo.commit("base");
    add_repo(&repo, &shell, cx);
    open_review(
        &shell,
        Location::Local(repo.path().to_path_buf()),
        OpenRequest::Target(Target::Working),
        cx,
    )
    .unwrap();
    settle(cx, &shell, "the empty list", |shell, cx| {
        shell.empty_state(cx).is_some()
    });
    cx.update(|_, cx| {
        assert_eq!(
            shell.read(cx).empty_state(cx),
            Some(EmptyState::WorkingTreeClean)
        )
    });
    // A refresh (the agent touched a file) keeps saying so while it runs: no first-load
    // skeleton over it, in the main area or the panel.
    let session = shell.read_with(cx, |shell, _| shell.session().cloned().unwrap());
    session.update(cx, |session, cx| {
        session.refresh(crate::session::Batch::default(), cx)
    });
    cx.update(|_, cx| {
        let shell = shell.read(cx);
        assert!(session.read(cx).is_loading(), "the refresh runs");
        assert_eq!(shell.empty_state(cx), Some(EmptyState::WorkingTreeClean));
        assert!(!shell.file_list().read(cx).shows_skeletons());
    });
    settle(cx, &shell, "the refresh", |_, _| true);
    cx.update(|_, cx| {
        assert_eq!(
            shell.read(cx).empty_state(cx),
            Some(EmptyState::WorkingTreeClean)
        )
    });
}

#[gpui::test]
fn a_review_that_does_not_open_shows_a_banner_with_retry(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let wsl = Location::Wsl {
        distro: "Ubuntu".into(),
        path: "/home/kyle/web".into(),
    };
    // Tests reach WSL through the stand-in, which refuses to start anything there. The
    // headline is one line; why, and the connection's diagnostics, are the detail.
    let failure = open_review(&shell, wsl, OpenRequest::Target(Target::Working), cx)
        .expect_err("no distro starts in a test");
    assert_eq!(failure.message, "couldn't open Ubuntu:/home/kyle/web");
    let detail = failure.detail.as_deref().expect("why");
    assert!(
        detail.contains(crate::services::locations::wsl::REFUSED)
            && detail.contains(
                "
history:
"
            ),
        "{detail}"
    );
    cx.update(|_, cx| {
        let view = shell.read(cx);
        let banner = view.banner().expect("a banner");
        assert_eq!(banner.failure, failure);
        assert!(banner.retry.is_some());
        assert_eq!(view.session().map(|_| ()), None);
        assert_eq!(view.empty_state(cx), Some(EmptyState::NoReview));
        let status = view.status_line(cx);
        assert_eq!(status.health, Health::Failing);
        assert_eq!(status.location, "Ubuntu · can't be opened");
    });
    // Retry runs the same open again, which fails the same way.
    cx.update(|window, cx| shell.update(cx, |shell, cx| shell.retry(window, cx)));
    cx.run_until_parked();
    settle(cx, &shell, "the retry", |shell, _| {
        shell.opening().is_none()
    });
    shell.read_with(cx, |shell, _| {
        assert_eq!(
            shell.banner().map(|b| &b.failure.message),
            Some(&failure.message)
        )
    });
}

fn open_repo(
    shell: &Entity<Shell>,
    location: Location,
    if_not_opened: IfNotOpened,
    cx: &mut VisualTestContext,
) -> Result<Location, Failure> {
    let task = cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_repo(location, if_not_opened, window, cx)
        })
    });
    cx.run_until_parked();
    settle(cx, shell, "the repository to be added", |_, _| true);
    cx.foreground_executor().block_test(task)
}

#[gpui::test]
fn dv_open_adds_the_repository_and_opens_the_switcher_on_it(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let repo = fixture();
    crate::test_support::register(&repo, cx);
    // A subdirectory names its work tree.
    let known = open_repo(
        &shell,
        Location::Local(repo.path().join("src")),
        IfNotOpened::Banner,
        cx,
    )
    .expect("a repository opens");
    assert_ne!(known, Location::Local(repo.path().join("src")), "the root");
    cx.update(|_, cx| {
        let view = shell.read(cx);
        assert!(view.switcher().is_some(), "the switcher is open");
        assert_eq!(view.banner().map(|_| ()), None);
        let reviews = cx.services().reviews.read(cx);
        assert_eq!(reviews.rows().len(), 1);
        assert!(reviews.row(&known).is_some(), "known by its root");
    });

    // A path that doesn't open: the banner says why, unless it was only a guess (bare `dv`).
    cx.update(|window, cx| shell.update(cx, |shell, cx| shell.close_menu(window, cx)));
    let missing = Location::Local(repo.path().join("no-such-dir"));
    let failure =
        open_repo(&shell, missing.clone(), IfNotOpened::Ignore, cx).expect_err("not a repository");
    assert!(failure.message.contains("no such directory"), "{failure}");
    shell.read_with(cx, |shell, _| {
        assert_eq!(shell.banner().map(|_| ()), None);
        assert!(shell.switcher().is_none());
    });
    let failure = open_repo(&shell, missing, IfNotOpened::Banner, cx).expect_err("still not one");
    shell.read_with(cx, |shell, _| {
        assert_eq!(shell.banner().map(|b| &b.failure), Some(&failure));
        assert!(shell.switcher().is_none());
    });
}

#[gpui::test]
fn a_ready_review_shows_the_agents_handoff_with_its_note(cx: &mut TestAppContext) {
    use dv_core::review::{Author, ReviewStatus, ops};

    let (_dir, shell, cx) = open_shell(cx);
    let repo = fixture();
    add_repo(&repo, &shell, cx);
    let location = Location::Local(repo.path().to_path_buf());
    open_review(&shell, location, OpenRequest::Target(Target::Working), cx)
        .expect("the review opens");
    let session = shell.read_with(cx, |shell, _| shell.session().cloned().unwrap());
    let (store, review) = session.read_with(cx, |session, _| {
        (session.store().clone(), session.review_id().clone())
    });
    assert_eq!(shell.read_with(cx, |shell, cx| shell.ready_note(cx)), None);
    // The agent runs `dv review done --note …`: the watch brings it in.
    ops::set_status(
        &store,
        &review,
        ReviewStatus::Ready,
        &Author::agent("claude-code"),
        Some("Capped the cache.\n\nSee **cache.ts** for the sweep."),
    )
    .expect("the agent hands the round back");
    settle(cx, &shell, "the handoff", |shell, cx| {
        shell.ready_note(cx).is_some()
    });
    let ready = shell.read_with(cx, |shell, cx| shell.ready_note(cx));
    assert_eq!(
        ready,
        Some(ReadyNote {
            author: Some(Author::agent("claude-code")),
            round: 1,
            note: Some("Capped the cache.\n\nSee **cache.ts** for the sweep.".to_string()),
            annotations: 0,
        })
    );
    // It heads the main area; "Show note" opens the whole note under it.
    let closed = bounds(cx, "ready-banner").expect("the banner").size.height;
    shell.update(cx, |shell, cx| shell.toggle_ready_note(cx));
    let open = bounds(cx, "ready-banner").expect("the banner").size.height;
    assert!(shell.read_with(cx, |shell, _| shell.ready_note_open()));
    assert!(open > closed, "{open:?} vs {closed:?}");
    assert_eq!(bounds(cx, "ready-note-fade"), None, "a short note fits");
    // The reviewer's verdict ends it.
    let finishing = session.update(cx, |session, cx| {
        session.finish_round(Some(dv_core::review::Verdict::RequestChanges), None, cx)
    });
    settle(cx, &shell, "the round", |shell, cx| {
        shell.ready_note(cx).is_none()
    });
    cx.foreground_executor()
        .block_test(finishing)
        .expect("the round finishes");
    assert_eq!(bounds(cx, "ready-banner"), None);

    // The next round's handoff is another banner: its note starts closed, though round 1's
    // was open when the round finished.
    ops::set_status(
        &store,
        &review,
        ReviewStatus::Ready,
        &Author::agent("claude-code"),
        Some("Round two.\n\nThe sweep runs hourly now."),
    )
    .expect("the agent hands round 2 back");
    settle(cx, &shell, "the second handoff", |shell, cx| {
        shell.ready_note(cx).is_some_and(|ready| ready.round == 2)
    });
    assert!(!shell.read_with(cx, |shell, _| shell.ready_note_open()));
    assert_eq!(
        bounds(cx, "ready-banner").expect("the banner").size.height,
        closed,
        "closed, its first line in the header"
    );
    // Saying it again with another note is another handoff too.
    shell.update(cx, |shell, cx| shell.toggle_ready_note(cx));
    assert!(shell.read_with(cx, |shell, _| shell.ready_note_open()));
    std::thread::sleep(std::time::Duration::from_millis(5));
    ops::set_status(
        &store,
        &review,
        ReviewStatus::Ready,
        &Author::agent("claude-code"),
        Some("Round two, again."),
    )
    .expect("the agent says it again");
    settle(cx, &shell, "the new note", |shell, cx| {
        shell
            .ready_note(cx)
            .is_some_and(|ready| ready.note.as_deref() == Some("Round two, again."))
    });
    assert!(!shell.read_with(cx, |shell, _| shell.ready_note_open()));
}

#[gpui::test]
fn a_long_note_or_name_never_pushes_the_handoff_out_of_its_region(cx: &mut TestAppContext) {
    use dv_core::review::{Author, ReviewStatus, ops};

    let (_dir, shell, cx) = open_shell(cx);
    let repo = fixture();
    add_repo(&repo, &shell, cx);
    let location = Location::Local(repo.path().to_path_buf());
    open_review(&shell, location, OpenRequest::Target(Target::Working), cx)
        .expect("the review opens");
    // The widest panel leaves the main area least room.
    cx.update(|_, cx| {
        let settings = cx.services().settings.clone();
        settings.update(cx, |settings, cx| settings.set_panel_width(420., cx));
    });
    let session = shell.read_with(cx, |shell, _| shell.session().cloned().unwrap());
    let (store, review) = session.read_with(cx, |session, _| {
        (session.store().clone(), session.review_id().clone())
    });
    let name = "a-very-long-agent-name-for-a-custom-agent-wrapper".repeat(3);
    let note: String = (1..=60)
        .map(|n| format!("Paragraph {n} of what the agent did this round.\n\n"))
        .collect();
    ops::set_status(
        &store,
        &review,
        ReviewStatus::Ready,
        &Author::agent(name),
        Some(&note),
    )
    .expect("the agent hands the round back");
    settle(cx, &shell, "the handoff", |shell, cx| {
        shell.ready_note(cx).is_some()
    });
    // The name truncates: "Show note" stays inside the banner.
    let banner = bounds(cx, "ready-banner").expect("the banner");
    let toggle = bounds(cx, "ready-note-toggle").expect("Show note");
    assert!(
        toggle.right() <= banner.right(),
        "{toggle:?} runs past {banner:?}"
    );
    // The open note scrolls past 240 high; the banner stays in the main area.
    shell.update(cx, |shell, cx| shell.toggle_ready_note(cx));
    let body = bounds(cx, "ready-note-body").expect("the note");
    assert!(body.size.height <= gpui::px(240.), "{body:?}");
    let main = bounds(cx, "main").expect("the main area");
    let banner = bounds(cx, "ready-banner").expect("the banner");
    // Its clipped foot stops short of the outline, and fades while more lies below...
    let inset = cx.update(|_, cx| cx.dv_theme().spacing.md);
    assert!(
        body.bottom() + inset <= banner.bottom(),
        "{body:?} runs into the outline of {banner:?}"
    );
    let fade = bounds(cx, "ready-note-fade").expect("a fade while the note goes on");
    assert!(fade.bottom() <= body.bottom() + gpui::px(0.5), "{fade:?}");
    // ...but not once it is read to its end.
    shell.update(cx, |shell, cx| {
        let end = shell.ready_note_scroll.max_offset().y;
        shell
            .ready_note_scroll
            .set_offset(gpui::point(gpui::px(0.), -end));
        cx.notify();
    });
    assert_eq!(bounds(cx, "ready-note-fade"), None, "read to its end");
    assert!(banner.bottom() <= main.bottom(), "{banner:?} in {main:?}");
    assert!(
        main.bottom() - banner.bottom() > gpui::px(200.),
        "the stream keeps its room: {banner:?} in {main:?}"
    );
}

/// A loopback connection over an in-process server whose starts fail while `blocked`: one
/// respawn after its server dies, so a death while blocked fails it for good.
fn blockable_loopback(blocked: Arc<AtomicBool>) -> Arc<Connection> {
    let launcher = Launcher::custom("an in-process server tests block", move || {
        if blocked.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("blocked by the test"));
        }
        Launcher::InProcess.start()
    });
    Connection::with_backoff("loopback", launcher, vec![Duration::from_millis(10)])
}

#[gpui::test]
fn a_remote_review_shows_its_connection_and_a_banner_once_it_fails(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let repo = fixture();
    let blocked = Arc::new(AtomicBool::new(false));
    let connection = blockable_loopback(blocked.clone());
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
    let location = Location::Local(repo.path().to_path_buf());
    open_review(&shell, location, OpenRequest::Target(Target::Working), cx)
        .expect("it opens through the loopback");
    settle(cx, &shell, "the file list", |shell, cx| {
        shell
            .session()
            .is_some_and(|session| session.read(cx).files().len() == 3)
    });
    cx.update(|_, cx| {
        let view = shell.read(cx);
        let status = view.status_line(cx);
        assert_eq!(status.health, Health::Live);
        assert_eq!(status.location, "loopback · watching for changes");
        assert_eq!(view.backend_down(cx), None);
    });

    // Its server dies and can't start again: the status bar and the banner say so.
    blocked.store(true, Ordering::SeqCst);
    connection.kill();
    settle(cx, &shell, "the connection to fail", |shell, cx| {
        shell.backend_down(cx).is_some()
    });
    // A refresh meanwhile fails too; the connection's banner is the one that shows.
    let session = shell.read_with(cx, |shell, _| shell.session().cloned().unwrap());
    session.update(cx, |session, cx| session.rescan(cx));
    settle(cx, &shell, "the refresh", |_, _| true);
    cx.update(|_, cx| {
        let view = shell.read(cx);
        let status = view.status_line(cx);
        assert_eq!(status.health, Health::Failing);
        assert_eq!(status.location, "loopback · disconnected");
        let down = view.backend_down(cx).expect("the backend-down banner");
        assert_eq!(
            down.failure.message,
            "Lost the connection to dv in loopback"
        );
        assert_eq!(down.action, "Retry");
        assert!(!down.stopped);
        let detail = down.failure.detail.expect("the error and the diagnostics");
        assert!(detail.contains("blocked by the test"), "{detail}");
        assert!(detail.contains("connection: loopback"), "{detail}");
        assert!(detail.contains("history:"), "{detail}");
    });

    // Retry, once it can start: connected, and the banner goes.
    blocked.store(false, Ordering::SeqCst);
    shell.update(cx, |shell, cx| shell.retry_backend(cx));
    settle(cx, &shell, "the retry", |shell, cx| {
        shell.backend_down(cx).is_none() && shell.status_line(cx).health == Health::Live
    });
    cx.update(|_, cx| {
        let view = shell.read(cx);
        assert_eq!(
            view.status_line(cx).location,
            "loopback · watching for changes"
        );
        let session = view.session().unwrap().read(cx);
        assert_eq!(session.error(), None);
        assert_eq!(session.files().len(), 3);
    });
}

#[gpui::test]
fn a_remote_review_whose_host_stops_says_so_and_starts_it_when_asked(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = open_shell(cx);
    let repo = fixture();
    // A server on a host the test stops, as the user stops a distro: its launcher's host check
    // reads `up`, and a start boots it.
    let up = Arc::new(AtomicBool::new(true));
    let launcher = Launcher::custom("an in-process server on a host the test stops", {
        let up = up.clone();
        move || {
            up.store(true, Ordering::SeqCst);
            Launcher::InProcess.start()
        }
    })
    .with_host_check({
        let up = up.clone();
        move |_| up.load(Ordering::SeqCst)
    });
    let connection = Connection::with_backoff("Ubuntu", launcher, vec![Duration::from_millis(10)]);
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
    let location = Location::Local(repo.path().to_path_buf());
    open_review(&shell, location, OpenRequest::Target(Target::Working), cx)
        .expect("it opens through the connection");
    settle(cx, &shell, "the file list", |shell, cx| {
        shell
            .session()
            .is_some_and(|session| session.read(cx).files().len() == 3)
    });

    // The user stops the distro: its server dies, and dv starts it again only when asked.
    up.store(false, Ordering::SeqCst);
    connection.kill();
    settle(cx, &shell, "the connection to stop", |shell, cx| {
        shell.backend_down(cx).is_some_and(|down| down.stopped)
    });
    // A refresh meanwhile starts nothing and raises nothing of its own.
    let session = shell.read_with(cx, |shell, _| shell.session().cloned().unwrap());
    session.update(cx, |session, cx| session.rescan(cx));
    settle(cx, &shell, "the refresh", |_, _| true);
    cx.update(|_, cx| {
        let view = shell.read(cx);
        let status = view.status_line(cx);
        assert_eq!(status.health, Health::Failing);
        assert_eq!(status.location, "Ubuntu · stopped");
        let down = view.backend_down(cx).expect("the backend-down banner");
        assert_eq!(down.failure.message, "Ubuntu stopped");
        assert_eq!(down.action, "Start Ubuntu");
        let detail = down.failure.detail.expect("why, and the diagnostics");
        assert!(
            detail.contains("isn't running, so dv wasn't started"),
            "{detail}"
        );
        let session = view.session().unwrap().read(cx);
        assert_eq!(session.error(), None);
        assert!(session.is_disconnected());
        assert_eq!(session.files().len(), 3, "what it showed stays");
    });
    assert_eq!(connection.state(), ConnectionState::Stopped);

    // Start: the explicit start boots it, and the review reads everything again.
    shell.update(cx, |shell, cx| shell.retry_backend(cx));
    settle(cx, &shell, "the start", |shell, cx| {
        shell.backend_down(cx).is_none() && shell.status_line(cx).health == Health::Live
    });
    assert!(up.load(Ordering::SeqCst));
    cx.update(|_, cx| {
        let session = shell.read(cx).session().unwrap().read(cx);
        assert_eq!(session.error(), None);
        assert!(!session.is_disconnected());
    });
}

#[gpui::test]
fn a_wsl_repo_spelled_two_ways_is_one_repo_named_as_wsl_spells_its_distro(cx: &mut TestAppContext) {
    let (dir, shell, cx) = open_shell(cx);
    let locations = cx.update(|_, cx| cx.services().locations.clone());
    // WSL lists the distro as `Ubuntu` (the stand-in starts nothing in it).
    locations.read_with(cx, |locations, _| {
        locations.wsl().set_distros(vec![dv_backend::wsl::Distro {
            name: "Ubuntu".into(),
            running: true,
        }])
    });
    let lower = Location::parse(r"\\wsl$\ubuntu\home\kyle\api").unwrap();
    let upper = Location::parse(r"\\wsl.localhost\Ubuntu\home\kyle\api").unwrap();
    for location in [&lower, &upper] {
        let known = open_repo(&shell, location.clone(), IfNotOpened::Banner, cx)
            .expect("known as given, its row saying why");
        assert_eq!(known, upper, "{location:?}");
        cx.update(|window, cx| shell.update(cx, |shell, cx| shell.close_menu(window, cx)));
    }
    let failure = open_review(&shell, lower, OpenRequest::Target(Target::Working), cx)
        .expect_err("nothing starts in a test");
    assert_eq!(failure.message, "couldn't open Ubuntu:/home/kyle/api");
    cx.update(|_, cx| {
        let view = shell.read(cx);
        assert_eq!(view.status_line(cx).location, "Ubuntu · can't be opened");
        let reviews = cx.services().reviews.read(cx);
        let rows: Vec<&Location> = reviews.rows().iter().map(|row| &row.location).collect();
        assert_eq!(rows, [&upper]);
        let connections = cx.services().locations.read(cx).connections().all();
        assert_eq!(connections.len(), 1);
        assert_eq!(connections[0].name(), "Ubuntu");
    });
    settle(cx, &shell, "the known repos to be saved", |_, _| true);
    let saved: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("known_repos.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        saved["repos"]
            .as_array()
            .unwrap()
            .iter()
            .map(|repo| &repo["location"])
            .collect::<Vec<_>>(),
        [&serde_json::to_value(&upper).unwrap()]
    );
}
