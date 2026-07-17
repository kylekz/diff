//! Live WSL test for the durable-concurrency slice (docs/backlog.md:
//! "Review store: durable concurrency answer ... the real fix is a lock
//! file around load-mutate-save", Phase-2 review P1 residual): proves
//! [`ReviewStore::with_lock`] actually serializes concurrent writers over
//! the REAL `fs/create_exclusive` RPC through a live `wsl.exe`-spawned
//! `dv-host`, not just the local-filesystem path every other test in this
//! workspace exercises.
//!
//! #[ignore]d: requires a working WSL Ubuntu distro with `dv-host` already
//! built inside it (see crates/host/tests/wsl_host.rs's module doc for the
//! build command — `fs/create_exclusive`/the `fs_lock` cap need a build
//! from AFTER this slice landed, so rebuild if you have an older cached
//! binary). Run manually:
//!
//!   DV_HOST_PATH=/home/kyle/.cache/dv-target/debug/dv-host \
//!   cargo test -p dv-core --test wsl_review_lock -- --ignored --nocapture

use std::sync::Arc;

use dv_core::remote::manager;
use dv_core::{DiffSource, RepoLocation, ReviewStore};

const DISTRO: &str = "Ubuntu";

/// Guards this test's env mutation, matching the pattern
/// `crates/core/tests/wsl_host_routing.rs` already established.
static ENV_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn host_path() -> String {
    std::env::var("DV_HOST_PATH")
        .unwrap_or_else(|_| "/home/kyle/.cache/dv-target/debug/dv-host".to_string())
}

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

fn temp_repo_path() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("/tmp/dv-review-lock-e2e-{nanos}-{}", std::process::id())
}

#[test]
#[ignore = "requires WSL Ubuntu with dv-host built inside it (see crates/host/tests/wsl_host.rs's module doc for the build command)"]
fn review_store_lock_over_a_live_wsl_host_loses_no_updates() {
    let _env_guard = ENV_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    let repo_path = temp_repo_path();
    wsl_sh(&format!(
        "mkdir -p '{repo_path}' && cd '{repo_path}' && git init -q"
    ));

    // SAFETY: test-only env mutation; the mutex above serializes this
    // against any other env-mutating test in this binary run alongside it
    // under a manual `--ignored` pass. Never runs during the normal `cargo
    // test` gate.
    unsafe {
        std::env::set_var("DV_HOST_PATH", host_path());
    }
    manager::enable_hosts();

    let location = RepoLocation::Wsl {
        distro: DISTRO.to_string(),
        path: repo_path.clone(),
    };
    let store = Arc::new(ReviewStore::open(location));

    // Zero fallback before anything runs — the baseline `wsl_host_routing.rs`
    // already asserts on `manager::client_for` directly (this file can't:
    // it's `pub(crate)`, and this is a separate integration-test crate that
    // only sees `pub` items). `spawn_fallback_count` staying at 0 through
    // the whole test is this file's proof that every load/save below
    // genuinely routed through the live host's `fs/create_exclusive` RPC
    // rather than silently falling back to the `sh -c`/`noclobber` path —
    // an older cached `dv-host` binary (built before this slice, so it
    // doesn't advertise `fs_lock`) would still "work" via that fallback,
    // which is exactly the case this test exists to rule out.
    assert_eq!(
        manager::spawn_fallback_count(),
        0,
        "must start at 0 before any routed call"
    );

    let review = store
        .create(DiffSource::WorkingTree)
        .expect("create draft review over the live host connection");
    let review_id = review.id.clone();

    const PER_THREAD: usize = 25;
    const THREADS: usize = 3;
    let mut handles = Vec::new();
    for t in 0..THREADS {
        let store = Arc::clone(&store);
        let review_id = review_id.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..PER_THREAD {
                store
                    .with_lock(|| -> anyhow::Result<()> {
                        let mut review = store
                            .load(&review_id)?
                            .expect("review must still exist mid-test");
                        review.add_comment(
                            "a.txt",
                            dv_core::Side::New,
                            1,
                            1,
                            None,
                            format!("t{t}-c{i}"),
                            format!("worker-{t}"),
                        )?;
                        store.save(&review)?;
                        Ok(())
                    })
                    .unwrap_or_else(|err| {
                        panic!("with_lock over the live host connection failed: {err:#}")
                    });
            }
        }));
    }
    for h in handles {
        h.join().expect("worker thread panicked");
    }

    let final_review = store
        .load(&review_id)
        .expect("final load")
        .expect("review must still exist");
    assert_eq!(
        final_review.comments.len(),
        THREADS * PER_THREAD,
        "every comment from every thread must have landed — got {} of {}",
        final_review.comments.len(),
        THREADS * PER_THREAD
    );
    assert_eq!(
        manager::spawn_fallback_count(),
        0,
        "zero wsl.exe spawn fallbacks — every load/save/lock op above must have routed through \
         the live host's fs/create_exclusive RPC, not the sh -c fallback"
    );

    unsafe {
        std::env::remove_var("DV_HOST_PATH");
    }
    wsl_sh(&format!("rm -rf '{repo_path}'"));
}
