//! `dv-host`: a headless stdio server that speaks the wire protocol
//! described in `dv_core::remote::proto` (docs/phase-5-implementation-plan.md
//! §2) over stdin/stdout. S1 scope: handshake + `proc/exec` only — no
//! git/review handlers, no watching (§8 S1).
//!
//! This binary deliberately does NOT depend on dv-core (see Cargo.toml):
//! it hand-writes the same JSON shapes proto.rs defines, verified by the
//! cross-process tests in tests/ that speak the REAL
//! `dv_core::remote::client::HostClient` against this REAL binary. NO
//! gpui anywhere in this crate, ever — see CLAUDE.md.

mod blob;

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
        "caps": ["exec", "blob"],
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
        let response = dispatch(job);
        write_line(stdout, &response);
    }
}

fn dispatch(job: Job) -> Value {
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
