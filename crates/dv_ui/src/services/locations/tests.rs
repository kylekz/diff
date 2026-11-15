use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use dv_backend::Launcher;
use dv_backend::wsl::Distro;
use dv_test_support::FixtureRepo;
use gpui::{Entity, TestAppContext};

use super::*;

fn locations(cx: &mut TestAppContext, distros: Vec<Distro>) -> Entity<Locations> {
    cx.new(|cx| Locations::new(WslAccess::scripted(distros), cx))
}

async fn open(
    locations: &Entity<Locations>,
    location: Location,
    cx: &mut TestAppContext,
) -> OpenResult {
    locations
        .update(cx, |locations, cx| locations.open(location, cx))
        .await
}

async fn reach(
    locations: &Entity<Locations>,
    location: Location,
    cx: &mut TestAppContext,
) -> Reach {
    locations
        .update(cx, |locations, cx| locations.reach(location, cx))
        .await
}

fn distro(name: &str, running: bool) -> Distro {
    Distro {
        name: name.to_string(),
        running,
    }
}

fn wsl(distro: &str, path: &str) -> Location {
    Location::Wsl {
        distro: distro.to_string(),
        path: path.to_string(),
    }
}

fn reason(reach: &Reach) -> String {
    match reach {
        Reach::Failed { error, .. } => error.to_string(),
        Reach::Opened(backend) => panic!("{} opened", backend.repo().toplevel),
        Reach::Stopped { distro, .. } => panic!("{distro} is stopped"),
        Reach::Reconnecting => panic!("reconnecting"),
    }
}

/// Whether a listing found a failed connection to retry.
fn retry(reach: &Reach) -> bool {
    matches!(reach, Reach::Failed { retry: true, .. })
}

fn stopped(reach: &Reach) -> Option<&str> {
    match reach {
        Reach::Stopped { distro, .. } => Some(distro),
        _ => None,
    }
}

