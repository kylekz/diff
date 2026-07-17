//! Exercises the REAL `dv-host` binary (this package's own bin target —
//! `env!("CARGO_BIN_EXE_dv-host")`, built automatically by `cargo test`)
//! through the REAL `dv_core::remote::client::HostClient`, on whatever
//! platform `cargo test` runs on. This is the S1 risk-burner's
//! cross-process proof that client and host agree on the wire shapes
//! byte-for-byte — no WSL required (see tests/wsl_host.rs for the real
//! `wsl.exe` path, which is #[ignore]d).
//!
//! dv-core reaches the shipped `dv-host` binary itself as of S4 (see
//! Cargo.toml's comment — `watch.rs`'s gitdir resolution reuses
//! `dv_core::review::resolve_local_git_dir`), on top of already being this
//! test binary's own dependency (driving the real `HostClient`). S2 added
//! `blob/get` coverage and the cross-process Spawn/Host error-text identity
//! proof (plan §8 S2 gate); S4 adds `watch/subscribe`|`watch/unsubscribe`
//! coverage (real `notify`/inotify-equivalent watching, not WSL-specific —
//! runs on every CI platform).

use std::io::Write as _;
use std::process::Command;
use std::sync::mpsc;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use dv_core::remote::client::HostClient;
use dv_core::remote::proto::WatchEventParams;
use dv_core::{CommandBuilder, RepoLocation};

/// Blocks for up to `timeout` waiting for a `watch/event` on `rx` (fed by a
/// callback registered via [`HostClient::register_watch_callback`]).
/// `None` on timeout — the "prove silence after unsubscribe" tests below
/// use that to assert a NEGATIVE (no event arrived), so this deliberately
/// returns an `Option` rather than panicking on timeout the way `expect`
/// would.
fn recv_watch_event(
    rx: &mpsc::Receiver<WatchEventParams>,
    timeout: Duration,
) -> Option<WatchEventParams> {
    rx.recv_timeout(timeout).ok()
}

fn spawn_host() -> HostClient {
    HostClient::spawn_command(Command::new(env!("CARGO_BIN_EXE_dv-host")))
        .expect("spawning dv-host")
}

#[test]
fn handshake_reports_proto_1_and_exec_cap() {
    let client = spawn_host();
    assert!(client.is_alive());
    assert!(client.pid() > 0);
    assert!(
        client.caps().iter().any(|cap| cap == "exec"),
        "caps: {:?}",
        client.caps()
    );
    assert!(!client.version().is_empty());
}

#[test]
fn exec_git_version() {
    let client = spawn_host();
    let outcome = client.exec("git", &["--version"], None).expect("exec");
    assert_eq!(outcome.exit_code, 0);
    let stdout = String::from_utf8(outcome.stdout).expect("git --version is ascii");
    assert!(stdout.starts_with("git version"), "{stdout:?}");
    assert!(outcome.stderr.is_empty());
}

#[test]
fn exec_nonzero_exit_carries_code_and_stderr() {
    let client = spawn_host();
    let outcome = client
        .exec(
            "git",
            &["rev-parse", "--verify", "nonexistent-ref-xyz"],
            None,
        )
        .expect("exec (non-zero exit is an Ok result, not an Err)");
    assert_ne!(outcome.exit_code, 0);
    let stderr = String::from_utf8_lossy(&outcome.stderr);
    assert!(
        !stderr.trim().is_empty(),
        "expected stderr text, got {stderr:?}"
    );
}

