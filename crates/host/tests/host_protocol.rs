//! Exercises the REAL `dv-host` binary (this package's own bin target —
//! `env!("CARGO_BIN_EXE_dv-host")`, built automatically by `cargo test`)
//! through the REAL `dv_core::remote::client::HostClient`, on whatever
//! platform `cargo test` runs on. This is the S1 risk-burner's
//! cross-process proof that client and host agree on the wire shapes
//! byte-for-byte — no WSL required (see tests/wsl_host.rs for the real
//! `wsl.exe` path, which is #[ignore]d).
//!
//! dv-core is a dev-dependency only (see Cargo.toml) — it never reaches
//! the shipped `dv-host` binary, just this test binary.

use std::io::Write as _;
use std::process::Command;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use dv_core::remote::client::HostClient;

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
