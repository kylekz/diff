//! `HostClient`: spawns a `dv-host` process (via `wsl.exe`, or any process
//! the test seam hands it) and speaks the [`super::proto`] wire protocol
//! over its stdin/stdout. See docs/phase-5-implementation-plan.md §3-4.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::Value;

use crate::command::decode_output;

use super::proto::{
    self, ExecParams, ExecResult, Hello, Notification, PROTO_VERSION, Request, RpcError, RpcResult,
};

/// Handshake read must complete within this long — covers a cold WSL
/// distro boot, not just process spawn (plan §2).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Per-request timeout. Long, deliberately: some `proc/exec` calls (a big
/// fetch) are legitimately slow, and this is a stand-in for what today's
/// synchronous `wsl.exe` spawns already tolerate (plan §3).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Ring buffer cap for the stderr forwarder (plan §4) — enough for
/// meaningful crash context without unbounded growth over a long-lived host.
const STDERR_RING_CAP: usize = 64 * 1024;

/// How long [`HostClient`]'s `Drop` waits for the host to exit on stdin EOF
/// before reaching for `kill`.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(2000);

type NotificationHandler = Box<dyn Fn(Notification) + Send + Sync>;

/// A live connection to a `dv-host` process. Every method takes `&self`
/// and is safe to call concurrently from multiple threads — dv-core's
/// callers are gpui background-executor tasks, always plural and
/// concurrent (badge walk, staleness, diff loads all run at once).
pub struct HostClient {
    child: Mutex<Child>,
    stdin: Mutex<Option<BufWriter<ChildStdin>>>,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, SyncSender<RpcResult>>>>,
    alive: Arc<AtomicBool>,
    stderr_ring: Arc<Mutex<Vec<u8>>>,
    notification_handler: Arc<Mutex<NotificationHandler>>,
    hello: Hello,
}

/// The typed result of [`HostClient::exec`] — `proc/exec` with the base64
/// stripped away. A non-zero `exit_code` is a normal value, not an error:
/// callers get the same "exit code is data" contract
/// [`crate::command::CommandBuilder::run`] already gives local callers.
#[derive(Debug, Clone)]
pub struct ExecOutcome {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl HostClient {
    /// Spawn `wsl.exe -d <distro> --exec <host_path>` and complete the
    /// handshake. `host_path` is an absolute POSIX path to an
    /// already-installed `dv-host` binary — S1 has no installer; callers
    /// (today, only tests) pass an explicit path. S3 adds `install.rs`.
    pub fn spawn_wsl(distro: &str, host_path: &str) -> Result<Self> {
        let mut cmd = Command::new("wsl.exe");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            cmd.creation_flags(crate::command::CREATE_NO_WINDOW);
        }
        // --exec, never --: same reasoning as CommandBuilder::command — the
        // login-shell form would expand metacharacters in a path we don't
        // fully control the contents of.
        cmd.args(["-d", distro, "--exec", host_path]);
        Self::spawn_internal(cmd, format!("[dv-host {distro}] "))
    }

    /// Spawn any process speaking the protocol — the test seam. Real
    /// `dv-host` binaries speak it; so does a scripted fake in tests.
    pub fn spawn_command(cmd: Command) -> Result<Self> {
        Self::spawn_internal(cmd, "[dv-host] ".to_string())
    }

    fn spawn_internal(mut cmd: Command, stderr_prefix: String) -> Result<Self> {
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn().context("failed to spawn dv-host process")?;
        let stdin = child
            .stdin
            .take()
            .context("dv-host: missing stdin handle")?;
        let stdout = child
            .stdout
            .take()
            .context("dv-host: missing stdout handle")?;
        let stderr = child
            .stderr
            .take()
            .context("dv-host: missing stderr handle")?;

        let stderr_ring = Arc::new(Mutex::new(Vec::new()));
        spawn_stderr_forwarder(stderr, stderr_prefix, Arc::clone(&stderr_ring));

        let reader = BufReader::new(stdout);
        let (reader, hello_line) = match read_hello_line(reader, HANDSHAKE_TIMEOUT) {
            Ok(pair) => pair,
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(err);
            }
        };
        let hello_text = hello_line.trim_end_matches(['\n', '\r']);

