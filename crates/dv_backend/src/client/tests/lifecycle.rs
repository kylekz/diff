//! What follows a server's death (PLAN §5.2): the calls in flight fail, the connection
//! reconnects with backoff (shortened here) or fails until `retry`, worktrees open again on the
//! new server, watches subscribe again with a synthetic rescan, and subscribers hear it all.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::RecvTimeoutError;

use dv_core::backend::{WatchEvent, WatchHandle, WatchSpec};
use pretty_assertions::assert_eq;

use super::*;

const WITHIN: Duration = Duration::from_secs(10);

/// An in-process server that a test can stop from starting, counting the starts.
#[derive(Clone, Default)]
struct Switch {
    blocked: Arc<AtomicBool>,
    starts: Arc<AtomicU32>,
}

impl Switch {
    fn launcher(&self) -> Launcher {
        let switch = self.clone();
        Launcher::custom("an in-process server behind a switch", move || {
            switch.starts.fetch_add(1, Ordering::SeqCst);
            if switch.blocked.load(Ordering::SeqCst) {
                return Err(io::Error::other("blocked by the test"));
            }
            Launcher::InProcess.start()
        })
    }

    fn block(&self, blocked: bool) {
        self.blocked.store(blocked, Ordering::SeqCst);
    }

    fn starts(&self) -> u32 {
        self.starts.load(Ordering::SeqCst)
    }
}

fn watch(backend: &dyn Backend, spec: WatchSpec) -> (WatchHandle, Receiver<WatchEvent>) {
    let (tx, rx) = mpsc::channel();
    let handle = backend
        .watch(
            spec,
            Box::new(move |event| {
                let _ = tx.send(event);
            }),
        )
        .unwrap();
    (handle, rx)
}

/// Waits for an event satisfying `wanted`.
fn event_where(rx: &Receiver<WatchEvent>, wanted: impl Fn(&WatchEvent) -> bool) -> WatchEvent {
    let deadline = Instant::now() + WITHIN;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(event) if wanted(&event) => return event,
            Ok(_) => {}
            Err(error) => panic!("no such event within {WITHIN:?}: {error:?}"),
        }
    }
}

fn is_rescan(event: &WatchEvent) -> bool {
    event.rescan
}

fn has_path(path: &'static str) -> impl Fn(&WatchEvent) -> bool {
    move |event| event.worktree_paths.iter().any(|p| p == path)
}

#[test]
fn a_kill_fails_the_calls_in_flight_then_the_connection_comes_back() {
    let repo = committed();
    let connection = Connection::with_backoff(
        "in-process",
        Launcher::InProcess,
        vec![Duration::from_millis(500); 3],
    );
    let (_subscription, rx) = states(&connection);
    let backend = Arc::new(open(&connection, &repo));
    states_until(&rx, WITHIN, |state| *state == ConnectionState::Connected);

    let (tx, done) = mpsc::channel();
    std::thread::spawn({
        let backend = Arc::clone(&backend);
        move || {
            let started = Instant::now();
            let result = backend.git(&["-c", "alias.slow=!sleep 3", "slow"], &GitOpts::default());
            let _ = tx.send((result, started.elapsed()));
        }
    });
    std::thread::sleep(Duration::from_millis(300));
    connection.kill();
    let (result, took) = done.recv_timeout(WITHIN).expect("the call ends");
    let error = result.unwrap_err();
    assert!(
        matches!(&error, BackendError::Disconnected { location, .. } if location == "in-process"),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "lost the connection to dv in in-process: dv was killed"
    );
    assert!(
        took < Duration::from_secs(2),
        "at once, not when git ends: {took:?}"
    );

    // While it waits to respawn, calls fail at once with the death.
    assert_eq!(
        connection.state(),
        ConnectionState::Reconnecting { attempt: 1 }
    );
    let started = Instant::now();
    let error = backend.worktree_len("a.txt").unwrap_err();
    assert_eq!(
        error.to_string(),
        "lost the connection to dv in in-process: dv was killed"
    );
    assert!(started.elapsed() < Duration::from_millis(200));

    assert_eq!(
        states_until(&rx, WITHIN, |state| *state == ConnectionState::Connected),
        [
            ConnectionState::Reconnecting { attempt: 1 },
            ConnectionState::Connected
        ]
    );
    assert_eq!(connection.generation(), 2);
    // The backend opens its worktree on the new server with its next call.
    assert_eq!(backend.worktree_len("a.txt").unwrap(), Some(2));
    let output = backend.git(&["ls-files"], &GitOpts::default()).unwrap();
    assert_eq!(output.stdout_utf8(), Some("a.txt\n"));

    let diagnostics = connection.diagnostics();
    for line in [
        " s dv died: dv was killed\n",
        " s reconnecting (attempt 1)\n",
        "servers started: 2\n",
    ] {
        assert!(diagnostics.contains(line), "{line:?} in {diagnostics}");
    }
}

