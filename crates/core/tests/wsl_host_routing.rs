//! Live WSL routing test (docs/phase-5-implementation-plan.md §8 S2 gate):
//! enables the `dv-host` transport for a real `wsl.exe`-spawned host and
//! proves `GitRepo` operations routed through `Route::Host` produce
//! results identical to a `DV_NO_HOST=1` Stage-A control run — including
//! transparent fallback when the host is killed mid-session, and a fresh
//! respawn once the (test-shortened) cool-down elapses.
//!
//! #[ignore]d: requires a working WSL Ubuntu distro with `dv-host` already
//! built inside it (see crates/host/tests/wsl_host.rs's module doc for the
//! build command) and a git repo at `~/zed-perf` (matches
//! crates/core/tests/review_integration.rs's `wsl_store_round_trip`).
//!
//! Run manually:
//!
//!   DV_HOST_PATH=/home/kyle/.cache/dv-target/debug/dv-host \
//!   cargo test -p dv-core --test wsl_host_routing -- --ignored --nocapture
//!
//! Everything here mutates real process-global state (env vars, the
//! `remote::manager` registry, the fallback counter) — this file has
//! exactly one test function so there's no risk of two ignored tests
//! racing each other's env mutation.

use dv_core::remote::manager;
use dv_core::{BlobSpec, DiffSource, GitRepo, RepoLocation};

const DISTRO: &str = "Ubuntu";
const REPO_PATH: &str = "/home/kyle/zed-perf";

/// Guards this test's env mutation, matching the pattern already
/// established in `crates/core/src/github/client.rs`'s
/// `resolve_gh_path_honors_dv_gh_env_var` test.
static ENV_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn location() -> RepoLocation {
    RepoLocation::Wsl {
        distro: DISTRO.to_string(),
        path: REPO_PATH.to_string(),
    }
}

fn host_path() -> String {
    std::env::var("DV_HOST_PATH")
        .unwrap_or_else(|_| "/home/kyle/.cache/dv-target/debug/dv-host".to_string())
}

#[test]
#[ignore = "requires WSL Ubuntu with dv-host built inside it and a repo at ~/zed-perf"]
fn wsl_routes_transparently_with_fallback_and_respawn() {
    let _env_guard = ENV_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    // SAFETY: test-only env mutation; this file has exactly one test and
    // it's #[ignore]d (never runs alongside other tests in normal `cargo
    // test`), and the mutex above still protects it against a manual
    // `--ignored` run that includes other env-mutating tests.
    unsafe {
        std::env::set_var("DV_HOST_PATH", host_path());
        // Shrinks the 30s default cool-down so the post-kill respawn
        // assertion below doesn't need a real 30-second sleep (plan §8 S2:
        // "make cool-down configurable via env or a test hook").
        std::env::set_var("DV_HOST_COOLDOWN_MS", "500");
    }

    // --- control run: Stage-A, hosts fully disabled ---------------------
    unsafe {
        std::env::set_var("DV_NO_HOST", "1");
    }
    let control = GitRepo::open(location()).expect("open control repo (DV_NO_HOST=1)");
    let control_files = control
        .changed_files(&DiffSource::WorkingTree)
        .expect("control changed_files");
    let control_head = control.head_label().expect("control head_label");
    let control_blob = control
        .blob_bytes(&BlobSpec::Rev {
            rev: "HEAD".to_string(),
            path: "Cargo.toml".to_string(),
        })
        .expect("control blob_bytes");
    unsafe {
        std::env::remove_var("DV_NO_HOST");
    }

    // --- routed run: hosts enabled, host stays alive ---------------------
    manager::enable_hosts();
    assert_eq!(
        manager::spawn_fallback_count(),
        0,
        "counter must start at 0 before any routed call"
    );

    let repo = GitRepo::open(location()).expect("open routed repo");
    let routed_files = repo
        .changed_files(&DiffSource::WorkingTree)
        .expect("routed changed_files");
    let routed_head = repo.head_label().expect("routed head_label");
    let routed_blob = repo
        .blob_bytes(&BlobSpec::Rev {
            rev: "HEAD".to_string(),
            path: "Cargo.toml".to_string(),
        })
        .expect("routed blob_bytes");

    assert_eq!(
        routed_files, control_files,
        "changed_files must match the Stage-A control run"
    );
    assert_eq!(
        routed_head, control_head,
        "head_label must match the Stage-A control run"
    );
    assert_eq!(
        routed_blob, control_blob,
        "blob_bytes must match the Stage-A control run"
    );
    assert_eq!(
        manager::spawn_fallback_count(),
        0,
        "zero wsl.exe spawn fallbacks while the host stayed alive for the whole routed session"
    );

    // --- kill the host mid-session; prove transparent fallback -----------
    let pkill = std::process::Command::new("wsl.exe")
        .args(["-d", DISTRO, "--exec", "pkill", "-9", "-f", "dv-host"])
        .output()
        .expect("pkill dv-host inside the distro");
    eprintln!(
        "pkill dv-host: status={:?} stdout={:?} stderr={:?}",
        pkill.status,
        String::from_utf8_lossy(&pkill.stdout),
        String::from_utf8_lossy(&pkill.stderr)
    );

    let after_kill = repo
        .head_label()
        .expect("head_label must still succeed via transparent fallback after the host dies");
    assert_eq!(
        after_kill, control_head,
        "fallback result must still be correct"
    );
    let fallback_count_after_kill = manager::spawn_fallback_count();
    assert!(
        fallback_count_after_kill >= 1,
        "expected at least one fallback after killing the host, got {fallback_count_after_kill}"
    );
    eprintln!("spawn_fallback_count after kill = {fallback_count_after_kill}");

    // --- cool-down elapses (shortened above to 500ms); a FRESH
    // CommandBuilder (via a fresh GitRepo::open) should get a live
    // Route::Host again — proven by the fallback counter NOT increasing
    // for calls through it, rather than by the mere absence of an error
    // (which a repeated fallback would also produce). --------------------
    std::thread::sleep(std::time::Duration::from_millis(700));
    let count_before_respawn = manager::spawn_fallback_count();

    let repo2 = GitRepo::open(location()).expect("reopen after cool-down");
    let respawned_head = repo2
        .head_label()
        .expect("head_label via the respawned host");
    assert_eq!(respawned_head, control_head);

    let count_after_respawn = manager::spawn_fallback_count();
    assert_eq!(
        count_after_respawn, count_before_respawn,
        "a fresh CommandBuilder after cool-down should get a live Route::Host again (no further \
         fallback needed) — count went {count_before_respawn} -> {count_after_respawn}"
    );

    eprintln!("final spawn_fallback_count = {count_after_respawn}");

    unsafe {
        std::env::remove_var("DV_HOST_PATH");
        std::env::remove_var("DV_HOST_COOLDOWN_MS");
    }
}
