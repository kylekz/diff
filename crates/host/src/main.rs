//! `dv-host`: a headless stdio server that speaks the wire protocol
//! described in `dv_core::remote::proto` (docs/phase-5-implementation-plan.md
//! §2) over stdin/stdout. S1 shipped handshake + `proc/exec`; S2 added
//! `blob/get`; S4 adds `watch/subscribe`|`watch/unsubscribe` (see
//! `watch.rs`) plus the outbound `watch/event` notifications a live
//! subscription pushes. S5 adds `fs/read`|`fs/write_atomic`|`fs/list`|
//! `fs/remove` (see `fs.rs`), replacing `StoreIo`'s WSL `sh -c`/`cat`/`ls`/
//! `rm` fallback path with structured, locale-proof host-side `std::fs`
//! calls whenever a live `fs`-capable connection exists.
//!
//! This binary's OWN wire shapes are still hand-written rather than built
//! on `dv_core::remote::proto` (see that module's doc for why: the
//! cross-process tests in tests/ are what actually hold the two to the
//! same schema, by speaking it over a real pipe, not shared types) — but
//! S4 does add dv-core as a real (non-dev) dependency for `watch.rs`'s
//! reuse of `resolve_local_git_dir` (see that module's doc and this
//! crate's Cargo.toml comment for why that's fine: dv-core has no gpui
//! import, ever, and everything else it pulls in is pure Rust/musl-safe).
//! `fs.rs` goes one step further and reuses dv-core's `read_file_at`/
//! `write_file_atomic_at`/`list_dir_names`/`remove_file_at` helpers
//! directly, not just gitdir resolution — see that module's doc.
//! NO gpui anywhere in this crate, ever — see CLAUDE.md.

mod blob;
mod fs;
mod watch;

use std::io::{BufRead as _, Read as _, Write as _};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};

/// Mirrors `dv_core::remote::proto::PROTO_VERSION`. Kept in sync by hand
/// (this crate doesn't depend on dv-core — see the module doc); the
/// cross-process handshake test is what actually holds the two to the
/// same value, not the type system.
const PROTO_VERSION: u32 = 1;

/// Plan §3: "dispatcher thread + small pool (8 threads)".
const THREAD_POOL_SIZE: usize = 8;

fn main() {
    let stdout = Arc::new(Mutex::new(std::io::stdout()));

    let hello = json!({
        "hello": "dv-host",
        "proto": PROTO_VERSION,
        "version": env!("DV_HOST_VERSION"),
        "pid": std::process::id(),
        // "fs_lock" gates `fs/create_exclusive` SEPARATELY from the general
        // "fs" cap (durable-concurrency slice, docs/backlog.md) so an
        // older already-installed host (this binary, before this method
        // existed) never gets asked for a method it doesn't have — see
        // `dv_core::remote::proto::FsCreateExclusiveParams`'s doc.
        "caps": ["exec", "blob", "watch", "fs", "fs_lock"],
    });
    write_line(&stdout, &hello);

    let (job_tx, job_rx) = mpsc::channel::<Job>();
    let job_rx = Arc::new(Mutex::new(job_rx));
    for _ in 0..THREAD_POOL_SIZE {
        let job_rx = Arc::clone(&job_rx);
        let stdout = Arc::clone(&stdout);
        thread::spawn(move || worker_loop(&job_rx, &stdout));
    }

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => break, // stdin read error: treat like EOF, exit below
        };
        if line.trim().is_empty() {
            continue;
        }
        match parse_request_line(&line) {
            Ok(job) => {
                if job_tx.send(job).is_err() {
                    break; // every worker thread panicked and is gone
                }
            }
            Err((id, message)) => {
                write_line(
                    &stdout,
                    &json!({"id": id, "err": {"code": "bad_request", "message": message}}),
                );
            }
        }
    }
    // stdin EOF (the client dropped its handle, or was dropped itself):
    // exit 0. Worker threads are daemon-like and never joined — process
    // exit reaps them, matching the plan's "host exits on stdin EOF" rule.
}

struct Job {
    id: u64,
    method: String,
    params: Value,
}

