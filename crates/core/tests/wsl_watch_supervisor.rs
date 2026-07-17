//! Live WSL proof of the remote-watcher supervisor (docs/backlog.md
//! "Remote watchers — resubscribe-on-respawn + subscribe off the GUI
//! thread"): a store watch and a worktree watch armed through the REAL
//! `remote::manager` path survive a `pkill -9 dv-host` mid-session — the
//! supervisor detects the death, waits out the (test-shortened) cool-down,
//! resubscribes on the respawned host, and events flow again. Also proves
//! dropping a watcher DURING an outage is clean: fast, silent, no leaked
//! supervisor thread.
//!
//! #[ignore]d: requires a working WSL Ubuntu distro with `dv-host` built
//! inside it (see crates/host/tests/wsl_host.rs's module doc for the build
//! command). Run manually:
//!
//!   DV_HOST_PATH=/home/kyle/.cache/dv-target/debug/dv-host \
//!   cargo test -p dv-core --test wsl_watch_supervisor -- --ignored --nocapture
//!
//! Mutates real process-global state (env vars, the `remote::manager`
//! registry) — this file has exactly one test function so nothing can race
//! it, same convention as wsl_host_routing.rs.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use dv_core::remote::manager;
use dv_core::{RepoLocation, ReviewStore};

const DISTRO: &str = "Ubuntu";

fn host_path() -> String {
    std::env::var("DV_HOST_PATH")
        .unwrap_or_else(|_| "/home/kyle/.cache/dv-target/debug/dv-host".to_string())
}

/// Run `sh -c script` inside the distro via a plain `wsl.exe --exec` — an
/// actor genuinely external to the connection under test (same helper shape
/// as crates/host/tests/wsl_host.rs).
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

fn pkill_dv_host() {
    let output = std::process::Command::new("wsl.exe")
        .args(["-d", DISTRO, "--exec", "pkill", "-9", "-f", "dv-host"])
        .output()
        .expect("pkill dv-host inside the distro");
    eprintln!(
        "pkill dv-host: status={:?} stderr={:?}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn temp_repo_path(label: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(
        "/tmp/dv-watch-supervisor-{label}-{nanos}-{}",
        std::process::id()
    )
}

fn drain(rx: &mpsc::Receiver<Instant>, quiet: Duration) {
    while rx.recv_timeout(quiet).is_ok() {}
}

