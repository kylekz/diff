//! `dv backend --stdio` as the app meets it (docs/PLAN.md §5.1, §5.2, §14.2): the built `dv`
//! started as a loopback server through `Launcher::Command`, and a `Connection` over its real
//! pipes — the handshake with and without a build, a repository with git and a watch, a kill
//! mid-call and the reconnect, respawns that fail and `retry`, a process it runs (PLAN
//! §18.3), and the server's end when the last connection goes. These run in every shape of
//! `dv`; the headless one is what WSL distros run.
//!
//! Every server is a child of this test process and none may outlive its test: each test
//! checks that every server it met (by the pid its `hello` answered with) is gone at the end.
//! Children get none of this process's `CLAUDECODE`, `DV_AUTHOR`, `DV_MODEL`, `DV_REVIEW` or `RUST_LOG`.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dv_backend::build::sha256_file;
use dv_backend::{Connection, ConnectionState, Launcher, RemoteBackend, Subscription};
use dv_core::backend::{
    Backend, BackendError, GitOpts, ProcessEvent, ProcessHandle, ProcessSpec, WatchEvent,
    WatchHandle, WatchSpec,
};
use dv_core::location::Location;
use dv_test_support::FixtureRepo;
use pretty_assertions::assert_eq;

const WITHIN: Duration = Duration::from_secs(20);

/// `dv backend --stdio`, the built binary, asking it to be `build` when given.
fn launcher(build: Option<String>) -> Launcher {
    Launcher::Command {
        program: env!("CARGO_BIN_EXE_dv").into(),
        args: vec!["backend".into(), "--stdio".into()],
        env: [
            "CLAUDECODE",
            "DV_AUTHOR",
            "DV_MODEL",
            "DV_REVIEW",
            "RUST_LOG",
        ]
        .into_iter()
        .map(|name| (name.into(), None))
        .collect(),
        build,
    }
}

fn fixture() -> FixtureRepo {
    let repo = FixtureRepo::new();
    repo.write("a.txt", "a\n");
    repo.commit("base");
    repo
}

fn open(connection: &Arc<Connection>, repo: &FixtureRepo) -> RemoteBackend {
    connection
        .open(repo.path().to_str().unwrap(), repo.env_overlay())
        .unwrap()
}

/// The pid of the server `connection` is connected to.
fn server_pid(connection: &Connection) -> u32 {
    let server = connection.server().expect("connected");
    assert_ne!(server.pid, std::process::id(), "a process of its own");
    server.pid
}

/// Whether the process `pid` still exists (on Linux, also as a zombie nobody reaped).
fn running(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .expect("tasklist runs");
        String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
    }
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success())
    }
}