/// Runs `cx` until `done` holds, letting real time pass for the connection's threads.
fn wait_for(cx: &mut TestAppContext, what: &str, done: impl Fn(&mut TestAppContext) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[gpui::test]
async fn a_repo_opens_once_and_is_shared_by_its_root(cx: &mut TestAppContext) {
    let repo = FixtureRepo::new();
    repo.write("src/lib.rs", "fn a() {}\n");
    repo.commit("base");
    let locations = locations(cx, Vec::new());
    locations.update(cx, |locations, _| {
        locations.register_env(repo.path().to_path_buf(), repo.env_overlay())
    });
    let inside = Location::Local(repo.path().join("src"));
    let first = open(&locations, inside.clone(), cx)
        .await
        .expect("the repo opens");
    assert_eq!(
        Path::new(&first.repo().toplevel),
        repo.path(),
        "a path inside the work tree opens its root"
    );
    let root = first.repo().location.clone();
    let again = open(&locations, root.clone(), cx).await.unwrap();
    assert!(Arc::ptr_eq(&first, &again), "one backend per repo");
    locations.read_with(cx, |locations, _| {
        assert!(locations.is_settled());
        assert!(locations.cached(&inside).is_some());
        assert!(locations.cached(&root).is_some());
        assert!(locations.connection_of(&first).is_none(), "in process");
    });
}

#[gpui::test]
async fn racing_opens_share_one_backend_and_a_dropped_open_still_lands(cx: &mut TestAppContext) {
    let (repo, other) = (FixtureRepo::new(), FixtureRepo::new());
    let locations = locations(cx, Vec::new());
    locations.update(cx, |locations, _| {
        locations.register_env(repo.path().to_path_buf(), repo.env_overlay());
        locations.register_env(other.path().to_path_buf(), other.env_overlay());
    });
    let location = Location::Local(repo.path().to_path_buf());
    let (first, second) = locations.update(cx, |locations, cx| {
        (
            locations.open(location.clone(), cx),
            locations.open(Location::Local(repo.path().join(".")), cx),
        )
    });
    let (first, second) = (first.await.unwrap(), second.await.unwrap());
    assert!(Arc::ptr_eq(&first, &second), "one backend per repo");

    let dropped = locations.update(cx, |locations, cx| {
        locations.open(Location::Local(other.path().to_path_buf()), cx)
    });
    drop(dropped);
    cx.run_until_parked();
    locations.read_with(cx, |locations, _| {
        assert!(locations.is_settled());
        assert!(
            locations
                .cached(&Location::Local(other.path().to_path_buf()))
                .is_some()
        );
    });
}

#[gpui::test]
async fn the_registered_overlay_reaches_git(cx: &mut TestAppContext) {
    let repo = FixtureRepo::new();
    // Only the fixture's overlay points git at this global config, so only a backend
    // opened with it sees the alias.
    std::fs::write(
        repo.home().join(".gitconfig"),
        "[dv-test]\n\tmarker = from-the-overlay\n",
    )
    .unwrap();
    let locations = locations(cx, Vec::new());
    locations.update(cx, |locations, _| {
        locations.register_env(repo.path().to_path_buf(), repo.env_overlay())
    });
    let backend = open(&locations, Location::Local(repo.path().to_path_buf()), cx)
        .await
        .unwrap();
    let output = backend
        .git(
            &["config", "--get", "dv-test.marker"],
            &dv_core::backend::GitOpts::default(),
        )
        .unwrap();
    assert_eq!(output.stdout_utf8().unwrap().trim(), "from-the-overlay");
}

#[gpui::test]
async fn a_missing_path_says_so(cx: &mut TestAppContext) {
    let locations = locations(cx, Vec::new());
    let dir = tempfile::tempdir().unwrap();
    let gone = Location::Local(dir.path().join("missing"));
    let error = open(&locations, gone.clone(), cx).await.err().unwrap();
    assert!(
        matches!(*error, BackendError::PathNotFound { .. }),
        "{error:?}"
    );
    locations.read_with(cx, |locations, _| {
        assert!(locations.is_settled());
        assert!(locations.cached(&gone).is_none());
    });
}

#[gpui::test]
async fn a_deleted_repo_is_opened_again_and_reported_missing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("api");
    let repo = FixtureRepo::persistent(&path);
    repo.write("a.txt", "a\n");
    repo.commit("base");
    let locations = locations(cx, Vec::new());
    locations.update(cx, |locations, _| {
        locations.register_env(repo.path().to_path_buf(), repo.env_overlay())
    });
    let location = Location::Local(repo.path().to_path_buf());
    open(&locations, location.clone(), cx)
        .await
        .expect("the repo opens");
    std::fs::remove_dir_all(repo.path()).unwrap();
    let error = open(&locations, location.clone(), cx)
        .await
        .err()
        .expect("gone");
    assert!(
        matches!(*error, BackendError::PathNotFound { .. }),
        "{error:?}"
    );
    locations.read_with(cx, |locations, _| {
        assert!(locations.cached(&location).is_none())
    });
}

#[gpui::test]
async fn a_folder_whose_git_dir_went_is_opened_again_and_fails(cx: &mut TestAppContext) {
    let repo = FixtureRepo::new();
    repo.write("a.txt", "a\n");
    repo.commit("base");
    let locations = locations(cx, Vec::new());
    locations.update(cx, |locations, _| {
        locations.register_env(repo.path().to_path_buf(), repo.env_overlay())
    });
    let location = Location::Local(repo.path().to_path_buf());
    open(&locations, location.clone(), cx)
        .await
        .expect("the repo opens");
    // The folder stays; it is no longer a repository.
    std::fs::remove_dir_all(repo.path().join(".git")).unwrap();
    assert!(
        open(&locations, location.clone(), cx).await.is_err(),
        "not the cached backend over a folder without a repository"
    );
    locations.read_with(cx, |locations, _| {
        assert!(locations.cached(&location).is_none())
    });
}