#[test]
fn watches_subscribe_again_after_a_reconnect_with_one_rescan() {
    let repo = committed();
    let connection = Connection::with_backoff(
        "in-process",
        Launcher::InProcess,
        vec![Duration::from_millis(300); 3],
    );
    let (_subscription, states_rx) = states(&connection);
    let backend = open(&connection, &repo);
    let (_worktree, worktree_rx) = watch(&backend, WatchSpec::WORKTREE);
    let (_store, store_rx) = watch(&backend, WatchSpec::STORE);
    repo.write("before.txt", "x\n");
    event_where(&worktree_rx, has_path("before.txt"));

    connection.kill();
    // Written while nobody watches: only the rescan can tell.
    repo.write("while-down.txt", "x\n");
    states_until(&states_rx, WITHIN, |state| {
        *state == ConnectionState::Connected
    });
    assert_eq!(
        event_where(&worktree_rx, is_rescan),
        WatchEvent {
            rescan: true,
            ..WatchEvent::default()
        }
    );
    event_where(&store_rx, is_rescan);

    // Both watch the new server.
    repo.write("after.txt", "x\n");
    event_where(&worktree_rx, has_path("after.txt"));
    backend.store_append("reviews/r-1.jsonl", b"{}").unwrap();
    event_where(&store_rx, |event| {
        event
            .store_paths
            .iter()
            .any(|path| path == "reviews/r-1.jsonl")
    });
}

#[test]
fn respawns_that_fail_leave_the_connection_failed_until_retry() {
    let repo = committed();
    let switch = Switch::default();
    let connection = Connection::with_backoff(
        "switched",
        switch.launcher(),
        vec![Duration::from_millis(20); 3],
    );
    let (_subscription, rx) = states(&connection);
    let backend = open(&connection, &repo);
    let (_handle, events) = watch(&backend, WatchSpec::WORKTREE);
    states_until(&rx, WITHIN, |state| *state == ConnectionState::Connected);
    assert_eq!(switch.starts(), 1);

    switch.block(true);
    connection.kill();
    let failed = ConnectionState::Failed {
        error: "couldn't start dv: blocked by the test".into(),
    };
    assert_eq!(
        states_until(&rx, WITHIN, |state| matches!(
            state,
            ConnectionState::Failed { .. }
        )),
        [
            ConnectionState::Reconnecting { attempt: 1 },
            ConnectionState::Reconnecting { attempt: 2 },
            ConnectionState::Reconnecting { attempt: 3 },
            failed.clone(),
        ]
    );
    assert_eq!(switch.starts(), 4, "three attempts");
    assert!(
        connection
            .diagnostics()
            .contains(" s attempt 3 failed: couldn't start dv: blocked by the test\n")
    );

    // Failed: calls fail without starting anything, and nothing starts by itself.
    let error = backend.worktree_len("a.txt").unwrap_err();
    assert!(
        matches!(error, BackendError::Unavailable { .. }),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "switched can't be opened: the connection to dv failed: couldn't start dv: blocked by \
         the test"
    );
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(switch.starts(), 4);
    assert_eq!(connection.state(), failed);

    // A retry that fails again stays failed; one that works brings everything back.
    assert!(connection.retry().is_err());
    assert_eq!(switch.starts(), 5);
    switch.block(false);
    connection.retry().unwrap();
    assert_eq!(connection.state(), ConnectionState::Connected);
    let seen = states_until(&rx, WITHIN, |state| *state == ConnectionState::Connected);
    assert_eq!(
        seen,
        [
            ConnectionState::Connecting,
            failed.clone(),
            ConnectionState::Connecting,
            ConnectionState::Connected,
        ]
    );
    event_where(&events, is_rescan);
    assert_eq!(backend.worktree_len("a.txt").unwrap(), Some(2));
    repo.write("back.txt", "x\n");
    event_where(&events, has_path("back.txt"));

    // On a live connection, retry is a no-op.
    connection.retry().unwrap();
    assert_eq!(switch.starts(), 6);
}

