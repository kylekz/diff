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
//!   cargo test -p dv-host --test wsl_host -- --ignored --nocapture
//!
//! `DV_HOST_PATH` overrides the default built-binary path below (must be
//! an absolute POSIX path INSIDE the distro).

use std::sync::Arc;
use std::time::{Duration, Instant};

use dv_core::remote::client::HostClient;

const DEFAULT_HOST_PATH: &str = "/home/kyle/.cache/dv-target/debug/dv-host";
const DISTRO: &str = "Ubuntu";

fn host_path() -> String {
    std::env::var("DV_HOST_PATH").unwrap_or_else(|_| DEFAULT_HOST_PATH.to_string())
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