        let hello: Hello = match serde_json::from_str(hello_text) {
            Ok(hello) => hello,
            Err(_) => {
                // Not JSON: this is wsl.exe's OWN error output (bad distro
                // name, WSL not installed, …), which rides UTF-16LE — the
                // same hazard `decode_output` exists for at the command
                // layer (crates/core/src/command.rs).
                let decoded = decode_output(hello_line.as_bytes());
                let _ = child.kill();
                let _ = child.wait();
                bail!("wsl.exe: {}", decoded.trim());
            }
        };
        if hello.proto != PROTO_VERSION {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "dv-host proto mismatch: host speaks {}, dv expects {}",
                hello.proto,
                PROTO_VERSION
            );
        }

        let pending: Arc<Mutex<HashMap<u64, SyncSender<RpcResult>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let notification_handler: Arc<Mutex<NotificationHandler>> =
            Arc::new(Mutex::new(Box::new(|_notification: Notification| {})));

        spawn_reader_thread(
            reader,
            Arc::clone(&pending),
            Arc::clone(&alive),
            Arc::clone(&notification_handler),
        );

        Ok(Self {
            child: Mutex::new(child),
            stdin: Mutex::new(Some(BufWriter::new(stdin))),
            next_id: AtomicU64::new(1),
            pending,
            alive,
            stderr_ring,
            notification_handler,
            hello,
        })
    }

    pub fn version(&self) -> &str {
        &self.hello.version
    }

    pub fn caps(&self) -> &[String] {
        &self.hello.caps
    }

    pub fn pid(&self) -> u32 {
        self.hello.pid
    }

    /// `false` once the reader thread has seen EOF/an I/O error on the
    /// host's stdout — a dead client's [`Self::request`] fails fast instead
    /// of blocking on a channel nothing will ever answer.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// A snapshot of the host's stderr output collected so far (lossy
    /// UTF-8; capped at 64KB, oldest bytes dropped first) — diagnostic
    /// context for "the host misbehaved" error reports.
    pub fn stderr_snapshot(&self) -> String {
        let ring = self.stderr_ring.lock().unwrap();
        String::from_utf8_lossy(&ring).into_owned()
    }

    /// Register a callback for id-less (`event`/`params`) lines. Only one
    /// handler at a time; the default (set at spawn) silently drops them.
    /// No host emits any notifications yet (`watch/*` is S4) — the seam
    /// exists now so S4 is a pure addition here, not a signature change.
    pub fn set_notification_handler(&self, handler: impl Fn(Notification) + Send + Sync + 'static) {
        *self.notification_handler.lock().unwrap() = Box::new(handler);
    }

    /// Force-kill the child — the test seam for "host connection lost"
    /// scenarios. The process itself is not otherwise supervised in S1
    /// (no crash-respawn until S2's `HostManager`).
    pub fn kill(&self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }

    /// Send `method`/`params`, block until the matching response arrives
    /// (or [`REQUEST_TIMEOUT`] elapses), and return its `ok` payload — an
    /// `err` becomes an `Err` via [`RpcError`]'s `Display`.
    pub fn request(&self, method: &str, params: Value) -> Result<Value> {
        if !self.is_alive() {
            bail!("host connection lost");
        }

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::sync_channel(1);
        self.pending.lock().unwrap().insert(id, tx);

        let line = Request::new(id, method, params).to_line();
        let send_result: Result<()> = (|| {
            let mut guard = self.stdin.lock().unwrap();
            let stdin = guard
                .as_mut()
                .ok_or_else(|| anyhow!("host connection lost"))?;
            stdin.write_all(line.as_bytes())?;
            stdin.write_all(b"\n")?;
            stdin.flush()?;
            Ok(())
        })();
        if let Err(err) = send_result {
            self.pending.lock().unwrap().remove(&id);
            return Err(err.context("writing request to dv-host"));
        }

        match rx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(RpcResult::Ok(value)) => Ok(value),
            Ok(RpcResult::Err(err)) => Err(anyhow!(err)),
            Err(RecvTimeoutError::Timeout) => {
                self.pending.lock().unwrap().remove(&id);
                bail!(
                    "dv-host request {method:?} timed out after {}s",
                    REQUEST_TIMEOUT.as_secs()
                );
            }
            Err(RecvTimeoutError::Disconnected) => bail!("host connection lost"),
        }
    }

    /// Typed `proc/exec` wrapper: base64 both ways, exit-code-is-data
    /// preserved.
    pub fn exec(&self, program: &str, args: &[&str], stdin: Option<&[u8]>) -> Result<ExecOutcome> {
        let params = ExecParams {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            stdin_b64: stdin.map(|bytes| BASE64.encode(bytes)),
        };
        let value = self.request(
            proto::method::PROC_EXEC,
            serde_json::to_value(params).context("serializing proc/exec params")?,
        )?;
        let result: ExecResult =
            serde_json::from_value(value).context("decoding proc/exec result")?;
        Ok(ExecOutcome {
            exit_code: result.exit_code,
            stdout: BASE64
                .decode(&result.stdout_b64)
                .context("decoding proc/exec stdout_b64")?,
            stderr: BASE64
                .decode(&result.stderr_b64)
                .context("decoding proc/exec stderr_b64")?,
        })
    }
}