#[gpui::test]
async fn a_listing_never_starts_a_distro_that_isnt_running(cx: &mut TestAppContext) {
    // A connection reports its state from a thread of its own.
    cx.executor().allow_parking();
    let locations = locations(cx, vec![distro("Ubuntu", false), distro("Debian", true)]);
    // Stopped, and nobody connected: the listing leaves it so, and makes no connection.
    let api = wsl("ubuntu", "/home/kyle/api");
    match reach(&locations, api.clone(), cx).await {
        Reach::Stopped { distro, error } => {
            assert_eq!(distro, "Ubuntu", "as WSL spells it");
            assert!(error.is_none());
        }
        other => panic!("{}", reason(&other)),
    }
    locations.read_with(cx, |locations, _| {
        assert!(locations.connections().all().is_empty());
        assert!(locations.connection_for(&api).is_none());
    });
    // No such distro.
    let nowhere = reason(&reach(&locations, wsl("Arch", "/srv/x"), cx).await);
    assert!(
        nowhere.ends_with("there is no WSL distro named Arch"),
        "{nowhere}"
    );
    // A running distro is connected to (the stand-in refuses to start anything), and the
    // failed connection is one to retry.
    let web = wsl("Debian", "/srv/web");
    let listed = reach(&locations, web.clone(), cx).await;
    assert!(retry(&listed));
    let refused = reason(&listed);
    assert!(refused.contains(wsl::REFUSED), "{refused}");
    let connection = locations
        .read_with(cx, |locations, _| locations.connection_for(&web))
        .expect("the listing made Debian's connection");
    assert!(matches!(connection.state(), ConnectionState::Failed { .. }));
    let started = connection.generation();
    // A listing never retries a failed connection; an explicit open does.
    let again = reach(&locations, web.clone(), cx).await;
    assert!(retry(&again));
    let again = reason(&again);
    assert!(again.contains(wsl::REFUSED), "{again}");
    assert_eq!(
        connection.generation(),
        started,
        "the listing started nothing"
    );
    let opened = open(&locations, web, cx).await.err().expect("refused");
    assert!(opened.to_string().contains(wsl::REFUSED), "{opened}");
    assert_eq!(connection.generation(), started + 1, "the open retried");
    // An explicit open of the stopped distro's repository connects (and so would boot it),
    // its connection named as WSL spells the distro.
    let error = open(&locations, api.clone(), cx)
        .await
        .err()
        .expect("refused");
    assert!(error.to_string().contains(wsl::REFUSED), "{error}");
    let connection = locations
        .read_with(cx, |locations, _| locations.connection_for(&api))
        .expect("the open made Ubuntu's connection");
    assert!(matches!(connection.state(), ConnectionState::Failed { .. }));
    assert_eq!(connection.name(), "Ubuntu");
    // Its distro still isn't running: the listing offers to start it, not to retry it, and
    // starts nothing.
    let started = connection.generation();
    let listed = reach(&locations, api.clone(), cx).await;
    assert_eq!(
        stopped(&listed),
        Some("Ubuntu"),
        "as the connection names it"
    );
    let Reach::Stopped {
        error: Some(error), ..
    } = &listed
    else {
        panic!("the start's failure comes with it");
    };
    assert!(error.to_string().contains(wsl::REFUSED), "{error}");
    assert_eq!(connection.generation(), started);
    locations.read_with(cx, |locations, _| assert!(locations.is_settled()));
}

#[test]
fn an_automation_run_reaches_wsl_only_with_a_scratch_home() {
    // `DV_WSL_HOME` as dv_backend's one check reads it: only a scratch home lets an automation
    // run reach the machine's WSL, never a value that would fall back to the real home.
    for home in [
        None,
        Some(""),
        Some(" "),
        Some("tmp/x"),
        Some("/"),
        Some("C:/Users/x/AppData/Local/Temp/x"),
    ] {
        assert!(
            WslAccess::for_run_with(true, home.map(str::to_string)).is_scripted(),
            "{home:?}"
        );
    }
    assert!(!WslAccess::for_run_with(true, Some("/tmp/dv-explore".into())).is_scripted());
    assert!(
        !WslAccess::for_run_with(false, None).is_scripted(),
        "the app"
    );
}

/// Off Windows the app's own WSL access asks `wsl.exe` nothing (inside WSL it would answer
/// through interop): there are no distros, and a WSL location can't be opened.
#[cfg(not(windows))]
#[gpui::test]
async fn an_app_off_windows_reaches_no_distro(cx: &mut TestAppContext) {
    // A connection reports its state from a thread of its own.
    cx.executor().allow_parking();
    let error = WslAccess::System.distros().unwrap_err();
    assert!(error.to_string().contains(wsl::NOT_WINDOWS), "{error}");
    let locations = cx.new(|cx| Locations::new(WslAccess::System, cx));
    let Err(error) = open(&locations, wsl("Ubuntu", "/home/kyle/api"), cx).await else {
        panic!("a WSL location opened off Windows");
    };
    assert!(error.to_string().contains(wsl::NOT_WINDOWS), "{error}");
}