#[test]
#[ignore = "requires WSL Ubuntu with dv-host built inside it — see module docs"]
fn wsl_watchers_survive_host_kill_and_unsubscribe_cleanly_during_outage() {
    // SAFETY: test-only env mutation; single-test file (see module doc).
    unsafe {
        std::env::set_var("DV_HOST_PATH", host_path());
        std::env::set_var("DV_HOST_COOLDOWN_MS", "500");
    }
    manager::enable_hosts();

    let repo = temp_repo_path("repo");
    wsl_sh(&format!(
        "mkdir -p '{repo}' && cd '{repo}' && git init -q && echo hello > a.txt && \
         git add a.txt && git -c user.email=t@t.com -c user.name=t commit -q -m seed"
    ));
    let location = RepoLocation::Wsl {
        distro: DISTRO.to_string(),
        path: repo.clone(),
    };

    // ---- (a) store watch: kill -> respawn -> events flow again ----------
    {
        let store = ReviewStore::open(location.clone());
        let (tx, rx) = mpsc::channel::<Instant>();
        let watcher = store
            .watch(Box::new(move || {
                let _ = tx.send(Instant::now());
            }))
            .expect("store watch");

        // First-arm synthetic (the supervisor's "armed now" signal) — the
        // very first host spawn for this process happens here, so allow a
        // cold-boot budget.
        rx.recv_timeout(Duration::from_secs(20))
            .expect("expected the store watcher's first-arm synthetic");
        drain(&rx, Duration::from_millis(500));

        // A genuinely external store write flows through host inotify.
        let write_at = Instant::now();
        wsl_sh(&format!(
            "mkdir -p '{repo}/.git/dv/reviews' && echo '{{}}' > '{repo}/.git/dv/reviews/r-1.json'"
        ));
        let seen = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("expected a store event through the live host");
        eprintln!(
            "(a) pre-kill store event latency: {:?}",
            seen.duration_since(write_at)
        );
        drain(&rx, Duration::from_millis(500));

        // Kill the host. The supervisor must fire again WITHOUT any new
        // repo open: first the fallback/re-arm synthetic, then — after the
        // 500ms cool-down elapses and the manager respawns — a live
        // resubscription.
        let killed_at = Instant::now();
        pkill_dv_host();
        let first_after_kill = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("expected the watcher to fire again after the kill (re-arm synthetic)");
        eprintln!(
            "(a) kill -> first watcher signal: {:?}",
            first_after_kill.duration_since(killed_at)
        );
        // Give the respawn cycle time to complete, then prove a fresh
        // external write still fires — the "no longer permanently silent"
        // headline assertion.
        drain(&rx, Duration::from_millis(1500));
        let write2_at = Instant::now();
        wsl_sh(&format!("echo '{{}}' > '{repo}/.git/dv/reviews/r-2.json'"));
        let seen2 = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("expected a store event after the host respawn");
        eprintln!(
            "(a) post-respawn store event latency: {:?} (kill -> recovered event: {:?})",
            seen2.duration_since(write2_at),
            seen2.duration_since(killed_at)
        );
        drop(watcher);
    }

    // ---- (b) worktree watch: kill -> respawn -> events flow again -------
    {
        let (tx, rx) = mpsc::channel::<Instant>();
        let watcher = dv_core::remote::watch_worktree(
            location.clone(),
            Box::new(move || {
                let _ = tx.send(Instant::now());
            }),
        )
        .expect("watch_worktree should return a supervised watcher");

        // No first-arm synthetic for worktree; wait for the subscription
        // to arm (host is already running from (a), so this is fast), then
        // prove a tracked-file edit fires.
        std::thread::sleep(Duration::from_millis(1500));
        let edit_at = Instant::now();
        wsl_sh(&format!("echo 'edit-1' >> '{repo}/a.txt'"));
        let seen = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("expected a worktree event through the live host");
        eprintln!(
            "(b) pre-kill worktree event latency: {:?}",
            seen.duration_since(edit_at)
        );
        drain(&rx, Duration::from_millis(500));

        let killed_at = Instant::now();
        pkill_dv_host();
        // Worktree has no fallback: the next signal IS the resubscribe
        // synthetic — its timestamp measures the outage-to-recovery gap
        // directly.
        let synthetic = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("expected the worktree re-arm synthetic after the kill");
        eprintln!(
            "(b) kill -> worktree resubscribe synthetic: {:?}",
            synthetic.duration_since(killed_at)
        );
        drain(&rx, Duration::from_millis(500));
        let edit2_at = Instant::now();
        wsl_sh(&format!("echo 'edit-2' >> '{repo}/a.txt'"));
        let seen2 = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("expected a worktree event after the host respawn");
        eprintln!(
            "(b) post-respawn worktree event latency: {:?}",
            seen2.duration_since(edit2_at)
        );
        drop(watcher);
    }

    // ---- (c) unsubscribe during an outage: clean, silent, no hang -------
    {
        let store = ReviewStore::open(location.clone());
        let (tx, rx) = mpsc::channel::<Instant>();
        let watcher = store
            .watch(Box::new(move || {
                let _ = tx.send(Instant::now());
            }))
            .expect("store watch (c)");
        rx.recv_timeout(Duration::from_secs(10))
            .expect("expected (c)'s first-arm synthetic");
        drain(&rx, Duration::from_millis(500));

        pkill_dv_host();
        // Drop mid-outage, before the cool-down can elapse: must return
        // immediately (teardown happens on the supervisor thread).
        std::thread::sleep(Duration::from_millis(200));
        let drop_at = Instant::now();
        drop(watcher);
        let drop_took = drop_at.elapsed();
        eprintln!("(c) drop during outage took {drop_took:?}");
        assert!(
            drop_took < Duration::from_millis(500),
            "dropping a watcher during an outage must not block ({drop_took:?})"
        );
        // Silence: a store write after the drop (and after the respawn
        // cool-down would have elapsed) must deliver nothing.
        std::thread::sleep(Duration::from_millis(1500));
        wsl_sh(&format!(
            "echo '{{}}' > '{repo}/.git/dv/reviews/r-post-drop.json'"
        ));
        assert!(
            rx.recv_timeout(Duration::from_secs(2)).is_err(),
            "no events may be delivered after the watcher was dropped during the outage"
        );
    }

    wsl_sh(&format!("rm -rf '{repo}'"));
    unsafe {
        std::env::remove_var("DV_HOST_PATH");
        std::env::remove_var("DV_HOST_COOLDOWN_MS");
    }
}
