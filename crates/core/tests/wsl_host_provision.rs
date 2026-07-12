//! Live WSL install-flow tests (docs/phase-5-implementation-plan.md §8 S3
//! gate): exercises [`dv_core::remote::install`] against a REAL locally-built
//! linux `dv-host` binary (the same one `crates/host/tests/wsl_host.rs` and
//! `crates/core/tests/wsl_host_routing.rs` use), standing in for the
//! Windows-side sidecar via `DV_HOST_SIDECAR`.
//!
//! Named `wsl_host_provision` rather than the more obvious `wsl_install`
//! deliberately: on Windows, an unmanifested `.exe` whose *filename*
//! contains "install" (or "setup"/"update"/"patch") gets silently
//! auto-elevated by the shell's installer-detection heuristic — cargo's
//! test binary is named after this file, so `wsl_install-<hash>.exe` failed
//! to launch at all (`ERROR_ELEVATION_REQUIRED`, os error 740) with no WSL
//! or dv-core involvement whatsoever. Confirmed by copying the exact same
//! built exe under a differently-named copy and finding it ran fine.
//!
//! #[ignore]d: requires a working WSL Ubuntu distro with a Rust toolchain
//! and `dv-host` already built inside it. Build it first:
//!
//!   wsl.exe -d Ubuntu --exec bash -lc "cd /mnt/d/Software/diff && \
//!     CARGO_TARGET_DIR=\$HOME/.cache/dv-target cargo build -p dv-host"
//!
//! Then run:
//!
//!   cargo test -p dv-core --test wsl_host_provision -- --ignored --nocapture
//!
//! Every test here mutates real process-global env state (`DV_HOST_PATH`,
//! `DV_HOST_SIDECAR`, `DV_HOST_INSTALL_ROOT`) — `ENV_TEST_MUTEX` serializes
//! them (matching `wsl_host_routing.rs`'s convention) so they're safe to run
//! together even without `--test-threads=1`. Each test uses its own
//! `/tmp/dv-test-<pid>-<tag>` install root (never the real
//! `~/.local/share/dv/host`) and removes it in an RAII guard's `Drop`, which
//! also fires on a mid-test panic (assertion failure) so a failed run
//! doesn't leave the env vars dirty for whichever test runs next.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::Mutex;

use dv_core::remote::HostClient;
use dv_core::remote::install::{self, HostBinarySource};

const DISTRO: &str = "Ubuntu";
/// The real, already-built linux `dv-host`, reached over the 9P mount —
/// same binary and build command `wsl_host_routing.rs`/`wsl_host.rs` use.
const REAL_HOST_UNC: &str = r"\\wsl.localhost\Ubuntu\home\kyle\.cache\dv-target\debug\dv-host";

static ENV_TEST_MUTEX: Mutex<()> = Mutex::new(());

fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner())
}

fn wsl(args: &[&str]) -> Output {
    Command::new("wsl.exe")
        .args(["-d", DISTRO, "--exec"])
        .args(args)
        .output()
        .expect("spawn wsl.exe")
}

fn wsl_sh(script: &str) -> Output {
    wsl(&["sh", "-c", script])
}