/// A connection to an in-process server on a host the test stops, as the user stops a distro
/// (its launcher's host check reads `up`); `launches` counts the servers started.
fn stoppable(up: Arc<AtomicBool>, launches: Arc<AtomicU32>) -> Arc<Connection> {
    let launcher = Launcher::custom("an in-process server on a host the test stops", {
        let up = Arc::clone(&up);
        move || {
            launches.fetch_add(1, Ordering::SeqCst);
            up.store(true, Ordering::SeqCst);
            Launcher::InProcess.start()
        }
    })
    .with_host_check(move |_| up.load(Ordering::SeqCst));
    Connection::with_backoff("Ubuntu", launcher, vec![Duration::from_millis(10)])
}

#[gpui::test]
async fn a_listing_never_starts_a_host_stopped_after_a_death(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repo = FixtureRepo::new();
    repo.write("a.txt", "a\n");
    repo.commit("base");
    let (up, launches) = (Arc::new(AtomicBool::new(true)), Arc::new(AtomicU32::new(0)));
    let connection = stoppable(up.clone(), launches.clone());
    let locations = locations(cx, Vec::new());
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
    let backend = open(&locations, location.clone(), cx).await.expect("opens");
    assert_eq!(launches.load(Ordering::SeqCst), 1);

    // The user stops the host; its server dies with it, and the respawn starts nothing.
    up.store(false, Ordering::SeqCst);
    connection.kill();
    wait_for(cx, "the connection to stop", |_| {
        connection.state() == ConnectionState::Stopped
    });
    // The listing starts nothing either: the row offers Start.
    let listed = reach(&locations, location.clone(), cx).await;
    assert_eq!(stopped(&listed), Some("Ubuntu"));
    assert_eq!(
        launches.load(Ordering::SeqCst),
        1,
        "the listing booted nothing"
    );
    assert_eq!(connection.state(), ConnectionState::Stopped);

    // Started outside dv, the host lists again: an automatic start, which its check allows.
    up.store(true, Ordering::SeqCst);
    let Reach::Opened(listed) = reach(&locations, location.clone(), cx).await else {
        panic!("a running host lists");
    };
    assert!(
        Arc::ptr_eq(&listed, &backend),
        "the backend outlives its server"
    );
    assert_eq!(launches.load(Ordering::SeqCst), 2);

    // Stopped again, an explicit open boots it.
    up.store(false, Ordering::SeqCst);
    connection.kill();
    wait_for(cx, "the connection to stop", |_| {
        connection.state() == ConnectionState::Stopped
    });
    open(&locations, location, cx).await.expect("booted");
    assert_eq!(launches.load(Ordering::SeqCst), 3);
    assert_eq!(connection.state(), ConnectionState::Connected);
}