/// Waits until the process `pid` is gone; a server outliving its connection fails the test.
fn assert_gone(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while running(pid) {
        assert!(
            Instant::now() < deadline,
            "dv backend --stdio (pid {pid}) outlived its connection"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Every state `connection` reports from now on, on a channel.
fn states(connection: &Connection) -> (Subscription, Receiver<ConnectionState>) {
    let (tx, rx) = mpsc::channel();
    let subscription = connection.subscribe(move |state| {
        let _ = tx.send(state.clone());
    });
    (subscription, rx)
}

/// The states reported until one satisfies `last` (included), within `within`.
fn states_until(
    rx: &Receiver<ConnectionState>,
    within: Duration,
    last: impl Fn(&ConnectionState) -> bool,
) -> Vec<ConnectionState> {
    let deadline = Instant::now() + within;
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(state) => {
                let done = last(&state);
                seen.push(state);
                if done {
                    return seen;
                }
            }
            Err(error) => panic!("no such state within {within:?} ({error:?}); saw {seen:?}"),
        }
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

fn has_path(path: &'static str) -> impl Fn(&WatchEvent) -> bool {
    move |event| event.worktree_paths.iter().any(|p| p == path)
}

/// Collects events until a second passes without one (OS watchers deliver late).
fn settle(rx: &Receiver<WatchEvent>) {
    while rx.recv_timeout(Duration::from_secs(1)).is_ok() {}
}

fn ls_files(backend: &dyn Backend) -> String {
    let output = backend.git(&["ls-files"], &GitOpts::default()).unwrap();
    output.stdout_utf8().unwrap().to_string()
}

#[test]
fn the_handshake_confirms_a_build_when_asked() {
    // No build asked: the server doesn't hash itself.
    let connection = Connection::new("loopback", launcher(None));
    connection.connect().unwrap();
    let server = connection.server().unwrap();
    assert_eq!(server.build_ok, None);
    assert_eq!(server.proto, dv_backend::proto::PROTO);
    assert_eq!(server.os, std::env::consts::OS);
    assert_eq!(server.arch, std::env::consts::ARCH);
    assert_eq!(server.version, env!("CARGO_PKG_VERSION"));
    let first = server_pid(&connection);
    assert!(
        connection
            .diagnostics()
            .contains(&format!("{} backend --stdio", env!("CARGO_BIN_EXE_dv"))),
        "{}",
        connection.diagnostics()
    );
    drop(connection);
    assert_gone(first);

    // This very file: confirmed.
    let own = sha256_file(env!("CARGO_BIN_EXE_dv").as_ref()).unwrap();
    let connection = Connection::new("loopback", launcher(Some(own)));
    connection.connect().unwrap();
    assert_eq!(connection.server().unwrap().build_ok, Some(true));
    let second = server_pid(&connection);
    drop(connection);
    assert_gone(second);

    // Another build: refused, and the refused server goes at once.
    let connection = Connection::new("loopback", launcher(Some("0".repeat(64))));
    let error = connection.connect().unwrap_err();
    assert_eq!(
        error.to_string(),
        "loopback can't be opened: the connection to dv failed: the dv in loopback doesn't \
         match this app"
    );
    let diagnostics = connection.diagnostics();
    let refused: u32 = diagnostics
        .split_once(", pid ")
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .and_then(|pid| pid.parse().ok())
        .unwrap_or_else(|| panic!("the refused server's pid in {diagnostics}"));
    assert_gone(refused);
}

#[test]
fn a_repository_opens_and_its_git_store_and_watch_work() {
    let repo = fixture();
    let connection = Connection::new("loopback", launcher(None));
    let backend = open(&connection, &repo);
    let pid = server_pid(&connection);

    // A loopback's repositories are this machine's.
    assert!(
        matches!(&backend.repo().location, Location::Local(_)),
        "{:?}",
        backend.repo().location
    );
    assert_eq!(ls_files(&backend), "a.txt\n");
    assert_eq!(backend.worktree_read("a.txt").unwrap().unwrap(), b"a\n");
    backend.store_append("reviews/r-1.jsonl", b"{}").unwrap();
    assert_eq!(
        backend.store_read("reviews/r-1.jsonl").unwrap().unwrap(),
        b"{}\n"
    );

    let (handle, events) = watch(&backend, WatchSpec::ALL);
    assert_eq!(handle.worktree_error(), None);
    settle(&events);
    repo.write("src/new.txt", "new\n");
    event_where(&events, has_path("src/new.txt"));
    drop(handle);

    drop(backend);
    drop(connection);
    assert_gone(pid);
}

#[test]
fn a_kill_mid_call_disconnects_and_the_connection_comes_back() {
    let repo = fixture();
    let connection = Connection::new("loopback", launcher(None));
    let (_subscription, rx) = states(&connection);
    let backend = Arc::new(open(&connection, &repo));
    states_until(&rx, WITHIN, |state| *state == ConnectionState::Connected);
    let (handle, events) = watch(backend.as_ref(), WatchSpec::WORKTREE);
    settle(&events);
    let first = server_pid(&connection);

    let (tx, done) = mpsc::channel();
    std::thread::spawn({
        let backend = Arc::clone(&backend);
        move || {
            let started = Instant::now();
            let result = backend.git(&["-c", "alias.slow=!sleep 30", "slow"], &GitOpts::default());
            let _ = tx.send((result, started.elapsed()));
        }
    });
    std::thread::sleep(Duration::from_millis(500));
    connection.kill();
    let (result, took) = done.recv_timeout(WITHIN).expect("the call ends");
    let error = result.unwrap_err();
    assert!(
        matches!(&error, BackendError::Disconnected { .. }),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "lost the connection to dv in loopback: dv was killed"
    );
    // At the kill, not when git would have ended.
    assert!(took < Duration::from_secs(10), "at once: {took:?}");

    // Reconnecting: calls fail at once, and a file changes while nobody watches.
    assert_eq!(
        connection.state(),
        ConnectionState::Reconnecting { attempt: 1 }
    );
    let started = Instant::now();
    assert!(matches!(
        backend.worktree_len("a.txt"),
        Err(BackendError::Disconnected { .. })
    ));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    repo.write("while-down.txt", "x\n");

    assert_eq!(
        states_until(&rx, WITHIN, |state| *state == ConnectionState::Connected),
        [
            ConnectionState::Reconnecting { attempt: 1 },
            ConnectionState::Connected
        ]
    );
    assert_eq!(connection.generation(), 2);
    let second = server_pid(&connection);
    assert_ne!(second, first);
    assert_gone(first);

    // The synthetic rescan, then the new server's own batches.
    event_where(&events, |event| event.rescan);
    assert_eq!(ls_files(backend.as_ref()), "a.txt\n");
    let untracked = backend
        .git(&["ls-files", "--others"], &GitOpts::default())
        .unwrap();
    assert_eq!(untracked.stdout_utf8(), Some("while-down.txt\n"));
    repo.write("after.txt", "x\n");
    event_where(&events, has_path("after.txt"));

    drop(handle);
    drop(backend);
    drop(connection);
    assert_gone(second);
}

#[test]
fn respawns_that_fail_leave_the_connection_failed_until_retry() {
    let repo = fixture();
    let blocked = Arc::new(AtomicBool::new(false));
    let pids = Arc::new(Mutex::new(Vec::new()));
    let blockable = Launcher::custom("dv backend --stdio, unless blocked", {
        let blocked = Arc::clone(&blocked);
        move || {
            if blocked.load(Ordering::SeqCst) {
                return Err(io::Error::other("blocked by the test"));
            }
            launcher(None).start()
        }
    });
    // Three short waits stand in for PLAN §5.2's 1, 2 and 5 s (dv_backend's tests hold the
    // schedule): what is under test is the respawns failing, then the retry.
    let connection =
        Connection::with_backoff("loopback", blockable, vec![Duration::from_millis(50); 3]);
    let (_subscription, rx) = states(&connection);
    let backend = open(&connection, &repo);
    let (handle, events) = watch(&backend, WatchSpec::WORKTREE);
    pids.lock().unwrap().push(server_pid(&connection));
    states_until(&rx, WITHIN, |state| *state == ConnectionState::Connected);

    // Three respawns, all blocked.
    blocked.store(true, Ordering::SeqCst);
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
    let error = backend.worktree_len("a.txt").unwrap_err();
    assert!(
        matches!(error, BackendError::Unavailable { .. }),
        "{error:?}"
    );

    // Retry brings the server, the repository and the watch back.
    blocked.store(false, Ordering::SeqCst);
    connection.retry().unwrap();
    assert_eq!(
        states_until(&rx, WITHIN, |state| *state == ConnectionState::Connected),
        [ConnectionState::Connecting, ConnectionState::Connected]
    );
    pids.lock().unwrap().push(server_pid(&connection));
    event_where(&events, |event| event.rescan);
    assert_eq!(ls_files(&backend), "a.txt\n");
    repo.write("back.txt", "x\n");
    event_where(&events, has_path("back.txt"));

    drop(handle);
    drop(backend);
    drop(connection);
    for pid in pids.lock().unwrap().iter() {
        assert_gone(*pid);
    }
}

/// A process `backend` runs, its events on a channel.
fn spawn(backend: &dyn Backend, spec: &ProcessSpec) -> (ProcessHandle, Receiver<ProcessEvent>) {
    let (tx, rx) = mpsc::channel();
    let handle = backend
        .spawn_stdio(
            spec,
            Box::new(move |event| {
                let _ = tx.send(event);
            }),
        )
        .unwrap();
    (handle, rx)
}

/// What a process printed on stdout and stderr until `done` held for its stdout, and its exit
/// when that came.
fn printed(
    rx: &Receiver<ProcessEvent>,
    done: impl Fn(&str, Option<Option<i32>>) -> bool,
) -> (String, Vec<String>, Option<Option<i32>>) {
    let (mut stdout, mut stderr, mut exit) = (Vec::new(), Vec::new(), None);
    let deadline = Instant::now() + WITHIN;
    while !done(&String::from_utf8_lossy(&stdout), exit) {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(ProcessEvent::Stdout(bytes)) => stdout.extend(bytes),
            Ok(ProcessEvent::Stderr(line)) => stderr.push(line),
            Ok(ProcessEvent::Exit(code)) => exit = Some(code),
            Err(error) => panic!("not within {WITHIN:?} ({error:?})"),
        }
    }
    (String::from_utf8_lossy(&stdout).into_owned(), stderr, exit)
}

#[test]
fn a_process_runs_through_the_server_and_ends_with_its_connection() {
    let repo = fixture();
    let connection = Connection::new("loopback", launcher(None));
    let backend = open(&connection, &repo);
    let server = server_pid(&connection);
    // `dv` itself is the process, headless subcommands only, as no one in particular.
    let dv = |args: &[&str]| {
        [
            "CLAUDECODE",
            "DV_AUTHOR",
            "DV_MODEL",
            "DV_REVIEW",
            "RUST_LOG",
        ]
        .into_iter()
        .fold(
            ProcessSpec::new(env!("CARGO_BIN_EXE_dv")).args(args.iter().copied()),
            |spec, name| spec.env(name, None),
        )
    };

    // A usage error: its stderr, and its exit code.
    let (_handle, events) = spawn(&backend, &dv(&["backend"]));
    let (_, stderr, exit) = printed(&events, |_, exit| exit.is_some());
    assert_eq!(exit, Some(Some(2)));
    assert!(
        stderr
            .iter()
            .any(|line| line.contains("dv backend --stdio")),
        "{stderr:?}"
    );

    // Another `dv backend --stdio`: a request written to its stdin, its answer on stdout.
    let (handle, events) = spawn(&backend, &dv(&["backend", "--stdio"]));
    let hello = format!(
        "{{\"id\":1,\"op\":\"hello\",\"proto\":{}}}\n",
        dv_backend::proto::PROTO
    );
    handle.write(hello.as_bytes()).unwrap();
    let (stdout, _, _) = printed(&events, |stdout, _| stdout.ends_with('\n'));
    let reply: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let inner = handle.pid();
    assert_eq!(reply["ok"]["pid"], inner);
    assert_ne!(inner, server);

    // Its server lost: the process ends with an exit, and doesn't outlive it.
    connection.kill();
    let (_, _, exit) = printed(&events, |_, exit| exit.is_some());
    assert_eq!(exit, Some(None));
    assert_gone(server);
    assert_gone(inner);
    drop(handle);
}

#[test]
fn dropping_the_last_connection_ends_the_server() {
    let repo = fixture();
    let connection = Connection::new("loopback", launcher(None));
    let backend = open(&connection, &repo);
    let (handle, _events) = watch(&backend, WatchSpec::ALL);
    let pid = server_pid(&connection);

    // A backend and a watch hold the connection too: the server lives while any does.
    drop(connection);
    drop(backend);
    std::thread::sleep(Duration::from_millis(500));
    assert!(running(pid), "the watch still holds the connection");
    let dropped = Instant::now();
    drop(handle);
    assert_gone(pid);
    assert!(
        dropped.elapsed() < Duration::from_secs(5),
        "ended by its stdin, not killed after the grace: {:?}",
        dropped.elapsed()
    );
}