impl Drop for HostClient {
    fn drop(&mut self) {
        // Close our end of stdin first: EOF is the host's signal to end its
        // read loop and exit on its own (crates/host/src/main.rs) — much
        // cleaner than killing every host on every dv exit/repo close.
        if let Ok(mut guard) = self.stdin.lock() {
            guard.take(); // dropping the BufWriter<ChildStdin> closes the pipe
        }

        let Ok(mut child) = self.child.lock() else {
            return;
        };
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                _ => break,
            }
        }
        // Overran the grace period (or try_wait itself errored) — don't let
        // dv's shutdown hang on a wedged host.
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Read the handshake line under a hard timeout. Implemented as a helper
/// thread rather than a poll loop: `BufRead::read_line` has no deadline
/// parameter, and a cold WSL distro boot can legitimately take several
/// seconds, so busy-polling would either spin or need its own coarse
/// granularity that adds latency to the common (already-booted) case. On
/// timeout we deliberately do NOT wait for the helper thread to finish —
/// the caller kills the child right after, which unblocks the pending
/// read (EOF) and lets the thread exit on its own; its (now unwanted)
/// result is silently dropped since `rx` is gone by then.
fn read_hello_line(
    mut reader: BufReader<ChildStdout>,
    timeout: Duration,
) -> Result<(BufReader<ChildStdout>, String)> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("dv-host-handshake".into())
        .spawn(move || {
            let mut line = String::new();
            let outcome = reader.read_line(&mut line);
            let _ = tx.send(outcome.map(|n| (reader, line, n)));
        })
        .expect("failed to spawn dv-host handshake thread");

    match rx.recv_timeout(timeout) {
        Ok(Ok((reader, line, n))) if n > 0 => Ok((reader, line)),
        Ok(Ok(_)) => bail!("dv-host: connection closed before sending a handshake line"),
        Ok(Err(err)) => Err(err).context("reading dv-host handshake line"),
        Err(RecvTimeoutError::Timeout) => {
            bail!("dv-host: handshake timed out after {}s", timeout.as_secs())
        }
        Err(RecvTimeoutError::Disconnected) => {
            bail!("dv-host: handshake reader thread died unexpectedly")
        }
    }
}

/// The long-lived reader: demuxes response lines by id into the pending
/// map's per-request channel, and routes id-less lines to the
/// notification handler. On EOF/I/O error, fails every still-pending
/// request and marks the client dead so subsequent [`HostClient::request`]
/// calls fail fast instead of blocking.
fn spawn_reader_thread(
    mut reader: BufReader<ChildStdout>,
    pending: Arc<Mutex<HashMap<u64, SyncSender<RpcResult>>>>,
    alive: Arc<AtomicBool>,
    notification_handler: Arc<Mutex<NotificationHandler>>,
) {
    std::thread::Builder::new()
        .name("dv-host-reader".into())
        .spawn(move || {
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        let trimmed = line.trim_end_matches(['\n', '\r']);
                        if !trimmed.is_empty() {
                            dispatch_line(trimmed, &pending, &notification_handler);
                        }
                    }
                    Err(_) => break,
                }
            }
            alive.store(false, Ordering::SeqCst);
            let lost: Vec<_> = pending.lock().unwrap().drain().collect();
            for (_, tx) in lost {
                let _ = tx.send(RpcResult::Err(RpcError::new(
                    proto::error_code::INTERNAL,
                    "host connection lost",
                )));
            }
        })
        .expect("failed to spawn dv-host reader thread");
}

fn dispatch_line(
    line: &str,
    pending: &Arc<Mutex<HashMap<u64, SyncSender<RpcResult>>>>,
    notification_handler: &Arc<Mutex<NotificationHandler>>,
) {
    let value: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("[dv-host client] malformed line from host, skipping: {err} ({line:?})");
            return;
        }
    };
    if value.get("id").is_none() {
        match serde_json::from_value::<Notification>(value) {
            Ok(notification) => (notification_handler.lock().unwrap())(notification),
            Err(err) => eprintln!("[dv-host client] malformed notification, skipping: {err}"),
        }
        return;
    }
    match proto::parse_response_line(line) {
        Ok(response) => match pending.lock().unwrap().remove(&response.id) {
            Some(tx) => {
                let _ = tx.send(response.result);
            }
            None => eprintln!(
                "[dv-host client] response for unknown or already-timed-out request id {}",
                response.id
            ),
        },
        Err(err) => eprintln!("[dv-host client] malformed response, skipping: {err}"),
    }
}

/// Forward the host's stderr to our own (prefixed, so multi-host output is
/// distinguishable) and keep the last [`STDERR_RING_CAP`] bytes for
/// [`HostClient::stderr_snapshot`].
fn spawn_stderr_forwarder(
    stderr: std::process::ChildStderr,
    prefix: String,
    ring: Arc<Mutex<Vec<u8>>>,
) {
    std::thread::Builder::new()
        .name("dv-host-stderr".into())
        .spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        eprint!("{prefix}{line}");
                        let mut ring = ring.lock().unwrap();
                        ring.extend_from_slice(line.as_bytes());
                        if ring.len() > STDERR_RING_CAP {
                            let overflow = ring.len() - STDERR_RING_CAP;
                            ring.drain(0..overflow);
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .expect("failed to spawn dv-host stderr forwarder thread");
}