/// Parse one request line. On failure, returns `(id, message)` so the
/// caller can still send a correlated error response when the id was
/// recoverable — falling back to id `0` only when it truly wasn't (plan:
/// "Unparseable line → `{"id":0,"err":...}` if no id recoverable, keep
/// serving").
fn parse_request_line(line: &str) -> Result<Job, (u64, String)> {
    let value: Value =
        serde_json::from_str(line).map_err(|err| (0, format!("invalid JSON: {err}")))?;
    let id = value.get("id").and_then(Value::as_u64);
    let method = value.get("method").and_then(Value::as_str);
    match (id, method) {
        (Some(id), Some(method)) => {
            let params = value.get("params").cloned().unwrap_or(Value::Null);
            Ok(Job {
                id,
                method: method.to_string(),
                params,
            })
        }
        (id, _) => Err((id.unwrap_or(0), "request missing id or method".to_string())),
    }
}

fn worker_loop(job_rx: &Arc<Mutex<mpsc::Receiver<Job>>>, stdout: &Arc<Mutex<std::io::Stdout>>) {
    loop {
        // Lock only across the (blocking) recv — processing happens after
        // the guard drops, so other workers can dequeue their next job
        // while this one is mid-request. Standard "Mutex<Receiver>" pool.
        let job = {
            let rx = job_rx.lock().unwrap();
            rx.recv()
        };
        let job = match job {
            Ok(job) => job,
            Err(_) => return, // sender dropped (stdin loop exited) — done
        };
        let response = dispatch(job, stdout);
        write_line(stdout, &response);
    }
}

fn dispatch(job: Job, stdout: &Arc<Mutex<std::io::Stdout>>) -> Value {
    let Job { id, method, params } = job;
    match method.as_str() {
        "proc/exec" => match handle_exec(params) {
            Ok(result) => json!({"id": id, "ok": result}),
            Err(err) => json!({"id": id, "err": {"code": err.code, "message": err.message}}),
        },
        "blob/get" => match handle_blob_get(params) {
            Ok(result) => json!({"id": id, "ok": result}),
            Err(err) => json!({"id": id, "err": {"code": err.code, "message": err.message}}),
        },
        // `watch/subscribe` is the one handler that needs `stdout` itself
        // (not just its own return value): a live subscription pushes
        // `watch/event` notifications asynchronously, from the coalescer's
        // own timer thread, long after this response has already gone out
        // (plan §2: "Notifications write through the same mutex'd stdout as
        // responses").
        "watch/subscribe" => match handle_watch_subscribe(params, stdout) {
            Ok(result) => json!({"id": id, "ok": result}),
            Err(err) => json!({"id": id, "err": {"code": err.code, "message": err.message}}),
        },
        "watch/unsubscribe" => match handle_watch_unsubscribe(params) {
            Ok(result) => json!({"id": id, "ok": result}),
            Err(err) => json!({"id": id, "err": {"code": err.code, "message": err.message}}),
        },
        "fs/read" => match handle_fs_read(params) {
            Ok(result) => json!({"id": id, "ok": result}),
            Err(err) => json!({"id": id, "err": {"code": err.code, "message": err.message}}),
        },
        "fs/write_atomic" => match handle_fs_write_atomic(params) {
            Ok(result) => json!({"id": id, "ok": result}),
            Err(err) => json!({"id": id, "err": {"code": err.code, "message": err.message}}),
        },
        "fs/list" => match handle_fs_list(params) {
            Ok(result) => json!({"id": id, "ok": result}),
            Err(err) => json!({"id": id, "err": {"code": err.code, "message": err.message}}),
        },
        "fs/remove" => match handle_fs_remove(params) {
            Ok(result) => json!({"id": id, "ok": result}),
            Err(err) => json!({"id": id, "err": {"code": err.code, "message": err.message}}),
        },
        "fs/create_exclusive" => match handle_fs_create_exclusive(params) {
            Ok(result) => json!({"id": id, "ok": result}),
            Err(err) => json!({"id": id, "err": {"code": err.code, "message": err.message}}),
        },
        other => {
            json!({"id": id, "err": {"code": "bad_request", "message": format!("unknown method: {other}")}})
        }
    }
}