#[test]
fn each_respawn_waits_its_own_backoff() {
    // Waits far enough apart that each gap shows which one it took: attempt `i` waits
    // `backoff[i - 1]` after `Reconnecting { attempt: i }`, never the next one's.
    let backoff = [
        Duration::from_millis(50),
        Duration::from_millis(400),
        Duration::from_millis(1200),
    ];
    let switch = Switch::default();
    let connection = Connection::with_backoff("switched", switch.launcher(), backoff.to_vec());
    let (tx, rx) = mpsc::channel();
    let _subscription = connection.subscribe(move |state| {
        let _ = tx.send((Instant::now(), state.clone()));
    });
    connection.connect().unwrap();
    let deadline = Instant::now() + WITHIN;
    let next = || {
        rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the next state")
    };
    while next().1 != ConnectionState::Connected {}

    switch.block(true);
    connection.kill();
    let states: Vec<(Instant, ConnectionState)> = (0..4).map(|_| next()).collect();
    assert_eq!(
        states
            .iter()
            .map(|(_, state)| state.clone())
            .collect::<Vec<_>>(),
        [
            ConnectionState::Reconnecting { attempt: 1 },
            ConnectionState::Reconnecting { attempt: 2 },
            ConnectionState::Reconnecting { attempt: 3 },
            ConnectionState::Failed {
                error: "couldn't start dv: blocked by the test".into()
            },
        ]
    );
    // A state reaches the test through the notifier thread, a moment after the reconnect
    // thread set it and began its wait, so a gap may read that moment short of the wait.
    let delivery = Duration::from_millis(20);
    for (i, wait) in backoff.iter().enumerate() {
        let gap = states[i + 1].0 - states[i].0;
        assert!(
            gap + delivery >= *wait,
            "attempt {} came {gap:?} after its state",
            i + 1
        );
        if let Some(longer) = backoff.get(i + 1) {
            assert!(
                gap < *longer,
                "attempt {} waited {gap:?}: the next attempt's wait",
                i + 1
            );
        }
    }
}

#[test]
fn a_server_that_dies_on_every_start_ends_failed() {
    // Answers `hello`, then ends its output: each server dies right after it is up.
    let starts = Arc::new(AtomicU32::new(0));
    let launcher = {
        let starts = Arc::clone(&starts);
        let fake = fake(|mut input, mut output| {
            if let Some(request) = next_request(&mut input) {
                answer(&mut output, hello_reply(&request["id"], PROTO, Value::Null));
            }
            std::thread::sleep(Duration::from_millis(50));
        });
        Launcher::custom("a server that dies", move || {
            starts.fetch_add(1, Ordering::SeqCst);
            fake.start()
        })
    };
    let connection =
        Connection::with_backoff("dying", launcher, vec![Duration::from_millis(20); 3]);
    let (_subscription, rx) = states(&connection);
    connection.connect().unwrap();
    let seen = states_until(&rx, WITHIN, |state| {
        matches!(state, ConnectionState::Failed { .. })
    });
    assert_eq!(
        seen.last(),
        Some(&ConnectionState::Failed {
            error: "dv's output ended".into()
        })
    );
    assert_eq!(
        starts.load(Ordering::SeqCst),
        4,
        "the first start and three respawns: {seen:?}"
    );
}

#[test]
fn a_server_refused_on_a_respawn_fails_the_connection_at_once() {
    let starts = Arc::new(AtomicU32::new(0));
    let launcher = {
        let starts = Arc::clone(&starts);
        Launcher::custom("a server replaced by another build", move || {
            let first = starts.fetch_add(1, Ordering::SeqCst) == 0;
            let build_ok = if first { Value::Null } else { json!(false) };
            fake(move |mut input, mut output| {
                while let Some(request) = next_request(&mut input) {
                    answer(
                        &mut output,
                        hello_reply(&request["id"], PROTO, build_ok.clone()),
                    );
                }
            })
            .start()
        })
    };
    let connection =
        Connection::with_backoff("Ubuntu", launcher, vec![Duration::from_millis(20); 3]);
    let (_subscription, rx) = states(&connection);
    connection.connect().unwrap();
    connection.kill();
    let seen = states_until(&rx, WITHIN, |state| {
        matches!(state, ConnectionState::Failed { .. })
    });
    assert_eq!(
        seen[2..],
        [
            ConnectionState::Reconnecting { attempt: 1 },
            ConnectionState::Failed {
                error: "the dv in Ubuntu doesn't match this app".into()
            }
        ]
    );
    assert_eq!(starts.load(Ordering::SeqCst), 2);
}

