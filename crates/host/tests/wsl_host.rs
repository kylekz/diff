//! Real `wsl.exe` smoke tests — the actual risk this slice exists to burn
//! down (docs/phase-5-implementation-plan.md §8 S1: "If S1 fails, the
//! design changes — nothing else lands first"). #[ignore]d: requires a
//! working WSL Ubuntu distro with a Rust toolchain, and `dv-host` already
//! built inside it. Build it first:
//!
//!   wsl.exe -d Ubuntu --exec bash -lc "cd /mnt/d/Software/diff && \
//!     CARGO_TARGET_DIR=\$HOME/.cache/dv-target cargo build -p dv-host"
//!
//! Then run:
//!
//!   cargo test -p dv-host --test wsl_host -- --ignored --nocapture --test-threads=1
//!
//! `--test-threads=1` matters here (found the hard way while validating S4):
//! this file's default (parallel) test runner spawns several `wsl.exe`
//! processes at once, and a loaded WSL distro can flake under that —
//! `spawn_wsl`'s 15s handshake timing out, or a plain setup `wsl.exe --exec
//! sh -c ...` failing outright — for reasons that have nothing to do with
//! the actual behavior under test. Serialized, all 9 tests here pass
//! reliably.
//!
//! `DV_HOST_PATH` overrides the default built-binary path below (must be
//! an absolute POSIX path INSIDE the distro).

use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use dv_core::remote::client::HostClient;
use dv_core::remote::proto::WatchEventParams;

const DEFAULT_HOST_PATH: &str = "/home/kyle/.cache/dv-target/debug/dv-host";
const DISTRO: &str = "Ubuntu";

fn host_path() -> String {
    std::env::var("DV_HOST_PATH").unwrap_or_else(|_| DEFAULT_HOST_PATH.to_string())
}