/// `{code, message}` — mirrors `dv_core::remote::proto::RpcError` minus
/// the exec_code/stderr fields (those only ever appear in a successful
/// `ok` result here; a non-zero exit is data, not a wire error).
struct HostError {
    code: &'static str,
    message: String,
}

impl HostError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            code: "bad_request",
            message: message.into(),
        }
    }

    fn exec_spawn_failed(message: impl Into<String>) -> Self {
        Self {
            code: "exec_spawn_failed",
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            code: "internal",
            message: message.into(),
        }
    }

    /// An OS/filesystem-level failure (plan §2's `io` code) — `watch.rs`'s
    /// gitdir-resolution and watcher-creation failures land here rather
    /// than `internal`, since they're squarely "something about the
    /// filesystem/OS didn't cooperate" rather than a bug in this process.
    fn io(message: impl Into<String>) -> Self {
        Self {
            code: "io",
            message: message.into(),
        }
    }
}

/// `watch/subscribe`.
fn handle_watch_subscribe(
    params: Value,
    stdout: &Arc<Mutex<std::io::Stdout>>,
) -> Result<Value, HostError> {
    let root = params
        .get("root")
        .and_then(Value::as_str)
        .ok_or_else(|| HostError::bad_request("watch/subscribe: missing \"root\""))?;
    let kind = params
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| HostError::bad_request("watch/subscribe: missing \"kind\""))?;

    match watch::subscribe(root, kind, Arc::clone(stdout)) {
        Ok(watch_id) => Ok(json!({"watch_id": watch_id})),
        Err(watch::SubscribeError::BadRequest(message)) => Err(HostError::bad_request(message)),
        Err(watch::SubscribeError::Io(message)) => Err(HostError::io(message)),
    }
}

/// `watch/unsubscribe`. Idempotent — an unknown `watch_id` is not an
/// error (see `watch::unsubscribe`'s doc).
fn handle_watch_unsubscribe(params: Value) -> Result<Value, HostError> {
    let watch_id = params
        .get("watch_id")
        .and_then(Value::as_u64)
        .ok_or_else(|| HostError::bad_request("watch/unsubscribe: missing \"watch_id\""))?;
    watch::unsubscribe(watch_id);
    Ok(json!({}))
}