#[test]
fn dropping_the_connection_ends_its_reconnecting() {
    let switch = Switch::default();
    let connection = Connection::with_backoff(
        "switched",
        switch.launcher(),
        vec![Duration::from_millis(300); 3],
    );
    connection.connect().unwrap();
    connection.kill();
    assert_eq!(
        connection.state(),
        ConnectionState::Reconnecting { attempt: 1 }
    );
    drop(connection);
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(
        switch.starts(),
        1,
        "no respawn for a connection nobody holds"
    );
}

#[test]
fn a_watch_dropped_while_reconnecting_isnt_subscribed_again() {
    let repo = committed();
    let connection = Connection::with_backoff(
        "in-process",
        Launcher::InProcess,
        vec![Duration::from_millis(300); 3],
    );
    let (_subscription, rx) = states(&connection);
    let backend = open(&connection, &repo);
    let (kept, kept_rx) = watch(&backend, WatchSpec::WORKTREE);
    let (dropped, dropped_rx) = watch(&backend, WatchSpec::WORKTREE);
    connection.kill();
    let started = Instant::now();
    drop(dropped);
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "the drop doesn't wait for a server"
    );
    // Its sink went with the drop, and the channel with it.
    while dropped_rx.try_recv().is_ok() {}
    assert_eq!(
        dropped_rx.recv_timeout(Duration::from_millis(100)).err(),
        Some(RecvTimeoutError::Disconnected)
    );
    states_until(&rx, WITHIN, |state| *state == ConnectionState::Connected);
    event_where(&kept_rx, is_rescan);
    assert_eq!(connection.watches.lock().len(), 1, "only the kept watch");
    drop(kept);
    assert!(connection.watches.lock().is_empty());
}

#[test]
fn subscriptions_hear_each_state_until_dropped() {
    let connection = Connection::with_backoff(
        "in-process",
        Launcher::InProcess,
        vec![Duration::from_millis(50); 3],
    );
    let (subscription, rx) = states(&connection);
    // One that drops itself from inside its own callback, which must not deadlock.
    let slot: Arc<parking_lot::Mutex<Option<Subscription>>> = Arc::default();
    let (once_tx, once_rx) = mpsc::channel();
    *slot.lock() = Some(connection.subscribe({
        let slot = Arc::clone(&slot);
        move |state| {
            let _ = once_tx.send(state.clone());
            drop(slot.lock().take());
        }
    }));

    connection.connect().unwrap();
    assert_eq!(
        states_until(&rx, WITHIN, |state| *state == ConnectionState::Connected),
        [ConnectionState::Connecting, ConnectionState::Connected]
    );
    assert_eq!(
        once_rx.recv_timeout(WITHIN).unwrap(),
        ConnectionState::Connecting
    );
    drop(subscription);
    // Dropped, its callback (and the sender inside it) is gone: nothing more arrives.
    connection.kill();
    assert_eq!(
        rx.recv_timeout(Duration::from_millis(500)).err(),
        Some(RecvTimeoutError::Disconnected)
    );
    assert_eq!(
        once_rx.recv_timeout(Duration::from_millis(500)).err(),
        Some(RecvTimeoutError::Disconnected)
    );
}

#[test]
fn state_reads_as_the_status_line_and_diagnostics_say_it() {
    for (state, text) in [
        (ConnectionState::Idle, "idle"),
        (ConnectionState::Connecting, "connecting"),
        (
            ConnectionState::Installing { done: 3, total: 7 },
            "installing dv (3 of 7 bytes)",
        ),
        (ConnectionState::Connected, "connected"),
        (
            ConnectionState::Reconnecting { attempt: 2 },
            "reconnecting (attempt 2)",
        ),
        (
            ConnectionState::Failed {
                error: "gone".into(),
            },
            "failed: gone",
        ),
    ] {
        assert_eq!(state.to_string(), text);
    }
    assert_eq!(
        BACKOFF,
        [
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(5)
        ],
        "PLAN §5.2"
    );
}