fn read_remote(path: &str) -> String {
    let out = wsl_sh(&format!("cat '{path}'"));
    assert!(
        out.status.success(),
        "cat {path} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn list_remote_dir(dir: &str) -> Vec<String> {
    let out = wsl(&["ls", "-1", dir]);
    assert!(
        out.status.success(),
        "ls {dir} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

/// Copy the real linux `dv-host` out to a throwaway Windows temp file —
/// reading it over the 9P mount for test setup is fine (module doc of the
/// S3 task this file implements). `tag` keeps concurrently-running tests'
/// copies from colliding.
fn copy_real_sidecar(tag: &str) -> PathBuf {
    let bytes = std::fs::read(REAL_HOST_UNC).unwrap_or_else(|err| {
        panic!(
            "reading real dv-host at {REAL_HOST_UNC}: {err}\n\
             Build it first: wsl.exe -d Ubuntu --exec bash -lc \"cd /mnt/d/Software/diff && \
             CARGO_TARGET_DIR=\\$HOME/.cache/dv-target cargo build -p dv-host\""
        )
    });
    let dest = std::env::temp_dir().join(format!(
        "dv-wsl-host-provision-test-sidecar-{}-{tag}",
        std::process::id()
    ));
    std::fs::write(&dest, &bytes).expect("write temp sidecar copy");
    dest
}

/// RAII fixture: points `DV_HOST_SIDECAR`/`DV_HOST_INSTALL_ROOT` at a
/// throwaway sidecar copy and a `/tmp/dv-test-<pid>-<tag>` root (never the
/// real `~/.local/share/dv/host`), clears `DV_HOST_PATH` so a stray value
/// left over from a previous failed run can't silently bypass the sidecar
/// flow, and cleans everything up on `Drop` — the remote root, the env
/// vars, AND the local temp sidecar copy (takes ownership of it, since
/// nothing after construction needs the file to still exist except through
/// this fixture) — including on a mid-test panic.
struct EnvFixture {
    root: String,
    sidecar: PathBuf,
}

impl EnvFixture {
    fn new(sidecar: PathBuf, tag: &str) -> Self {
        let root = format!("/tmp/dv-test-{}-{tag}", std::process::id());
        // Belt-and-suspenders: a previous run that panicked before its own
        // Drop ran could have left this exact root behind.
        let _ = wsl(&["rm", "-rf", &root]);
        // SAFETY: test-only env mutation, scoped to this process; guarded
        // by `test_env_lock` against concurrent access from other tests in
        // this file that touch the same env vars.
        unsafe {
            std::env::remove_var("DV_HOST_PATH");
            std::env::set_var("DV_HOST_SIDECAR", &sidecar);
            std::env::set_var("DV_HOST_INSTALL_ROOT", &root);
        }
        Self { root, sidecar }
    }
}

impl Drop for EnvFixture {
    fn drop(&mut self) {
        // SAFETY: see `new` above — same guarded, test-only env mutation.
        unsafe {
            std::env::remove_var("DV_HOST_SIDECAR");
            std::env::remove_var("DV_HOST_INSTALL_ROOT");
        }
        let _ = wsl(&["rm", "-rf", &self.root]);
        let _ = std::fs::remove_file(&self.sidecar);
    }
}

/// (a) Fresh install: the binary lands, the marker is a well-formed sha256
/// hex digest, and the freshly-installed binary actually spawns and
/// handshakes — `HostClient::spawn_wsl` returning `Ok` at all is itself
/// proof the proto version matched (a mismatch makes it `bail!`).
#[test]
#[ignore = "requires WSL Ubuntu with dv-host built inside it — see module docs"]
fn a_fresh_install_lands_binary_and_handshakes() {
    let _lock = test_env_lock();
    let sidecar = copy_real_sidecar("a");
    let _fixture = EnvFixture::new(sidecar, "install-a");

    let binary = install::ensure_installed(DISTRO).expect("ensure_installed");
    assert_eq!(binary.source, HostBinarySource::Managed);
    assert!(
        binary.path.ends_with("/dv-host"),
        "unexpected binary path: {}",
        binary.path
    );

    let dir = binary.path.trim_end_matches("/dv-host");
    let marker = read_remote(&format!("{dir}/dv-host.sha256"));
    let marker = marker.trim();
    assert_eq!(
        marker.len(),
        64,
        "marker should be a sha256 hex digest: {marker:?}"
    );
    assert!(
        marker.chars().all(|c| c.is_ascii_hexdigit()),
        "marker should be hex: {marker:?}"
    );

    let client = HostClient::spawn_wsl(DISTRO, &binary.path)
        .expect("spawn_wsl the freshly installed binary");
    assert!(client.caps().iter().any(|cap| cap == "exec"));
    eprintln!(
        "fresh install: path={} version={} pid={}",
        binary.path,
        client.version(),
        client.pid()
    );
}

/// (b) A corrupted marker (the actual signal `ensure_installed` checks —
/// see `install.rs`'s module doc: it compares the marker's stored hash
/// against a freshly-computed LOCAL sidecar hash, it never re-hashes the
/// remote binary on every call) is detected as a mismatch, triggers a
/// reinstall, and the marker verifies correctly again afterward.
#[test]
#[ignore = "requires WSL Ubuntu with dv-host built inside it — see module docs"]
fn b_corrupted_marker_triggers_reinstall_and_reverifies() {
    let _lock = test_env_lock();
    let sidecar = copy_real_sidecar("b");
    let _fixture = EnvFixture::new(sidecar, "install-b");

    let first = install::ensure_installed(DISTRO).expect("first ensure_installed");
    let dir = first.path.trim_end_matches("/dv-host").to_string();
    let marker_path = format!("{dir}/dv-host.sha256");

    let original_marker = read_remote(&marker_path).trim().to_string();
    assert_eq!(original_marker.len(), 64);

    // Corrupt the marker with something that can never equal a real sha256
    // hex digest — simulates the install being interrupted/tampered with
    // after the binary landed but the recorded hash no longer reflects it.
    let corrupt = wsl_sh(&format!("printf 'not-a-real-hash' > '{marker_path}'"));
    assert!(corrupt.status.success());
    assert_eq!(read_remote(&marker_path).trim(), "not-a-real-hash");

    let second =
        install::ensure_installed(DISTRO).expect("second ensure_installed after corruption");
    assert_eq!(
        second.path, first.path,
        "same sidecar bytes must resolve to the same content-hash directory"
    );

    let repaired_marker = read_remote(&marker_path);
    assert_eq!(
        repaired_marker.trim(),
        original_marker,
        "reinstall must restore the correct marker"
    );

    // And the reinstalled binary is runnable/correct again.
    let client =
        HostClient::spawn_wsl(DISTRO, &second.path).expect("spawn_wsl the reinstalled binary");
    drop(client);
}

/// (c) Different sidecar bytes hash to a different directory; the old
/// directory is left in place rather than being cleaned up (plan §5: "Old
/// dirs kept").
#[test]
#[ignore = "requires WSL Ubuntu with dv-host built inside it — see module docs"]
fn c_changed_sidecar_bytes_create_a_new_dir_and_keep_the_old_one() {
    let _lock = test_env_lock();
    let sidecar = copy_real_sidecar("c");
    let fixture = EnvFixture::new(sidecar, "install-c");

    let first = install::ensure_installed(DISTRO).expect("first ensure_installed");
    let listing_before = list_remote_dir(&fixture.root);
    assert_eq!(
        listing_before.len(),
        1,
        "exactly one version dir after the first install: {listing_before:?}"
    );

    // Append a byte to the LOCAL sidecar copy (owned by `fixture` now):
    // different content -> a different sha256 -> a different install
    // directory.
    let mut bytes = std::fs::read(&fixture.sidecar).expect("read temp sidecar");
    bytes.push(0xAB);
    std::fs::write(&fixture.sidecar, &bytes).expect("append byte to temp sidecar");

    let second = install::ensure_installed(DISTRO)
        .expect("second ensure_installed with modified sidecar bytes");
    assert_ne!(
        second.path, first.path,
        "different content must land in a different directory"
    );

    let listing_after = list_remote_dir(&fixture.root);
    assert_eq!(
        listing_after.len(),
        2,
        "old dir kept, new dir added: {listing_after:?}"
    );
    let dir_name = |path: &str| -> String {
        path.trim_end_matches("/dv-host")
            .rsplit('/')
            .next()
            .expect("path has a final component")
            .to_string()
    };
    assert!(listing_after.contains(&dir_name(&first.path)));
    assert!(listing_after.contains(&dir_name(&second.path)));
}

/// (d) `DV_HOST_PATH` set bypasses the sidecar/install flow entirely — the
/// install root is never even touched, let alone created.
#[test]
#[ignore = "requires WSL Ubuntu with dv-host built inside it — see module docs"]
fn d_dv_host_path_bypasses_install_and_leaves_the_root_untouched() {
    let _lock = test_env_lock();
    let root = format!("/tmp/dv-test-{}-install-d", std::process::id());
    let dev_path = "/home/kyle/.cache/dv-target/debug/dv-host";
    let _ = wsl(&["rm", "-rf", &root]);

    // SAFETY: test-only env mutation; guarded by test_env_lock above.
    unsafe {
        std::env::remove_var("DV_HOST_SIDECAR");
        std::env::set_var("DV_HOST_PATH", dev_path);
        std::env::set_var("DV_HOST_INSTALL_ROOT", &root);
    }

    let binary = install::ensure_installed(DISTRO).expect("ensure_installed with DV_HOST_PATH set");
    assert_eq!(binary.source, HostBinarySource::DevOverride);
    assert_eq!(binary.path, dev_path);

    let test_dir = wsl(&["test", "-d", &root]);
    assert!(
        !test_dir.status.success(),
        "install root must never have been created: {root}"
    );

    unsafe {
        std::env::remove_var("DV_HOST_PATH");
        std::env::remove_var("DV_HOST_INSTALL_ROOT");
    }
}