/// `proc/exec`: run `program(args)` with `stdin_b64` (if present) written
/// to its stdin, base64 both ways. A non-zero exit is still a wire-level
/// `ok` — only a spawn failure is an `err`.
fn handle_exec(params: Value) -> Result<Value, HostError> {
    let program = params
        .get("program")
        .and_then(Value::as_str)
        .ok_or_else(|| HostError::bad_request("proc/exec: missing \"program\""))?;
    let args: Vec<String> = match params.get("args") {
        None => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| HostError::bad_request("proc/exec: \"args\" must be strings"))
            })
            .collect::<Result<_, _>>()?,
        Some(_) => {
            return Err(HostError::bad_request(
                "proc/exec: \"args\" must be an array",
            ));
        }
    };
    let stdin_bytes: Option<Vec<u8>> =
        match params.get("stdin_b64").and_then(Value::as_str) {
            Some(b64) => Some(BASE64.decode(b64).map_err(|err| {
                HostError::bad_request(format!("proc/exec: bad stdin_b64: {err}"))
            })?),
            None => None,
        };

    let mut cmd = Command::new(program);
    cmd.args(&args);
    cmd.stdin(if stdin_bytes.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|err| HostError::exec_spawn_failed(format!("{program}: {err}")))?;

    // Drain stdout+stderr on their own threads BEFORE writing stdin, so a
    // child that echoes large output while still reading its input (e.g.
    // `cat`) can't deadlock us: without this, a full stdout/stderr pipe
    // would block the child mid-read while we're blocked mid-write to its
    // stdin, and neither side would ever unblock the other.
    let stdout_handle = child.stdout.take().expect("piped stdout");
    let stderr_handle = child.stderr.take().expect("piped stderr");
    let stdout_thread = thread::spawn(move || {
        let mut buf = Vec::new();
        let mut handle = stdout_handle;
        let _ = handle.read_to_end(&mut buf);
        buf
    });
    let stderr_thread = thread::spawn(move || {
        let mut buf = Vec::new();
        let mut handle = stderr_handle;
        let _ = handle.read_to_end(&mut buf);
        buf
    });

    if let Some(bytes) = stdin_bytes
        && let Some(mut stdin) = child.stdin.take()
    {
        // A child that exits before reading all of stdin (e.g. `git
        // rev-parse` ignoring stdin entirely) makes this a broken-pipe
        // write error — expected, not a failure of the exec itself.
        //
        // Deliberate drift from Stage-A's `run_with_stdin_spawn`
        // (crates/core/src/command.rs): that path propagates a stdin write
        // failure as ITS OWN `Err` (via `?`, before ever calling
        // `wait_with_output`), which masks whatever the child's real exit
        // code/stderr would have said. Swallowing the write error here and
        // falling through to `child.wait()` below instead is strictly MORE
        // informative — the caller gets the program's actual exit code and
        // stderr rather than an opaque "failed writing to stdin". Not
        // unified with Stage-A because that would change today's
        // `CommandBuilder` error text for local/Spawn-arm callers (plan §8:
        // error strings must stay byte-identical) — left as a known,
        // reviewed gap (plan §8 S2 review finding P3-7).
        let _ = stdin.write_all(&bytes);
    }
    // Ensure stdin is closed (EOF) before waiting, whether or not we wrote
    // to it — `child.stdin` is already `None` in the no-stdin case
    // (`Stdio::null()` was never `.take()`n), so this is a no-op then.
    drop(child.stdin.take());

    let status = child
        .wait()
        .map_err(|err| HostError::internal(format!("waiting for {program}: {err}")))?;
    let stdout = stdout_thread
        .join()
        .map_err(|_| HostError::internal("stdout drain thread panicked"))?;
    let stderr = stderr_thread
        .join()
        .map_err(|_| HostError::internal("stderr drain thread panicked"))?;

    Ok(json!({
        "exit_code": status.code().unwrap_or(-1),
        "stdout_b64": BASE64.encode(&stdout),
        "stderr_b64": BASE64.encode(&stderr),
    }))
}

/// `blob/get`: one `git cat-file --batch` request against `blob::get`'s
/// per-root pool. `found: false` (empty `bytes_b64`) is a normal `ok`
/// result — only a pool/spawn-level failure is a wire `err`.
fn handle_blob_get(params: Value) -> Result<Value, HostError> {
    let root = params
        .get("root")
        .and_then(Value::as_str)
        .ok_or_else(|| HostError::bad_request("blob/get: missing \"root\""))?;
    let spec = params
        .get("spec")
        .and_then(Value::as_str)
        .ok_or_else(|| HostError::bad_request("blob/get: missing \"spec\""))?;

    match blob::get(root, spec) {
        Ok((found, bytes)) => Ok(json!({
            "found": found,
            "bytes_b64": BASE64.encode(&bytes),
        })),
        Err(err) => Err(HostError::internal(format!(
            "blob/get failed for root={root:?} spec={spec:?}: {err:#}"
        ))),
    }
}

/// `fs/read` (S5): `found: false` (empty `bytes_b64`) for a missing file is
/// a normal `ok` result — only a gitdir-resolution/OS-level failure is a
/// wire `err` (`io`, plan §2's code for exactly this kind of failure).
fn handle_fs_read(params: Value) -> Result<Value, HostError> {
    let root = params
        .get("root")
        .and_then(Value::as_str)
        .ok_or_else(|| HostError::bad_request("fs/read: missing \"root\""))?;
    let rel = params
        .get("rel")
        .and_then(Value::as_str)
        .ok_or_else(|| HostError::bad_request("fs/read: missing \"rel\""))?;

    match fs::read(root, rel) {
        Ok((found, bytes)) => Ok(json!({
            "found": found,
            "bytes_b64": BASE64.encode(&bytes),
        })),
        Err(err) => Err(HostError::io(format!(
            "fs/read failed for root={root:?} rel={rel:?}: {err:#}"
        ))),
    }
}

