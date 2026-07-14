//! Live WSL install-flow tests for the native `dv` CLI
//! (docs/phase-8-lsp-and-polish.md's onboarding spine, S8c task): exercises
//! [`dv_core::remote::install`]'s [`CLI_SPEC`]/[`InstallLayout::StablePath`]
//! path against a REAL locally-built musl `dv-cli` binary (S8b), standing in
//! for the Windows-side sidecar via `DV_CLI_SIDECAR`. Mirrors
//! `crates/core/tests/wsl_host_provision.rs`'s structure (a/b/c/d), which in
//! turn mirrors `crates/host/tests/wsl_host.rs`.
//!
//! **Unlike `dv-host`'s `HashDir` layout, `CLI_SPEC`'s `bin_dir`
//! (`~/.local/bin`) is NEVER redirected by `DV_HOST_INSTALL_ROOT`** (see
//! `install.rs`'s `stable_dirs`/`InstallLayout::StablePath` doc) — a fresh
//! install here really does stream into the real `~/.local/bin/dv` inside
//! the `Ubuntu` distro. That's intentional (docs/phase-8-lsp-and-polish.md
//! doc-deviation 5: dv owns the `dv` name there) and harmless/self-healing
//! across runs — an atomic pid-tmp + hash-gated `mv`, same as `dv-host`.
//! Only the MARKER directory is redirected to a throwaway root by this
//! fixture, so the drift signal these tests assert on never touches the
//! real `~/.local/share/dv/cli/dv.sha256`.
//!
//! Named `wsl_cli_provision` (not `wsl_cli_install`) for the same reason
//! `wsl_host_provision.rs` isn't `wsl_install`: Windows silently
//! auto-elevates an unmanifested `.exe` whose filename contains "install"
//! (cargo's test binary is named after this file).
//!
//! Build the sidecar first (WSL, ext4 target dir — 9P is punishingly slow):
//!
//!   wsl.exe -d Ubuntu --exec bash -lc "cd /mnt/d/Software/diff && \
//!     CARGO_TARGET_DIR=\$HOME/.cache/dv-target cargo build -p dv-cli --release \
//!     --target x86_64-unknown-linux-musl"
//!
//! Then run:
//!
//!   cargo test -p dv-core --test wsl_cli_provision -- --ignored --nocapture
//!
//! Every test here mutates real process-global env state (`DV_CLI_SIDECAR`,
//! `DV_CLI_PATH`, `DV_HOST_INSTALL_ROOT`) — `ENV_TEST_MUTEX` serializes them
//! (matching `wsl_host_provision.rs`'s convention). Each test's marker root
//! is its own `/tmp/dv-test-<pid>-<tag>` (never the real
//! `~/.local/share/dv/cli`), removed in an RAII guard's `Drop` — including
//! on a mid-test panic.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::Mutex;

use dv_core::remote::install::{self, HostBinarySource};

const DISTRO: &str = "Ubuntu";
/// The real, already-built musl `dv-cli` (S8b), reached over the 9P mount.
const REAL_CLI_UNC: &str =
    r"\\wsl.localhost\Ubuntu\home\kyle\.cache\dv-target\x86_64-unknown-linux-musl\release\dv-cli";

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