#[gpui::test]
async fn a_reconnecting_connection_lists_as_reconnecting_and_says_so(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repo = FixtureRepo::new();
    repo.write("a.txt", "a\n");
    repo.commit("base");
    let connection = Connection::with_backoff(
        "loopback",
        Launcher::InProcess,
        vec![Duration::from_secs(2)],
    );
    let locations = locations(cx, Vec::new());
    let events = Rc::new(RefCell::new(Vec::new()));
    let _subscription = cx.update(|cx| {
        let events = events.clone();
        cx.subscribe(&locations, move |_, event: &LocationsEvent, _| {
            events.borrow_mut().push(event.clone())
        })
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
    open(&locations, location.clone(), cx).await.expect("opens");
    let key = ConnectionKey::loopback("loopback");
    assert_eq!(
        locations.read_with(cx, |locations, _| locations.key_for(&location)),
        Some(key.clone())
    );
    cx.run_until_parked();
    assert!(events.borrow().is_empty(), "connecting changes no listing");

    connection.kill();
    wait_for(cx, "the death to be heard", |_| !events.borrow().is_empty());
    assert_eq!(
        *events.borrow(),
        [LocationsEvent::ConnectionChanged {
            key: key.clone(),
            up: false
        }]
    );
    // Reconnecting: the listing reads nothing and says so.
    assert!(matches!(
        reach(&locations, location.clone(), cx).await,
        Reach::Reconnecting
    ));
    // Back: another event, and it lists.
    wait_for(cx, "the reconnect to be heard", |_| {
        events.borrow().len() == 2
    });
    assert_eq!(
        events.borrow()[1],
        LocationsEvent::ConnectionChanged { key, up: true }
    );
    assert_eq!(connection.state(), ConnectionState::Connected);
    assert!(matches!(
        reach(&locations, location, cx).await,
        Reach::Opened(_)
    ));
}

/// A loopback connection over a server on a thread of this process; `retries` respawns
/// before it fails.
fn loopback(retries: usize) -> Arc<Connection> {
    Connection::with_backoff(
        "loopback",
        Launcher::InProcess,
        vec![Duration::from_millis(10); retries],
    )
}

#[gpui::test]
async fn a_loopback_route_reaches_its_repos_through_the_connection(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repo = FixtureRepo::new();
    repo.write("a.txt", "a\n");
    repo.commit("base");
    let connection = loopback(0);
    let locations = locations(cx, Vec::new());
    let notified = Rc::new(Cell::new(0));
    let _observing = cx.update(|cx| {
        let notified = notified.clone();
        cx.observe(&locations, move |_, _| notified.set(notified.get() + 1))
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
    let backend = open(&locations, location.clone(), cx)
        .await
        .expect("it opens through the server");
    assert_eq!(connection.state(), ConnectionState::Connected);
    assert_eq!(
        backend.repo().location,
        Location::Local(PathBuf::from(&backend.repo().toplevel)),
        "a loopback keeps the server's local path"
    );
    let reached = locations.read_with(cx, |locations, _| locations.connection_of(&backend));
    assert!(reached.is_some_and(|reached| Arc::ptr_eq(&reached, &connection)));
    let head = backend
        .git(
            &["rev-parse", "HEAD"],
            &dv_core::backend::GitOpts::default(),
        )
        .expect("git runs in the server");
    assert_eq!(head.stdout_utf8().unwrap().trim().len(), 40);
    // Kept: no local file-system check for a remote backend.
    let again = open(&locations, location.clone(), cx).await.unwrap();
    assert!(Arc::ptr_eq(&backend, &again));
    // Its state changes repaint the service's observers.
    let before = notified.get();
    connection.kill();
    wait_for(cx, "the death to be heard", |_| notified.get() > before);
    assert!(matches!(connection.state(), ConnectionState::Failed { .. }));
    // A listing doesn't retry the failed connection: the row offers Retry...
    let generation = connection.generation();
    let listed = reach(&locations, location.clone(), cx).await;
    assert!(retry(&listed));
    assert!(
        reason(&listed).contains("the connection to dv failed"),
        "{}",
        reason(&listed)
    );
    assert_eq!(connection.generation(), generation);
    // ...an explicit open does, and the backend goes on through the new server.
    let reopened = open(&locations, location, cx).await.expect("retried");
    assert_eq!(connection.state(), ConnectionState::Connected);
    assert_eq!(connection.generation(), generation + 1);
    assert!(
        Arc::ptr_eq(&backend, &reopened),
        "a remote backend outlives its server"
    );
    reopened
        .git(
            &["status", "--short"],
            &dv_core::backend::GitOpts::default(),
        )
        .expect("the new server answers");
}

#[gpui::test]
async fn which_connection_a_repo_goes_through_is_known_once_an_open_of_it_ran(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let repo = FixtureRepo::new();
    repo.write("a.txt", "a\n");
    repo.commit("base");
    // Its host stopped: the listing opens nothing, yet the row's Start needs the key.
    let (up, launches) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicU32::new(0)),
    );
    let connection = stoppable(up, launches.clone());
    let locations = locations(cx, Vec::new());
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
    // The route is matched by canonical paths, which only an open (off the UI thread) reads.
    assert_eq!(
        locations.read_with(cx, |locations, _| locations.key_for(&location)),
        None
    );
    let listed = reach(&locations, location.clone(), cx).await;
    assert_eq!(stopped(&listed), Some("Ubuntu"));
    assert_eq!(launches.load(Ordering::SeqCst), 0);
    assert_eq!(
        locations.read_with(cx, |locations, _| locations.key_for(&location)),
        Some(ConnectionKey::loopback("Ubuntu"))
    );
    // A WSL location's is its distro's, opened or not.
    assert_eq!(
        locations.read_with(cx, |locations, _| locations
            .key_for(&wsl("Ubuntu", "/home/kyle/api"))),
        Some(ConnectionKey::wsl("ubuntu"))
    );
}

#[gpui::test]
async fn an_explicit_open_during_a_reconnect_waits_for_it_and_keeps_the_backend(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let repo = FixtureRepo::new();
    repo.write("a.txt", "a\n");
    repo.commit("base");
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
        Connection::with_backoff("loopback", launcher, vec![Duration::from_millis(400); 2]);
    let locations = locations(cx, Vec::new());
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
    let backend = open(&locations, location.clone(), cx).await.expect("opens");

    // Picked while it reconnects: the open waits, then goes through the new server.
    connection.kill();
    assert!(matches!(
        connection.state(),
        ConnectionState::Reconnecting { .. }
    ));
    let again = open(&locations, location.clone(), cx)
        .await
        .expect("opens once the connection is back");
    assert!(Arc::ptr_eq(&backend, &again));
    assert_eq!(connection.state(), ConnectionState::Connected);

    // A reconnect that fails for good fails the open as the connection did, and the backend
    // stays for when it is back.
    blocked.store(true, Ordering::SeqCst);
    connection.kill();
    let error = open(&locations, location.clone(), cx)
        .await
        .err()
        .expect("the connection failed");
    assert!(
        matches!(&*error, BackendError::Unavailable { reason, .. }
            if reason == "the connection to dv failed: couldn't start dv: blocked by the test"),
        "{error:?}"
    );
    locations.read_with(cx, |locations, _| {
        assert!(
            locations
                .cached(&location)
                .is_some_and(|cached| Arc::ptr_eq(&cached, &backend)),
            "kept"
        )
    });
    blocked.store(false, Ordering::SeqCst);
    let back = open(&locations, location, cx).await.expect("retried");
    assert!(Arc::ptr_eq(&backend, &back));
}

#[gpui::test]
async fn a_distros_connection_is_named_as_wsl_spells_it(cx: &mut TestAppContext) {
    // A connection reports its state from a thread of its own.
    cx.executor().allow_parking();
    let locations = locations(cx, vec![distro("Ubuntu", true), distro("Debian", true)]);
    // A listing makes the connection from WSL's list.
    let listed = reach(&locations, wsl("ubuntu", "/srv/api"), cx).await;
    assert!(retry(&listed), "the stand-in starts nothing");
    // An explicit open lists WSL's distros first.
    assert!(
        open(&locations, wsl("DEBIAN", "/srv/web"), cx)
            .await
            .is_err()
    );
    locations.read_with(cx, |locations, _| {
        let names: Vec<String> = locations
            .connections()
            .all()
            .iter()
            .map(|connection| connection.name().to_string())
            .collect();
        assert_eq!(names, ["Debian", "Ubuntu"]);
        assert_eq!(
            locations.canonical(&wsl("uBuNtU", "/srv/api")),
            wsl("Ubuntu", "/srv/api")
        );
        assert_eq!(
            locations.canonical(&wsl("debian", "/srv/web")),
            wsl("Debian", "/srv/web")
        );
        // A distro WSL doesn't list stays as given.
        assert_eq!(
            locations.canonical(&wsl("arch", "/srv/x")),
            wsl("arch", "/srv/x")
        );
    });
}

#[gpui::test]
async fn the_base_environment_reaches_every_backends_git(cx: &mut TestAppContext) {
    // Two repositories: one with a route of its own (a seeded repo's), one without.
    let (routed, plain) = (FixtureRepo::new(), FixtureRepo::new());
    for repo in [&routed, &plain] {
        repo.write("a.txt", "a\n");
        repo.commit("base");
    }
    let locations = locations(cx, Vec::new());
    // A transport git doesn't use here stands for an automation run's `file`: with it, a
    // local path is refused, which shows the variable reached git without leaving the disk.
    let base = |transport: &str| vec![("GIT_ALLOW_PROTOCOL".into(), Some(transport.into()))];
    locations.update(cx, |locations, _| {
        locations.set_base_env(base("ssh"));
        locations.register_env(routed.path().to_path_buf(), routed.env_overlay());
    });
    let ls_remote = |backend: &Arc<dyn Backend>| {
        let own = backend.repo().toplevel.clone();
        backend.git(&["ls-remote", &own], &dv_core::backend::GitOpts::new())
    };
    let plain_backend = open(&locations, Location::Local(plain.path().to_path_buf()), cx)
        .await
        .unwrap();
    match ls_remote(&plain_backend) {
        Err(BackendError::Git { stderr, .. }) => {
            assert!(stderr.contains("transport 'file' not allowed"), "{stderr}")
        }
        other => panic!("{other:?}"),
    }
    // A route's own environment goes over the base's: the fixture's allows a local path.
    let routed_backend = open(&locations, Location::Local(routed.path().to_path_buf()), cx)
        .await
        .unwrap();
    ls_remote(&routed_backend).expect("the fixture's git reads a local path");
}