/// `fs/write_atomic` (S5).
fn handle_fs_write_atomic(params: Value) -> Result<Value, HostError> {
    let root = params
        .get("root")
        .and_then(Value::as_str)
        .ok_or_else(|| HostError::bad_request("fs/write_atomic: missing \"root\""))?;
    let rel = params
        .get("rel")
        .and_then(Value::as_str)
        .ok_or_else(|| HostError::bad_request("fs/write_atomic: missing \"rel\""))?;
    let bytes_b64 = params
        .get("bytes_b64")
        .and_then(Value::as_str)
        .ok_or_else(|| HostError::bad_request("fs/write_atomic: missing \"bytes_b64\""))?;
    let bytes = BASE64
        .decode(bytes_b64)
        .map_err(|err| HostError::bad_request(format!("fs/write_atomic: bad bytes_b64: {err}")))?;

    fs::write_atomic(root, rel, &bytes).map_err(|err| {
        HostError::io(format!(
            "fs/write_atomic failed for root={root:?} rel={rel:?}: {err:#}"
        ))
    })?;
    Ok(json!({}))
}

/// `fs/list` (S5). `[]` for a missing directory is a normal `ok` result.
fn handle_fs_list(params: Value) -> Result<Value, HostError> {
    let root = params
        .get("root")
        .and_then(Value::as_str)
        .ok_or_else(|| HostError::bad_request("fs/list: missing \"root\""))?;
    let rel_dir = params
        .get("rel_dir")
        .and_then(Value::as_str)
        .ok_or_else(|| HostError::bad_request("fs/list: missing \"rel_dir\""))?;

    match fs::list(root, rel_dir) {
        Ok(names) => Ok(json!({ "names": names })),
        Err(err) => Err(HostError::io(format!(
            "fs/list failed for root={root:?} rel_dir={rel_dir:?}: {err:#}"
        ))),
    }
}

/// `fs/remove` (S5). Not an error if `rel` is already gone.
fn handle_fs_remove(params: Value) -> Result<Value, HostError> {
    let root = params
        .get("root")
        .and_then(Value::as_str)
        .ok_or_else(|| HostError::bad_request("fs/remove: missing \"root\""))?;
    let rel = params
        .get("rel")
        .and_then(Value::as_str)
        .ok_or_else(|| HostError::bad_request("fs/remove: missing \"rel\""))?;

    fs::remove(root, rel).map_err(|err| {
        HostError::io(format!(
            "fs/remove failed for root={root:?} rel={rel:?}: {err:#}"
        ))
    })?;
    Ok(json!({}))
}

/// `fs/create_exclusive` (durable-concurrency slice, docs/backlog.md
/// review-store-locking item). `created: false` (not an error) when `rel`
/// already exists.
fn handle_fs_create_exclusive(params: Value) -> Result<Value, HostError> {
    let root = params
        .get("root")
        .and_then(Value::as_str)
        .ok_or_else(|| HostError::bad_request("fs/create_exclusive: missing \"root\""))?;
    let rel = params
        .get("rel")
        .and_then(Value::as_str)
        .ok_or_else(|| HostError::bad_request("fs/create_exclusive: missing \"rel\""))?;
    let bytes_b64 = params
        .get("bytes_b64")
        .and_then(Value::as_str)
        .ok_or_else(|| HostError::bad_request("fs/create_exclusive: missing \"bytes_b64\""))?;
    let bytes = BASE64.decode(bytes_b64).map_err(|err| {
        HostError::bad_request(format!("fs/create_exclusive: bad bytes_b64: {err}"))
    })?;

    match fs::create_exclusive(root, rel, &bytes) {
        Ok(created) => Ok(json!({ "created": created })),
        Err(err) => Err(HostError::io(format!(
            "fs/create_exclusive failed for root={root:?} rel={rel:?}: {err:#}"
        ))),
    }
}

fn write_line(stdout: &Arc<Mutex<std::io::Stdout>>, value: &Value) {
    let mut out = stdout.lock().unwrap();
    // A write failure means the client is gone (broken pipe) — exit like
    // stdin EOF rather than looping on a channel nobody will ever drain.
    let ok = serde_json::to_writer(&mut *out, value).is_ok()
        && out.write_all(b"\n").is_ok()
        && out.flush().is_ok();
    if !ok {
        std::process::exit(0);
    }
}