/// Run `sh -c script` INSIDE the distro via a plain `wsl.exe --exec` —
/// deliberately NOT routed through the `dv-host` connection under test, so
/// setup/mutation here simulates a genuinely external actor (another dv
/// window, the agent CLI) the way the real watch feature needs to react to.
fn wsl_sh(script: &str) {
    let output = std::process::Command::new("wsl.exe")
        .args(["-d", DISTRO, "--exec", "sh", "-c", script])
        .output()
        .expect("wsl.exe --exec sh -c");
    assert!(
        output.status.success(),
        "wsl sh -c {script:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A fresh, throwaway repo path inside the distro — never `~/zed-perf`
/// (that fixture is read/list-only elsewhere; this slice's tests create
/// AND delete their own temp repos so they can't leave anything behind).
fn temp_repo_path(label: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("/tmp/dv-watch-e2e-{label}-{nanos}-{}", std::process::id())
}

#[test]
#[ignore = "requires WSL Ubuntu with dv-host built inside it — see module docs"]
fn wsl_handshake_and_basic_exec() {
    let started = Instant::now();
    let client = HostClient::spawn_wsl(DISTRO, &host_path()).expect("spawn_wsl");
    let handshake_ms = started.elapsed().as_millis();
    eprintln!(
        "handshake: {handshake_ms}ms, version={}, pid={}",
        client.version(),
        client.pid()
    );
    assert!(client.caps().iter().any(|cap| cap == "exec"));

    let outcome = client
        .exec("git", &["--version"], None)
        .expect("exec git --version");
    assert_eq!(outcome.exit_code, 0);
    assert!(String::from_utf8_lossy(&outcome.stdout).starts_with("git version"));
}

#[test]
#[ignore = "requires WSL Ubuntu with dv-host built inside it — see module docs"]
fn wsl_1mb_binary_round_trip_both_directions() {
    let client = HostClient::spawn_wsl(DISTRO, &host_path()).expect("spawn_wsl");
    let mut bytes = vec![0u8; 1024 * 1024];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = (i as u32)
            .wrapping_mul(2_654_435_761)
            .wrapping_add(i as u32) as u8;
    }
    let started = Instant::now();
    // `cat` with no args, given the bytes as stdin: proc/exec spawns the
    // program directly (no shell), so this is a clean byte-for-byte echo
    // with nothing in between to mangle it.
    let outcome = client.exec("cat", &[], Some(&bytes)).expect("exec cat");
    let elapsed = started.elapsed();
    eprintln!("1MB round trip: {}ms", elapsed.as_millis());
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(
        outcome.stdout, bytes,
        "1MB payload must round-trip byte-identical"
    );
}

#[test]
#[ignore = "requires WSL Ubuntu with dv-host built inside it — see module docs"]
fn wsl_nonzero_exit_and_stderr_fidelity() {
    let client = HostClient::spawn_wsl(DISTRO, &host_path()).expect("spawn_wsl");
    let outcome = client
        .exec("git", &["-C", "/nonexistent", "status"], None)
        .expect("exec");
    assert_ne!(outcome.exit_code, 0);
    assert!(!outcome.stderr.is_empty());
    eprintln!("stderr: {}", String::from_utf8_lossy(&outcome.stderr));
}

#[test]
#[ignore = "requires WSL Ubuntu with dv-host built inside it — see module docs"]
fn wsl_concurrent_requests() {
    let client = Arc::new(HostClient::spawn_wsl(DISTRO, &host_path()).expect("spawn_wsl"));
    let handles: Vec<_> = (0..16)
        .map(|t| {
            let client = Arc::clone(&client);
            std::thread::spawn(move || {
                for i in 0..10 {
                    let payload = format!("wsl-concurrency-thread-{t}-iter-{i}");
                    let outcome = client
                        .exec("cat", &[], Some(payload.as_bytes()))
                        .unwrap_or_else(|err| panic!("thread {t} iter {i}: {err:#}"));
                    assert_eq!(outcome.stdout, payload.as_bytes());
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("thread panicked");
    }
}

#[test]
#[ignore = "requires WSL Ubuntu with dv-host built inside it — see module docs"]
fn wsl_utf16_error_path_for_missing_distro() {
    let result = HostClient::spawn_wsl("NoSuchDistro-dv-test", &host_path());
    let err = match result {
        Err(err) => err,
        Ok(_) => panic!("nonexistent distro must fail"),
    };
    let message = format!("{err:#}");
    eprintln!("error message: {message}");
    // The point of decode_output at handshake: this must be *readable*
    // text, not UTF-16LE mojibake (every-other-byte NUL, which renders as
    // boxes/garbage if the raw bytes are printed as if they were UTF-8).
    assert!(
        !message.contains('\u{0}'),
        "message should be decoded, not raw UTF-16: {message:?}"
    );
}

#[test]
#[ignore = "requires WSL Ubuntu with dv-host built inside it — see module docs"]
fn wsl_drop_leaves_no_orphan() {
    let client = HostClient::spawn_wsl(DISTRO, &host_path()).expect("spawn_wsl");
    let pid = client.pid();
    drop(client);
    std::thread::sleep(Duration::from_millis(500));
    let output = std::process::Command::new("wsl.exe")
        .args(["-d", DISTRO, "--exec", "pgrep", "-f", "dv-host"])
        .output()
        .expect("pgrep");
    let running = String::from_utf8_lossy(&output.stdout);
    eprintln!("pgrep dv-host after drop: {running:?} (dropped client's pid was {pid})");
    assert!(
        running.trim().is_empty(),
        "dv-host still running inside the distro after drop: {running}"
    );
}

// --- watch/subscribe over a REAL `wsl.exe`-spawned host (plan §8 S4) -----

#[test]
#[ignore = "requires WSL Ubuntu with dv-host built inside it — see module docs"]
fn wsl_store_watch_event_within_1s_then_unsubscribe_silences_it() {
    let repo = temp_repo_path("store");
    wsl_sh(&format!("mkdir -p '{repo}' && cd '{repo}' && git init -q"));

    let client = HostClient::spawn_wsl(DISTRO, &host_path()).expect("spawn_wsl");
    assert!(
        client.caps().iter().any(|cap| cap == "watch"),
        "caps: {:?}",
        client.caps()
    );

    let (tx, rx) = mpsc::channel::<WatchEventParams>();
    let watch_id = client
        .watch_subscribe(&repo, "store")
        .expect("watch/subscribe store");
    client.register_watch_callback(watch_id, move |params| {
        let _ = tx.send(params);
    });

    // An external actor (a separate `wsl.exe --exec`, not this
    // connection) writing a review file — exactly the "agent CLI in one
    // process, GUI watching in another" shape this whole feature exists
    // for.
    let started = Instant::now();
    wsl_sh(&format!(
        "mkdir -p '{repo}/.git/dv/reviews' && echo '{{}}' > '{repo}/.git/dv/reviews/r-test.json'"
    ));
    let event = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("expected a store watch/event within 1s of an external write");
    eprintln!("store watch event latency: {:?}", started.elapsed());
    assert_eq!(event.watch_id, watch_id);
    assert_eq!(event.kind, "store");

    client
        .watch_unsubscribe(watch_id)
        .expect("watch/unsubscribe");
    while rx.try_recv().is_ok() {} // drain anything already in flight
    wsl_sh(&format!(
        "echo '{{}}' > '{repo}/.git/dv/reviews/r-test-2.json'"
    ));
    assert!(
        rx.recv_timeout(Duration::from_millis(1500)).is_err(),
        "must not receive watch/event after unsubscribe"
    );

    wsl_sh(&format!("rm -rf '{repo}'"));
}

#[test]
#[ignore = "requires WSL Ubuntu with dv-host built inside it — see module docs"]
fn wsl_worktree_watch_event_on_tracked_file_edit() {
    let repo = temp_repo_path("worktree");
    wsl_sh(&format!(
        "mkdir -p '{repo}' && cd '{repo}' && git init -q && echo hello > a.txt && \
         git add a.txt && git -c user.email=t@t.com -c user.name=t commit -q -m seed"
    ));

    let client = HostClient::spawn_wsl(DISTRO, &host_path()).expect("spawn_wsl");

    let (tx, rx) = mpsc::channel::<WatchEventParams>();
    let watch_id = client
        .watch_subscribe(&repo, "worktree")
        .expect("watch/subscribe worktree");
    client.register_watch_callback(watch_id, move |params| {
        let _ = tx.send(params);
    });

    let started = Instant::now();
    wsl_sh(&format!("echo 'hello again' >> '{repo}/a.txt'"));
    let event = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("expected a worktree watch/event within 1s of an external edit");
    eprintln!("worktree watch event latency: {:?}", started.elapsed());
    assert_eq!(event.watch_id, watch_id);
    assert_eq!(event.kind, "worktree");

    client.watch_unsubscribe(watch_id).ok();
    wsl_sh(&format!("rm -rf '{repo}'"));
}

#[test]
#[ignore = "requires WSL Ubuntu with dv-host built inside it — see module docs"]
fn wsl_kill_9_host_then_respawn_still_supports_a_fresh_watch() {
    // Plan §8 S1's "kill -9 → errors surface → respawn works" gate,
    // extended to prove a FRESH watch_subscribe on the respawned
    // connection still works — a stale watcher from the killed process
    // obviously can't (its whole process is gone), so this is really
    // about confirming the NEW `HostClient` (a plain fresh `spawn_wsl`,
    // same as `remote::manager`'s real respawn path) gets a working watch
    // from scratch.
    let repo = temp_repo_path("respawn");
    wsl_sh(&format!("mkdir -p '{repo}' && cd '{repo}' && git init -q"));

    let client = HostClient::spawn_wsl(DISTRO, &host_path()).expect("spawn_wsl (first)");
    let pid = client.pid();
    let kill = std::process::Command::new("wsl.exe")
        .args(["-d", DISTRO, "--exec", "kill", "-9", &pid.to_string()])
        .output()
        .expect("kill -9 inside the distro");
    eprintln!(
        "kill -9 {pid}: status={:?} stderr={:?}",
        kill.status,
        String::from_utf8_lossy(&kill.stderr)
    );
    assert!(
        wait_until(Duration::from_secs(5), || !client.is_alive()),
        "reader thread should observe EOF after kill -9"
    );

    let respawned = HostClient::spawn_wsl(DISTRO, &host_path()).expect("spawn_wsl (respawn)");
    let (tx, rx) = mpsc::channel::<WatchEventParams>();
    let watch_id = respawned
        .watch_subscribe(&repo, "store")
        .expect("watch/subscribe on the respawned connection");
    respawned.register_watch_callback(watch_id, move |params| {
        let _ = tx.send(params);
    });
    wsl_sh(&format!(
        "mkdir -p '{repo}/.git/dv/reviews' && echo '{{}}' > '{repo}/.git/dv/reviews/r-test.json'"
    ));
    rx.recv_timeout(Duration::from_secs(1))
        .expect("respawned connection's watch/subscribe should still receive events");

    wsl_sh(&format!("rm -rf '{repo}'"));
}

// --- fs/* over a REAL `wsl.exe`-spawned host (plan §8 S5) ----------------

#[test]
#[ignore = "requires WSL Ubuntu with dv-host built inside it — see module docs"]
fn fs_round_trip_through_host() {
    let repo = temp_repo_path("fs");
    wsl_sh(&format!("mkdir -p '{repo}' && cd '{repo}' && git init -q"));

    let client = HostClient::spawn_wsl(DISTRO, &host_path()).expect("spawn_wsl");
    assert!(
        client.caps().iter().any(|cap| cap == "fs"),
        "caps: {:?}",
        client.caps()
    );

    let rel = "dv/reviews/r-test.json";
    let bytes = b"{\"v\":1,\"id\":\"r-test\"}".to_vec();

    // Nothing written yet: fs/read must report not-found, not an error.
    assert!(
        client
            .fs_read(&repo, rel)
            .expect("fs/read (absent)")
            .is_none(),
        "fs/read of a not-yet-written file must be Ok(None)"
    );

    client
        .fs_write_atomic(&repo, rel, &bytes)
        .expect("fs/write_atomic");

    let read_back = client
        .fs_read(&repo, rel)
        .expect("fs/read (present)")
        .expect("file was just written, must be found");
    assert_eq!(
        read_back, bytes,
        "fs/read must return byte-identical content"
    );

    let names = client.fs_list(&repo, "dv/reviews").expect("fs/list");
    assert!(
        names.iter().any(|n| n == "r-test.json"),
        "fs/list should see the just-written file: {names:?}"
    );

    client.fs_remove(&repo, rel).expect("fs/remove");
    assert!(
        client
            .fs_read(&repo, rel)
            .expect("fs/read (after remove)")
            .is_none(),
        "fs/read after fs/remove must report not-found"
    );

    wsl_sh(&format!("rm -rf '{repo}'"));
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
