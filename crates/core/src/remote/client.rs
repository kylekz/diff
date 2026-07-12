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

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::Value;

use crate::command::decode_output;

use super::proto::{
    self, BlobGetParams, BlobGetResult, ExecParams, ExecResult, Hello, Notification, PROTO_VERSION,
    Request, RpcError, RpcResult,
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

type NotificationHandler = Arc<dyn Fn(Notification) + Send + Sync>;

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

/// The classification of a [`HostClient::request`]/[`HostClient::exec`]/
/// [`HostClient::blob_get`] failure. Every `anyhow::Error` those methods
/// return carries one of these as its root cause — retrievable via
/// `err.downcast_ref::<RequestFailure>()`, or [`Self::is_connection_failure`]
/// as the ready-made helper — so callers (`CommandBuilder`,
/// `GitRepo::batch_request`) can act on the KIND of failure instead of
/// string-matching the message (plan §8 S2 review findings P2-1/P2-2).
///
/// Only [`Self::Connection`] means the client CHANNEL ITSELF is no longer
/// usable — that is the one and only condition under which a caller should
/// downgrade a [`super::manager`] registry entry to `Dead` or fall back to
/// the Stage-A `Route::Spawn` path. The other two are both, in their own
/// way, a COMPLETED round trip:
///   - [`Self::Timeout`]: the host is presumably still alive and still
///     working on it — the request just hasn't come back yet. Re-running
///     the same command via Spawn would run it a SECOND time, concurrently
///     with the still-in-flight first attempt (ref-lock contention on a
///     fetch, e.g.) — never safe to do transparently.
///   - [`Self::Rpc`]: the host received the request and answered with a
///     structured error. Routing worked; this is the result, exactly as
///     final as a successful response with a non-zero exit code.
#[derive(Debug, Clone, PartialEq)]
pub enum RequestFailure {
    /// The client channel is dead: already known dead before this request
    /// was even sent, the stdin write itself failed, or the reader
    /// thread's reply channel hung up while a reply was still pending.
    Connection(String),
    /// [`REQUEST_TIMEOUT`] elapsed with no reply to `method`.
    Timeout { method: String, secs: u64 },
    /// The host replied with a structured RPC error.
    Rpc(RpcError),
}

impl RequestFailure {
    fn connection(message: impl Into<String>) -> Self {
        RequestFailure::Connection(message.into())
    }

    /// Whether this is the one kind of failure that means the client
    /// channel itself is unusable — see the type doc for why only this
    /// kind should ever downgrade a host entry to `Dead` or trigger the
    /// transparent `Route::Spawn` fallback.
    pub fn is_connection(&self) -> bool {
        matches!(self, RequestFailure::Connection(_))
    }

    /// Whether this failure was a request timeout — the host-side command
    /// may still be running; callers must never re-run it via a fallback
    /// path.
    pub fn is_timeout(&self) -> bool {
        matches!(self, RequestFailure::Timeout { .. })
    }

    /// The shared classification helper: does `err`'s root cause name a
    /// [`RequestFailure::Connection`]? `false` for a `Timeout`, an `Rpc`
    /// error, or any error that isn't a `RequestFailure` at all (a
    /// serialization/deserialization bug, say) — none of those should ever
    /// downgrade a host entry or trigger a Spawn-arm fallback.
    pub fn is_connection_failure(err: &anyhow::Error) -> bool {
        err.downcast_ref::<RequestFailure>()
            .is_some_and(Self::is_connection)
    }
}

impl std::fmt::Display for RequestFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestFailure::Connection(message) => write!(f, "{message}"),
            RequestFailure::Timeout { method, secs } => {
                write!(f, "dv-host request {method:?} timed out after {secs}s")
            }
            RequestFailure::Rpc(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for RequestFailure {}

/// Whether `err` (as returned by [`HostClient::spawn_wsl`]) is specifically
/// the proto-version-mismatch `bail!` in [`HostClient::spawn_internal`] —
/// the one handshake failure [`crate::remote::manager`]'s `client_for`
/// responds to by forcing a reinstall (plan §2) rather than just cooling
/// down like every other spawn failure. Substring match on the exact
/// message `spawn_internal` bails with, same style as
/// [`crate::review::io`]'s `is_missing_path_error` stderr sniffing — there's
/// no structured error type here because `spawn_wsl`'s return type is a bare
/// `anyhow::Result<Self>` and every other failure inside it (spawn failure,
/// handshake timeout, wsl.exe's own UTF-16 error text) is equally terminal,
/// so a whole enum just for this one case isn't worth the churn.
pub(crate) fn is_proto_mismatch(err: &anyhow::Error) -> bool {
    err.to_string().contains("dv-host proto mismatch")
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
        let notification_handler: Arc<Mutex<NotificationHandler>> = Arc::new(Mutex::new(Arc::new(
            |_notification: Notification| {},
        )
            as NotificationHandler));

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
    ///
    /// Runs on [`spawn_reader_thread`]'s dedicated reader thread — the SAME
    /// thread that demuxes every response and must keep looping to unblock
    /// whichever caller is blocked in [`Self::request`]. It must therefore
    /// never block and never call back into this `HostClient` (a `request`
    /// call from inside the handler would deadlock waiting on the very
    /// thread it's running on). Hand off to a channel/queue if the real
    /// handler needs to do either.
    pub fn set_notification_handler(&self, handler: impl Fn(Notification) + Send + Sync + 'static) {
        *self.notification_handler.lock().unwrap() = Arc::new(handler);
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
    /// (or [`REQUEST_TIMEOUT`] elapses), and return its `ok` payload. An
    /// `err` is a [`RequestFailure`] (via `anyhow`'s blanket conversion, so
    /// it's still a plain `anyhow::Error` to callers, but its root cause is
    /// always downcastable back to one) — see that type's doc for what
    /// each variant means and how callers should react to it.
    pub fn request(&self, method: &str, params: Value) -> Result<Value, RequestFailure> {
        if !self.is_alive() {
            return Err(RequestFailure::connection("host connection lost"));
        }

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::sync_channel(1);
        self.pending.lock().unwrap().insert(id, tx);

        let line = Request::new(id, method, params).to_line();
        let send_result: Result<(), RequestFailure> = (|| {
            let mut guard = self.stdin.lock().unwrap();
            let stdin = guard
                .as_mut()
                .ok_or_else(|| RequestFailure::connection("host connection lost"))?;
            stdin.write_all(line.as_bytes()).map_err(|err| {
                RequestFailure::connection(format!("writing request to dv-host: {err}"))
            })?;
            stdin.write_all(b"\n").map_err(|err| {
                RequestFailure::connection(format!("writing request to dv-host: {err}"))
            })?;
            stdin.flush().map_err(|err| {
                RequestFailure::connection(format!("writing request to dv-host: {err}"))
            })?;
            Ok(())
        })();
        if let Err(err) = send_result {
            self.pending.lock().unwrap().remove(&id);
            return Err(err);
        }

        // Post-insert liveness re-check (plan §8 S2 review finding P3-3): a
        // write CAN succeed into a pipe whose other end (the host's
        // stdout) is already closed without our stdin noticing yet — the
        // reader thread is what actually notices, asynchronously. Without
        // this, that case would block for the FULL `REQUEST_TIMEOUT`
        // waiting on a reply that will never come, instead of failing
        // fast. `rx.try_recv()` first because the reader thread may have
        // ALREADY delivered our answer and then hit EOF in the very next
        // read — a blind "dead therefore fail" here would silently discard
        // a perfectly good, already-buffered reply.
        if !self.is_alive() {
            match rx.try_recv() {
                Ok(RpcResult::Ok(value)) => return Ok(value),
                Ok(RpcResult::Err(err)) => return Err(RequestFailure::Rpc(err)),
                Err(_) => {
                    self.pending.lock().unwrap().remove(&id);
                    return Err(RequestFailure::connection(
                        "host connection lost (after send)",
                    ));
                }
            }
        }

        match rx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(RpcResult::Ok(value)) => Ok(value),
            Ok(RpcResult::Err(err)) => Err(RequestFailure::Rpc(err)),
            Err(RecvTimeoutError::Timeout) => {
                self.pending.lock().unwrap().remove(&id);
                Err(RequestFailure::Timeout {
                    method: method.to_string(),
                    secs: REQUEST_TIMEOUT.as_secs(),
                })
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err(RequestFailure::connection("host connection lost"))
            }
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

    /// Typed `blob/get` wrapper: base64 decode, `found: false` becomes
    /// `Ok(None)` — matching `BlobStore::request`'s existing "missing
    /// object" contract so `GitRepo::batch_request`'s Host arm is a drop-in
    /// replacement for the Spawn arm's `BlobStore` (plan §3).
    pub fn blob_get(&self, root: &str, spec: &str) -> Result<Option<Vec<u8>>> {
        let params = BlobGetParams {
            root: root.to_string(),
            spec: spec.to_string(),
        };
        let value = self.request(
            proto::method::BLOB_GET,
            serde_json::to_value(params).context("serializing blob/get params")?,
        )?;
        let result: BlobGetResult =
            serde_json::from_value(value).context("decoding blob/get result")?;
        if !result.found {
            return Ok(None);
        }
        let bytes = BASE64
            .decode(&result.bytes_b64)
            .context("decoding blob/get bytes_b64")?;
        Ok(Some(bytes))
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

/// Ensures the "mark dead + drain pending" cleanup in
/// [`spawn_reader_thread`] runs on EVERY way the reader thread's closure can
/// exit — not just the two clean `break`s. Rust runs local destructors
/// during a panicking unwind too (e.g. `dispatch_line` hitting a poisoned
/// lock), so a plain `Drop` guard constructed before the read loop covers
/// that path for free; the ORIGINAL inline-after-the-loop code did not —
/// a panic there would skip the cleanup entirely, leaving `alive` stuck at
/// `true` forever (every future [`HostClient::request`] would then block
/// for the full [`REQUEST_TIMEOUT`] instead of failing fast, and whatever
/// was already pending would do the same, since nothing would ever answer
/// or drain it).
struct ReaderDeathGuard {
    pending: Arc<Mutex<HashMap<u64, SyncSender<RpcResult>>>>,
    alive: Arc<AtomicBool>,
}

impl Drop for ReaderDeathGuard {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
        let lost: Vec<_> = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .collect();
        for (_, tx) in lost {
            let _ = tx.send(RpcResult::Err(RpcError::new(
                proto::error_code::INTERNAL,
                "host connection lost",
            )));
        }
    }
}

/// The long-lived reader: demuxes response lines by id into the pending
/// map's per-request channel, and routes id-less lines to the
/// notification handler. On EOF/I/O error (or a panic unwinding out of
/// `dispatch_line` — see [`ReaderDeathGuard`]), fails every still-pending
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
            // Constructed before the read loop so it's in scope for the
            // whole closure body: its `Drop` fires on the clean `break`
            // paths below AND on a panic unwinding through them, which is
            // exactly the "any exit path" coverage inline cleanup code
            // placed after the loop could never give us.
            let _death_guard = ReaderDeathGuard {
                pending: Arc::clone(&pending),
                alive: Arc::clone(&alive),
            };
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
            // `_death_guard` drops here on the clean path — same cleanup
            // code as the panic path, just reached via `Drop` either way.
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
            Ok(notification) => {
                // Clone the `Arc<dyn Fn>` out and drop the lock BEFORE
                // calling it — the handler contract (see
                // `HostClient::set_notification_handler`) forbids it from
                // blocking or calling back into this client, but holding
                // the mutex across the call would make even a well-behaved
                // handler that merely takes a moment stall every future
                // `set_notification_handler` caller too, and a panicking
                // handler would poison the lock for good measure.
                let handler = Arc::clone(&notification_handler.lock().unwrap());
                handler(notification);
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    // --- RequestFailure classification (plan §8 S2 review P2-1/P2-2) ----
    //
    // These construct `RequestFailure` values directly and check
    // classification through `anyhow::Error` the same way real callers
    // (`CommandBuilder::run`, `GitRepo::batch_request`) do — no real
    // `HostClient`/transport needed, since the decision this type exists to
    // make is pure data once the failure has already happened.

    #[test]
    fn connection_is_the_only_kind_classified_as_a_connection_failure() {
        let err: anyhow::Error = RequestFailure::connection("host connection lost").into();
        assert!(RequestFailure::is_connection_failure(&err));
    }

    #[test]
    fn timeout_is_not_a_connection_failure() {
        // The P2-2 regression this guards: a 300s recv timeout means the
        // host-side command is almost certainly still running — it must
        // NOT be classified the same as a dead channel, or
        // `CommandBuilder::run` would transparently re-run it via Spawn
        // concurrently with the still-in-flight original.
        let failure = RequestFailure::Timeout {
            method: "proc/exec".to_string(),
            secs: 300,
        };
        assert!(!failure.is_connection());
        assert!(failure.is_timeout());
        let err: anyhow::Error = failure.into();
        assert!(!RequestFailure::is_connection_failure(&err));
    }

    #[test]
    fn rpc_error_the_host_answered_with_is_not_a_connection_failure() {
        // The P2-1 regression this guards: a structured RpcError from a
        // HEALTHY host (blob/get against a submodule path, a bad_request
        // from a stale binary, ...) must never downgrade the connection —
        // only a channel-level failure may.
        let failure = RequestFailure::Rpc(RpcError::new("internal", "blob/get: not a blob"));
        assert!(!failure.is_connection());
        let err: anyhow::Error = failure.into();
        assert!(!RequestFailure::is_connection_failure(&err));
    }

    #[test]
    fn an_unrelated_error_is_not_classified_as_a_connection_failure() {
        // Anything that isn't a `RequestFailure` at all (a serialization
        // bug, say) must classify as "not connection" too, rather than
        // panicking or false-positiving — `downcast_ref` returning `None`
        // is the expected, safe outcome.
        let err = anyhow::anyhow!("some unrelated error");
        assert!(!RequestFailure::is_connection_failure(&err));
    }

    // --- is_proto_mismatch (plan §8 S3: force-reinstall-once trigger) ----

    #[test]
    fn is_proto_mismatch_matches_the_exact_spawn_internal_bail_text() {
        let err = anyhow::anyhow!("dv-host proto mismatch: host speaks 2, dv expects 1");
        assert!(is_proto_mismatch(&err));
    }

    #[test]
    fn is_proto_mismatch_false_for_unrelated_spawn_failures() {
        assert!(!is_proto_mismatch(&anyhow::anyhow!(
            "failed to spawn dv-host process"
        )));
        assert!(!is_proto_mismatch(&anyhow::anyhow!(
            "dv-host: handshake timed out after 15s"
        )));
    }

    #[test]
    fn request_failure_display_is_readable_for_every_variant() {
        assert_eq!(
            RequestFailure::connection("host connection lost").to_string(),
            "host connection lost"
        );
        assert_eq!(
            RequestFailure::Timeout {
                method: "proc/exec".to_string(),
                secs: 300,
            }
            .to_string(),
            "dv-host request \"proc/exec\" timed out after 300s"
        );
        let rpc = RequestFailure::Rpc(RpcError::new("bad_request", "missing \"program\""));
        assert_eq!(rpc.to_string(), "bad_request: missing \"program\"");
    }
}