fn remote_sha256(path: &str) -> String {
    let out = wsl_sh(&format!("sha256sum < '{path}' | cut -d' ' -f1"));
    assert!(
        out.status.success(),
        "sha256sum {path} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn remote_mtime(path: &str) -> String {
    let out = wsl_sh(&format!("stat -c %Y '{path}'"));
    assert!(
        out.status.success(),
        "stat {path} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Copy the real musl `dv-cli` out to a throwaway Windows temp file —
/// reading it over the 9P mount for test setup is fine (same convention
/// `wsl_host_provision.rs` uses for `dv-host`). `tag` keeps concurrently-run
/// tests' copies from colliding.
fn copy_real_sidecar(tag: &str) -> PathBuf {
    let bytes = std::fs::read(REAL_CLI_UNC).unwrap_or_else(|err| {
        panic!(
            "reading real dv-cli at {REAL_CLI_UNC}: {err}\n\
             Build it first: wsl.exe -d Ubuntu --exec bash -lc \"cd /mnt/d/Software/diff && \
             CARGO_TARGET_DIR=\\$HOME/.cache/dv-target cargo build -p dv-cli --release \
             --target x86_64-unknown-linux-musl\""
        )
    });
    let dest = std::env::temp_dir().join(format!(
        "dv-wsl-cli-provision-test-sidecar-{}-{tag}",
        std::process::id()
    ));
    std::fs::write(&dest, &bytes).expect("write temp sidecar copy");
    dest
}

/// RAII fixture: points `DV_CLI_SIDECAR`/`DV_HOST_INSTALL_ROOT` (the shared
/// install-root override — see `install.rs`'s `INSTALL_ROOT_ENV` doc) at a
/// throwaway sidecar copy and a `/tmp/dv-test-<pid>-<tag>` marker root
/// (never the real `~/.local/share/dv/cli`), clears `DV_CLI_PATH` so a
/// stray value from a previous failed run can't silently bypass the sidecar
/// flow, and cleans everything up on `Drop` — the remote marker root, the
/// env vars, AND the local temp sidecar copy — including on a mid-test
/// panic. Does NOT touch `~/.local/bin/dv`: that's the real, on-`PATH`
/// stable path (see module doc) and this fixture leaves it exactly as
/// `ensure_cli_installed` writes it.
struct EnvFixture {
    marker_root: String,
    sidecar: PathBuf,
}

impl EnvFixture {
    fn new(sidecar: PathBuf, tag: &str) -> Self {
        let marker_root = format!("/tmp/dv-test-{}-{tag}", std::process::id());
        // Belt-and-suspenders: a previous run that panicked before its own
        // Drop ran could have left this exact root behind.
        let _ = wsl(&["rm", "-rf", &marker_root]);
        // SAFETY: test-only env mutation, scoped to this process; guarded
        // by `test_env_lock` against concurrent access from other tests in
        // this file that touch the same env vars.
        unsafe {
            std::env::remove_var("DV_CLI_PATH");
            std::env::set_var("DV_CLI_SIDECAR", &sidecar);
            std::env::set_var("DV_HOST_INSTALL_ROOT", &marker_root);
        }
        Self {
            marker_root,
            sidecar,
        }
    }

    fn marker_path(&self) -> String {
        format!("{}/dv.sha256", self.marker_root)
    }
}

impl Drop for EnvFixture {
    fn drop(&mut self) {
        // SAFETY: see `new` above — same guarded, test-only env mutation.
        unsafe {
            std::env::remove_var("DV_CLI_SIDECAR");
            std::env::remove_var("DV_HOST_INSTALL_ROOT");
        }
        let _ = wsl(&["rm", "-rf", &self.marker_root]);
        let _ = std::fs::remove_file(&self.sidecar);
    }
}

/// (a) Fresh install: the binary lands at the REAL stable path
/// (`~/.local/bin/dv`), the (throwaway-rooted) marker is a well-formed
/// sha256 hex digest matching the sidecar's content hash, and the remote
/// binary's own content hash matches too (proof the bytes actually landed,
/// not just the marker).
#[test]
#[ignore = "requires WSL Ubuntu with the musl dv-cli built inside it — see module docs"]
fn a_fresh_install_lands_binary_at_the_stable_path_and_writes_marker() {
    let _lock = test_env_lock();
    let sidecar = copy_real_sidecar("a");
    let fixture = EnvFixture::new(sidecar, "cli-a");

    let installed = install::ensure_cli_installed(DISTRO).expect("ensure_cli_installed");
    assert_eq!(installed.source, HostBinarySource::Managed);
    assert!(
        installed.path.ends_with("/.local/bin/dv"),
        "unexpected binary path: {}",
        installed.path
    );

    let marker = read_remote(&fixture.marker_path());
    let marker = marker.trim();
    assert_eq!(
        marker.len(),
        64,
        "marker should be a sha256 hex digest: {marker:?}"
    );
    assert_eq!(
        marker, installed.hash,
        "marker must match the returned hash"
    );

    let remote_hash = remote_sha256(&installed.path);
    assert_eq!(
        remote_hash, installed.hash,
        "the streamed binary's own content hash must match what was verified"
    );

    // cli_install_marker (the fast, no-streaming read S8d's consistency
    // check uses) must agree with what ensure_cli_installed just wrote.
    let fast_read = install::cli_install_marker(DISTRO).expect("cli_install_marker");
    assert_eq!(fast_read, Some(installed.hash));
}

/// (b) Unchanged-hash re-call is a no-op: calling `ensure_cli_installed`
/// again with the exact same sidecar bytes must NOT re-stream — proven by
/// the remote binary's mtime staying exactly the same across both calls.
#[test]
#[ignore = "requires WSL Ubuntu with the musl dv-cli built inside it — see module docs"]
fn b_unchanged_hash_re_call_is_a_no_op() {
    let _lock = test_env_lock();
    let sidecar = copy_real_sidecar("b");
    let _fixture = EnvFixture::new(sidecar, "cli-b");

    let first = install::ensure_cli_installed(DISTRO).expect("first ensure_cli_installed");
    let mtime_after_first = remote_mtime(&first.path);

    let second = install::ensure_cli_installed(DISTRO).expect("second ensure_cli_installed");
    let mtime_after_second = remote_mtime(&second.path);

    assert_eq!(second.path, first.path);
    assert_eq!(second.hash, first.hash);
    assert_eq!(
        mtime_after_second, mtime_after_first,
        "same-hash re-call must not re-stream (mtime must be unchanged)"
    );
}

/// (c) A changed sidecar (different content, different hash) re-streams:
/// the stable path binary is overwritten in place and its remote content
/// hash changes to match the new sidecar.
#[test]
#[ignore = "requires WSL Ubuntu with the musl dv-cli built inside it — see module docs"]
fn c_changed_sidecar_bytes_re_stream_the_stable_path_binary() {
    let _lock = test_env_lock();
    let sidecar = copy_real_sidecar("c");
    let fixture = EnvFixture::new(sidecar, "cli-c");

    let first = install::ensure_cli_installed(DISTRO).expect("first ensure_cli_installed");

    // Append a byte to the LOCAL sidecar copy (owned by `fixture` now):
    // different content -> a different sha256 -> a required re-stream.
    let mut bytes = std::fs::read(&fixture.sidecar).expect("read temp sidecar");
    bytes.push(0xAB);
    std::fs::write(&fixture.sidecar, &bytes).expect("append byte to temp sidecar");

    let second = install::ensure_cli_installed(DISTRO)
        .expect("second ensure_cli_installed with modified sidecar bytes");
    assert_eq!(
        second.path, first.path,
        "the stable path never moves, even across content changes"
    );
    assert_ne!(
        second.hash, first.hash,
        "different sidecar bytes must produce a different verified hash"
    );

    let remote_hash = remote_sha256(&second.path);
    assert_eq!(
        remote_hash, second.hash,
        "the re-streamed binary's content hash must match the new sidecar"
    );

    let marker = read_remote(&fixture.marker_path()).trim().to_string();
    assert_eq!(
        marker, second.hash,
        "marker must be updated to the new hash"
    );
}

/// (d) A deleted marker forces reinstall even though the (unrelated,
/// still-correct) binary bytes never changed — the marker is the SOLE
/// drift signal for a `StablePath` layout (S8c binding (4)).
#[test]
#[ignore = "requires WSL Ubuntu with the musl dv-cli built inside it — see module docs"]
fn d_deleted_marker_forces_reinstall() {
    let _lock = test_env_lock();
    let sidecar = copy_real_sidecar("d");
    let fixture = EnvFixture::new(sidecar, "cli-d");

    let first = install::ensure_cli_installed(DISTRO).expect("first ensure_cli_installed");
    let original_marker = read_remote(&fixture.marker_path()).trim().to_string();
    assert_eq!(original_marker.len(), 64);

    // cli_install_marker must agree there's a marker before we delete it.
    assert_eq!(
        install::cli_install_marker(DISTRO).expect("cli_install_marker before deletion"),
        Some(original_marker.clone())
    );

    let rm = wsl_sh(&format!("rm -f '{}'", fixture.marker_path()));
    assert!(rm.status.success());

    // With the marker gone, the fast no-streaming read must report drift
    // (None) even though the binary at the stable path is still present
    // and byte-correct — never assumed-good (S8c binding (4)).
    assert_eq!(
        install::cli_install_marker(DISTRO).expect("cli_install_marker after deletion"),
        None
    );

    let second =
        install::ensure_cli_installed(DISTRO).expect("second ensure_cli_installed after deletion");
    assert_eq!(second.path, first.path);
    assert_eq!(
        second.hash, first.hash,
        "same sidecar bytes must re-verify to the same hash"
    );

    let repaired_marker = read_remote(&fixture.marker_path()).trim().to_string();
    assert_eq!(
        repaired_marker, original_marker,
        "reinstall must restore the correct marker"
    );
}

/// (e) `DV_CLI_PATH` set bypasses the sidecar/install flow entirely — no
/// marker directory is even touched, let alone created.
#[test]
#[ignore = "requires WSL Ubuntu with the musl dv-cli built inside it — see module docs"]
fn e_dv_cli_path_bypasses_install_and_leaves_the_marker_root_untouched() {
    let _lock = test_env_lock();
    let marker_root = format!("/tmp/dv-test-{}-cli-e", std::process::id());
    let dev_path = "/home/kyle/.cache/dv-target/x86_64-unknown-linux-musl/release/dv-cli";
    let _ = wsl(&["rm", "-rf", &marker_root]);

    // SAFETY: test-only env mutation; guarded by test_env_lock above.
    unsafe {
        std::env::remove_var("DV_CLI_SIDECAR");
        std::env::set_var("DV_CLI_PATH", dev_path);
        std::env::set_var("DV_HOST_INSTALL_ROOT", &marker_root);
    }

    let installed =
        install::ensure_cli_installed(DISTRO).expect("ensure_cli_installed with DV_CLI_PATH set");
    assert_eq!(installed.source, HostBinarySource::DevOverride);
    assert_eq!(installed.path, dev_path);

    let test_dir = wsl(&["test", "-d", &marker_root]);
    assert!(
        !test_dir.status.success(),
        "marker root must never have been created: {marker_root}"
    );

    unsafe {
        std::env::remove_var("DV_CLI_PATH");
        std::env::remove_var("DV_HOST_INSTALL_ROOT");
    }
}