#[test]
fn exec_binary_stdin_stdout_round_trips_byte_identical() {
    let client = spawn_host();
    let dir = temp_dir("dv-host-blob");
    run_git(&["init", "-q"], &dir);

    // All 256 byte values, plus a deterministic pseudo-random tail so the
    // payload isn't just a monotonic ramp (that alone wouldn't catch e.g.
    // a byte-swap or off-by-one truncation bug).
    let mut bytes: Vec<u8> = (0..=255u8).collect();
    for i in 0..4096u32 {
        bytes.push(i.wrapping_mul(2_654_435_761).wrapping_add(i) as u8);
    }

    let dir_str = dir.to_str().expect("utf8 temp dir path");
    let write = client
        .exec(
            "git",
            &["-C", dir_str, "hash-object", "-w", "--stdin"],
            Some(&bytes),
        )
        .expect("hash-object");
    assert_eq!(
        write.exit_code,
        0,
        "stderr: {}",
        String::from_utf8_lossy(&write.stderr)
    );
    let sha = String::from_utf8(write.stdout)
        .expect("sha is ascii")
        .trim()
        .to_string();

    let read = client
        .exec("git", &["-C", dir_str, "cat-file", "-p", &sha], None)
        .expect("cat-file");
    assert_eq!(read.exit_code, 0);
    assert_eq!(
        read.stdout, bytes,
        "blob content must round-trip byte-identical"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn concurrent_requests_do_not_cross_talk() {
    let client = Arc::new(spawn_host());
    const THREADS: usize = 16;
    const ITERS: usize = 10;
    let barrier = Arc::new(Barrier::new(THREADS));

    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let client = Arc::clone(&client);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                for i in 0..ITERS {
                    let payload = format!("dv-host-concurrency-thread-{t}-iter-{i}-marker");
                    let expected = local_hash_object(payload.as_bytes());
                    let outcome = client
                        .exec("git", &["hash-object", "--stdin"], Some(payload.as_bytes()))
                        .unwrap_or_else(|err| panic!("thread {t} iter {i}: {err:#}"));
                    assert_eq!(outcome.exit_code, 0, "thread {t} iter {i}");
                    let got = String::from_utf8(outcome.stdout)
                        .unwrap()
                        .trim()
                        .to_string();
                    assert_eq!(
                        got, expected,
                        "thread {t} iter {i}: cross-talk (got a different request's hash)"
                    );
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("worker thread panicked");
    }
}

#[test]
fn drop_closes_stdin_and_host_exits() {
    let client = spawn_host();
    let pid = client.pid();
    assert!(
        process_is_running(pid),
        "host should be running right after spawn"
    );
    drop(client);
    assert!(
        wait_until(Duration::from_secs(5), || !process_is_running(pid)),
        "dv-host (pid {pid}) is still running after HostClient was dropped — orphan process"
    );
}

#[test]
fn kill_fails_pending_and_future_requests_promptly() {
    let client = Arc::new(spawn_host());
    let started = Instant::now();
    let deadline = started + Duration::from_secs(10);

    let worker = {
        let client = Arc::clone(&client);
        std::thread::spawn(move || -> Result<String, String> {
            loop {
                match client.exec("git", &["--version"], None) {
                    Ok(_) => {
                        if Instant::now() > deadline {
                            return Err("never observed a failure".to_string());
                        }
                    }
                    Err(err) => return Ok(err.to_string()),
                }
            }
        })
    };

    std::thread::sleep(Duration::from_millis(30));
    client.kill();

    let result = worker.join().expect("worker thread panicked");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "failure took {elapsed:?} — looks like it fell through to the 300s request timeout \
         instead of observing the reader thread's EOF fast-path"
    );
    result.expect("exec loop should observe an error after kill(), not run forever");
    assert!(!client.is_alive());
}

#[test]
fn blob_get_found_by_rev_path() {
    let client = spawn_host();
    let dir = temp_dir("dv-host-blob-get");
    run_git(&["init", "-q"], &dir);
    std::fs::write(dir.join("a.txt"), b"hello blob\n").unwrap();
    run_git(&["add", "a.txt"], &dir);
    run_git(
        &[
            "-c",
            "user.email=t@t.com",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "seed",
        ],
        &dir,
    );

    let root = dir.to_str().unwrap();
    let bytes = client
        .blob_get(root, "HEAD:a.txt")
        .expect("blob_get")
        .expect("blob must be found");
    assert_eq!(bytes, b"hello blob\n");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn blob_get_missing_spec_returns_none() {
    let client = spawn_host();
    let dir = temp_dir("dv-host-blob-get-missing");
    run_git(&["init", "-q"], &dir);
    run_git(
        &[
            "-c",
            "user.email=t@t.com",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "seed",
        ],
        &dir,
    );

    let root = dir.to_str().unwrap();
    let result = client
        .blob_get(root, "HEAD:does-not-exist.txt")
        .expect("blob_get");
    assert!(
        result.is_none(),
        "missing path must report None, not an error"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn blob_get_binary_blob_by_raw_oid_round_trips() {
    let client = spawn_host();
    let dir = temp_dir("dv-host-blob-get-binary");
    run_git(&["init", "-q"], &dir);

    let mut bytes: Vec<u8> = (0..=255u8).collect();
    for i in 0..4096u32 {
        bytes.push(i.wrapping_mul(2_654_435_761).wrapping_add(i) as u8);
    }
    let write = client
        .exec(
            "git",
            &["-C", dir.to_str().unwrap(), "hash-object", "-w", "--stdin"],
            Some(&bytes),
        )
        .expect("hash-object");
    assert_eq!(write.exit_code, 0);
    let oid = String::from_utf8(write.stdout).unwrap().trim().to_string();

    let got = client
        .blob_get(dir.to_str().unwrap(), &oid)
        .expect("blob_get")
        .expect("blob must be found");
    assert_eq!(got, bytes, "binary blob must round-trip byte-identical");

    std::fs::remove_dir_all(&dir).ok();
}

/// The plan §8 S2 gate: `CommandBuilder::run`'s error text for a failing
/// command must be byte-identical whether it ran via the Stage-A `wsl.exe`
/// spawn (here: a plain local spawn, since this test isn't run inside WSL)
/// or via a `dv-host` `proc/exec` response. `CommandBuilder::with_host`
/// (a real, non-test-gated constructor — see its doc comment) is what lets
/// this test force the Host route without a `RepoLocation::Wsl` to route
/// `manager::client_for` through.
#[test]
fn spawn_and_host_routes_produce_byte_identical_error_text() {
    let dir = temp_dir("dv-host-error-identity");
    run_git(&["init", "-q"], &dir);
    let dir_str = dir.to_str().unwrap().to_string();
    let fail_args = [
        "-C",
        dir_str.as_str(),
        "rev-parse",
        "--verify",
        "nonexistent-ref-xyz",
    ];

    let spawn_builder = CommandBuilder::new(RepoLocation::Local(dir.clone()));
    let spawn_err = spawn_builder
        .run("git", &fail_args)
        .expect_err("unknown ref must fail");

    let client = Arc::new(spawn_host());
    let host_builder = CommandBuilder::with_host(RepoLocation::Local(dir.clone()), client);
    let host_err = host_builder
        .run("git", &fail_args)
        .expect_err("unknown ref must fail via the host route too");

    assert_eq!(
        format!("{spawn_err:#}"),
        format!("{host_err:#}"),
        "Spawn and Host routes must produce byte-identical error text for the same failing command"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Plan §8 S2 review findings P2-1/P2-2 regression: a structured RPC error
/// the host itself answered with (here, `exec_spawn_failed` — the host's
/// OWN attempt to spawn a nonexistent program failing) must NOT trigger
/// `CommandBuilder::run`'s transparent Spawn-arm fallback. Before this fix,
/// ANY `Err` from `client.exec` fell back silently, masking the host's
/// answer behind a differently-shaped local "spawn failed" error (and,
/// separately, would have marked the host `Dead` for a failure that says
/// nothing about the CONNECTION's health). Asserting the host's own
/// `exec_spawn_failed` code string survives verbatim proves neither
/// happened.
#[test]
fn rpc_error_from_host_is_not_masked_by_spawn_fallback() {
    let dir = temp_dir("dv-host-rpc-not-masked");
    let client = Arc::new(spawn_host());
    let builder = CommandBuilder::with_host(RepoLocation::Local(dir.clone()), client);

    let err = builder
        .run("dv-test-nonexistent-program-xyz", &[])
        .expect_err("a program the host can't spawn must fail");
    let message = format!("{err:#}");
    eprintln!("error message: {message}");
    assert!(
        message.contains("exec_spawn_failed"),
        "expected the host's own exec_spawn_failed RPC error to surface verbatim, not a \
         Spawn-arm 'spawn failed' message: {message:?}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Positive-case counterpart to the test above: a genuine CONNECTION-level
/// failure (the host process killed out from under an in-flight
/// `CommandBuilder`) must still get the transparent Spawn-arm fallback —
/// proving the P2-2 fix (Timeout/Rpc no longer fall back) didn't
/// accidentally take Connection-kind fallback down with it.
#[test]
fn connection_failure_still_falls_back_to_spawn() {
    let client = Arc::new(spawn_host());
    client.kill();
    assert!(
        wait_until(Duration::from_secs(5), || !client.is_alive()),
        "reader thread should observe EOF and flip alive to false after kill()"
    );

    let builder = CommandBuilder::with_host(RepoLocation::Local(std::env::temp_dir()), client);
    let out = builder
        .run("git", &["--version"])
        .expect("a connection-level failure must still fall back to a local spawn");
    assert!(
        String::from_utf8_lossy(&out).starts_with("git version"),
        "fallback output should be the local `git --version`, got {:?}",
        String::from_utf8_lossy(&out)
    );
}

// --- watch/subscribe, watch/unsubscribe, watch/event (plan §8 S4) --------
//
// Real `notify` watching (inotify on Linux, ReadDirectoryChangesW on
// Windows, FSEvents on macOS) — not WSL-specific, so these run on every CI
// platform, same posture as `blob::tests` above. `HostClient::has_cap`
// isn't asserted directly here (that's `handshake_reports_proto_1_and_exec_cap`'s
// job to extend) but every test below only works AT ALL if the real host's
// hello advertised "watch", so a regression there would fail loudly here
// too.

#[test]
fn handshake_advertises_watch_cap() {
    let client = spawn_host();
    assert!(
        client.caps().iter().any(|cap| cap == "watch"),
        "caps: {:?}",
        client.caps()
    );
}

#[test]
fn watch_store_touch_fires_event_then_unsubscribe_silences_it() {
    let client = spawn_host();
    let dir = temp_dir("dv-host-watch-store");
    run_git(&["init", "-q"], &dir);
    let root = dir.to_str().unwrap();

    let (tx, rx) = mpsc::channel::<WatchEventParams>();
    let watch_id = client
        .watch_subscribe(root, "store")
        .expect("watch/subscribe store");
    client.register_watch_callback(watch_id, move |params| {
        let _ = tx.send(params);
    });

    // `subscribe` create_dir_all's this before returning, so it exists by
    // the time watch_subscribe's response comes back.
    let reviews_dir = dir.join(".git").join("dv").join("reviews");
    std::fs::write(reviews_dir.join("r-test.json"), b"{}").expect("write review file");

    let event = recv_watch_event(&rx, Duration::from_secs(5))
        .expect("expected a watch/event after touching a file under dv/reviews");
    assert_eq!(event.watch_id, watch_id);
    assert_eq!(event.kind, "store");

    client
        .watch_unsubscribe(watch_id)
        .expect("watch/unsubscribe");
    // SETTLE, don't just drain: the first write can have late/duplicate
    // notifications still in flight through the host when unsubscribe
    // returns (Windows readily delivers two Modify events for one write),
    // and a bare `try_recv` drain only clears what has ALREADY crossed the
    // channel — a straggler landing a moment later was then misread as a
    // post-unsubscribe event (the windows-latest CI flake this test was
    // known for). Consume events until the channel stays quiet for a full
    // second; only then does the negative assertion below prove anything
    // about the FRESH write.
    while recv_watch_event(&rx, Duration::from_secs(1)).is_some() {}
    std::fs::write(reviews_dir.join("r-test-2.json"), b"{}").expect("write second review file");
    assert!(
        recv_watch_event(&rx, Duration::from_millis(800)).is_none(),
        "must not receive watch/event after unsubscribe"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn watch_worktree_ignores_dot_git_but_sees_tracked_file_edits() {
    let client = spawn_host();
    let dir = temp_dir("dv-host-watch-worktree");
    run_git(&["init", "-q"], &dir);
    std::fs::write(dir.join("a.txt"), b"hello\n").unwrap();
    let root = dir.to_str().unwrap();

    let (tx, rx) = mpsc::channel::<WatchEventParams>();
    let watch_id = client
        .watch_subscribe(root, "worktree")
        .expect("watch/subscribe worktree");
    client.register_watch_callback(watch_id, move |params| {
        let _ = tx.send(params);
    });

    // An edit under `.git/` must NOT produce a worktree event.
    std::fs::write(dir.join(".git").join("dv-host-test-marker"), b"x").unwrap();
    assert!(
        recv_watch_event(&rx, Duration::from_millis(800)).is_none(),
        "an edit under .git/ must not fire a worktree watch/event"
    );

    // A tracked-file edit must.
    std::fs::write(dir.join("a.txt"), b"hello again\n").unwrap();
    let event = recv_watch_event(&rx, Duration::from_secs(5))
        .expect("expected a watch/event after editing a tracked file");
    assert_eq!(event.watch_id, watch_id);
    assert_eq!(event.kind, "worktree");

    client.watch_unsubscribe(watch_id).ok();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn watch_overflow_flag_set_beyond_1000_coalesced_paths() {
    // 1500 plain sequential `fs::write` calls routinely take WELL over
    // 200ms wall clock (~700ms measured on a Windows dev machine — NTFS
    // per-file create overhead, antivirus, whatever) — comfortably longer
    // than the real `COALESCE_WINDOW`, which would just split the burst
    // across several under-1000 batches and never actually exercise the
    // overflow path. `DV_HOST_WATCH_COALESCE_MS` (a test-only override —
    // see `crates/host/src/watch.rs`'s `coalesce_window`) widens the
    // window enough that the whole loop below reliably lands in ONE
    // batch, so the overflow flag itself — not incidental write-loop
    // timing — is what's under test.
    let client = HostClient::spawn_command({
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_dv-host"));
        cmd.env("DV_HOST_WATCH_COALESCE_MS", "5000");
        cmd
    })
    .expect("spawning dv-host with a widened coalesce window");
    let dir = temp_dir("dv-host-watch-overflow");
    run_git(&["init", "-q"], &dir);
    let root = dir.to_str().unwrap();

    let (tx, rx) = mpsc::channel::<WatchEventParams>();
    let watch_id = client
        .watch_subscribe(root, "store")
        .expect("watch/subscribe store");
    client.register_watch_callback(watch_id, move |params| {
        let _ = tx.send(params);
    });

    let reviews_dir = dir.join(".git").join("dv").join("reviews");
    for i in 0..1500 {
        std::fs::write(reviews_dir.join(format!("r-overflow-{i}.json")), b"{}")
            .unwrap_or_else(|err| panic!("write {i}: {err}"));
    }

    let event = recv_watch_event(&rx, Duration::from_secs(10))
        .expect("expected at least one coalesced watch/event for the burst");
    assert!(
        event.overflow,
        "1500 paths in one burst must set overflow (host caps a batch at 1000)"
    );
    assert!(
        event.paths.len() <= 1000,
        "capped batch must carry at most 1000 paths, got {}",
        event.paths.len()
    );

    client.watch_unsubscribe(watch_id).ok();
    std::fs::remove_dir_all(&dir).ok();
}

fn local_hash_object(bytes: &[u8]) -> String {
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn git hash-object");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(bytes)
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait git hash-object");
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn run_git(args: &[&str], cwd: &std::path::Path) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

fn temp_dir(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("{label}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(windows)]
fn process_is_running(pid: u32) -> bool {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .expect("tasklist");
    String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
}

#[cfg(not(windows))]
fn process_is_running(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}
