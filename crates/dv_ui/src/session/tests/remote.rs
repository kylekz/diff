//! A review whose backend is remote (PLAN §5.2): the session over a `RemoteBackend` through a
//! connection to a server on a thread of this process, which a test kills as a crash would.
//! A lost connection raises no banner and loses nothing: once it is back, the synthetic rescan
//! reads what changed meanwhile.

use std::sync::atomic::{AtomicBool, Ordering};

use dv_backend::{Connection, ConnectionState, Launcher};
use pretty_assertions::assert_eq;

use super::*;

/// A connection to an in-process server whose starts fail while `blocked`, respawning after
/// each of `backoff`.
fn connection(blocked: Arc<AtomicBool>, backoff: Vec<Duration>) -> Arc<Connection> {
    let launcher = Launcher::custom("an in-process server tests block", move || {
        if blocked.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("blocked by the test"));
        }
        Launcher::InProcess.start()
    });
    Connection::with_backoff("loopback", launcher, backoff)
}

/// A session over `repo`'s working review through `connection`.
fn remote_session(
    repo: &FixtureRepo,
    connection: &Arc<Connection>,
    cx: &mut TestAppContext,
) -> Entity<ReviewSession> {
    let backend: Arc<dyn Backend> = Arc::new(
        connection
            .open(repo.path().to_str().unwrap(), repo.env_overlay())
            .expect("the fixture opens through the server"),
    );
    let store = ReviewStore::new(backend.clone());
    let opened = ops::open_review(&store, Target::Working, reviewer()).unwrap();
    let loaded = store.load(&opened.review.id).unwrap();
    cx.new(|cx| {
        ReviewSession::new(
            OpenedReview {
                origin: backend.repo().location.clone(),
                backend,
                loaded,
                created: opened.created,
                reviewer: reviewer(),
                labels: LabelContext::default(),
                connection: Some(connection.clone()),
                pr: None,
            },
            cx,
        )
    })
}

/// Lets real time pass until `done` holds of the session, settled or not.
fn wait_until(
    cx: &mut TestAppContext,
    session: &Entity<ReviewSession>,
    what: &str,
    done: impl Fn(&ReviewSession) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        cx.executor().advance_clock(REFRESH_DEBOUNCE * 2);
        cx.run_until_parked();
        if session.read_with(cx, |session, _| done(session)) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[gpui::test]
fn a_lost_connection_raises_nothing_and_the_rescan_catches_up(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repo = fixture();
    let connection = connection(Arc::default(), vec![Duration::from_secs(2)]);
    let session = remote_session(&repo, &connection, cx);
    wait(cx, &session, "the list", |session| !session.is_loading());
    session.update(cx, |session, cx| {
        session.ensure_loaded(["src/a.rs".to_string()], cx)
    });
    wait(cx, &session, "a diff", |session| {
        session
            .file_state("src/a.rs")
            .and_then(FileState::diff)
            .is_some()
    });
    session.read_with(cx, |session, _| {
        assert_eq!(
            session.connection_state(),
            Some(&ConnectionState::Connected)
        );
        assert_eq!(session.backend_place(), "loopback");
    });

    // The server dies; the next respawn is 2 s away.
    connection.kill();
    assert!(matches!(
        connection.state(),
        ConnectionState::Reconnecting { attempt: 1 }
    ));
    // A refresh meanwhile fails with `Disconnected`, which is no refresh failure: the list and
    // the loaded diff stay, nothing more loads, and no banner is raised.
    session.update(cx, |session, cx| session.rescan(cx));
    cx.run_until_parked();
    session.read_with(cx, |session, _| {
        assert!(session.is_disconnected());
        assert_eq!(session.error(), None);
        assert_eq!(paths(session), ["notes.txt", "src/a.rs", "src/b.rs"]);
        assert!(
            session.shown("src/a.rs").is_some(),
            "the diff stays on screen"
        );
        assert!(session.is_settled(), "nothing can land before it is back");
    });
    wait_until(cx, &session, "reconnecting to be heard", |session| {
        matches!(
            session.connection_state(),
            Some(ConnectionState::Reconnecting { .. })
        )
    });
    // The agent works on meanwhile; no watch sees it.
    repo.write("src/c.rs", "fn c() {}\n");

    // Back: the synthetic rescan lists what changed while nobody watched.
    wait(cx, &session, "the rescan", |session| {
        paths(session).contains(&"src/c.rs") && !session.is_disconnected()
    });
    session.read_with(cx, |session, _| {
        assert_eq!(
            session.connection_state(),
            Some(&ConnectionState::Connected)
        );
        assert_eq!(session.error(), None);
        assert_eq!(connection.generation(), 2);
    });
    // And the watch follows the agent again.
    repo.write("src/d.rs", "fn d() {}\n");
    wait(cx, &session, "a live edit", |session| {
        paths(session).contains(&"src/d.rs")
    });
}

#[gpui::test]
fn a_failed_connection_waits_for_retry(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repo = fixture();
    let blocked = Arc::new(AtomicBool::new(false));
    let connection = connection(blocked.clone(), vec![Duration::from_millis(10)]);
    let session = remote_session(&repo, &connection, cx);
    wait(cx, &session, "the list", |session| !session.is_loading());

    blocked.store(true, Ordering::SeqCst);
    connection.kill();
    wait_until(cx, &session, "the connection to fail", |session| {
        matches!(
            session.connection_state(),
            Some(ConnectionState::Failed { .. })
        )
    });
    repo.write("src/c.rs", "fn c() {}\n");
    // Retry while still blocked fails again, and says so in the state.
    session.update(cx, |session, cx| session.retry_connection(cx));
    wait(cx, &session, "the retry", |session| {
        !session.is_retrying_connection()
    });
    assert!(matches!(connection.state(), ConnectionState::Failed { .. }));

    blocked.store(false, Ordering::SeqCst);
    session.update(cx, |session, cx| session.retry_connection(cx));
    wait(cx, &session, "the retry and its rescan", |session| {
        session.connection_state() == Some(&ConnectionState::Connected)
            && paths(session).contains(&"src/c.rs")
    });
    session.read_with(cx, |session, _| {
        assert_eq!(session.error(), None);
        assert!(!session.is_disconnected());
    });
}
