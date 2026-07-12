//! Wire protocol for the `dv-host` RPC channel — JSON-Lines, UTF-8, one
//! object per line, both directions (docs/phase-5-implementation-plan.md
//! §2). Deliberately dependency-light (serde + serde_json only).
//!
//! `dv-host` itself hand-writes its own copy of these shapes rather than
//! depending on dv-core (see crates/host/src/main.rs — dv-host avoids
//! dv-core's heavier deps for a binary that must compile everywhere), so
//! this module is the single normative source of the schema: the
//! cross-process tests in crates/host/tests/ hold both sides to it by
//! actually speaking it over a real pipe, not by sharing these types.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bumped on any breaking wire change. [`crate::remote::client::HostClient`]
/// kills the child and refuses to proceed if a host reports a different
/// value (plan §2: "proto mismatch: kill, reinstall from sidecar, respawn
/// once; still bad → hard error" — S1 implements the kill+error half only).
pub const PROTO_VERSION: u32 = 1;

/// Method names. S1 shipped `proc/exec`; S2 adds `blob/get`; S4 adds
/// `watch/subscribe`|`watch/unsubscribe` (see [`WATCH_EVENT`] for the
/// id-less notification a live subscription pushes). `fs/*` from plan §2
/// arrives in S5.
pub mod method {
    pub const PROC_EXEC: &str = "proc/exec";
    pub const BLOB_GET: &str = "blob/get";
    pub const WATCH_SUBSCRIBE: &str = "watch/subscribe";
    pub const WATCH_UNSUBSCRIBE: &str = "watch/unsubscribe";
}

/// [`Notification::event`] value for a live `watch/subscribe`'s pushed
/// change events (plan §2/§6) — see [`WatchEventParams`].
pub const WATCH_EVENT: &str = "watch/event";

/// `err.code` values the protocol defines (plan §2). Plain string
/// constants rather than a closed enum: codes travel as JSON strings, and
/// a code from a newer host version that this client doesn't recognize
/// yet must still deserialize — a closed enum would make that a parse
/// error instead of forward-compat data.
pub mod error_code {
    pub const EXEC_SPAWN_FAILED: &str = "exec_spawn_failed";
    pub const NOT_FOUND: &str = "not_found";
    pub const IO: &str = "io";
    pub const BAD_REQUEST: &str = "bad_request";
    pub const INTERNAL: &str = "internal";
}

/// Client -> host. `id` is client-assigned and monotonic; the host echoes
/// it verbatim in its `Response` so replies — which may arrive out of
/// order once methods run concurrently across the host's thread pool —
/// correlate back to whichever blocked caller sent them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl Request {
    pub fn new(id: u64, method: impl Into<String>, params: Value) -> Self {
        Self {
            id,
            method: method.into(),
            params,
        }
    }

    /// Serialize as the bare JSON object — callers append their own `\n`,
    /// keeping this reusable for real writes and test fixtures alike.
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("Request always serializes")
    }
}

/// The `{code, message, exit_code?, stderr?}` error object. `exit_code`/
/// `stderr` are populated only by handlers that ran a process to
/// completion and are reporting details about IT (so the client can
/// rebuild today's `CommandBuilder` error strings byte-for-byte);
/// protocol-level errors (`bad_request`, `internal`, a spawn failure)
/// leave both `None`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
}

impl RpcError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            exit_code: None,
            stderr: None,
        }
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for RpcError {}

/// Host -> client reply. Modeled as an enum (rather than two `Option`
/// fields) so callers can't observe the invalid "both present" or
/// "neither present" states the wire's `RawResponse` shape would
/// otherwise allow — [`parse_response_line`] rejects those up front.
#[derive(Debug, Clone, PartialEq)]
pub enum RpcResult {
    Ok(Value),
    Err(RpcError),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub id: u64,
    pub result: RpcResult,
}

impl Response {
    pub fn ok(id: u64, value: Value) -> Self {
        Self {
            id,
            result: RpcResult::Ok(value),
        }
    }

    pub fn err(id: u64, err: RpcError) -> Self {
        Self {
            id,
            result: RpcResult::Err(err),
        }
    }

    pub fn to_line(&self) -> String {
        let raw = match &self.result {
            RpcResult::Ok(value) => RawResponse {
                id: self.id,
                ok: Some(value.clone()),
                err: None,
            },
            RpcResult::Err(err) => RawResponse {
                id: self.id,
                ok: None,
                err: Some(err.clone()),
            },
        };
        serde_json::to_string(&raw).expect("Response always serializes")
    }
}

/// The actual wire shape. Deriving `Deserialize` on a plain struct (no
/// `deny_unknown_fields`) is what gives forward compat for free: a future
/// host adding a field this client doesn't know about still parses fine,
/// the unknown field just gets dropped.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawResponse {
    id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ok: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    err: Option<RpcError>,
}

/// Parse one line of host output. Malformed JSON, or a line with both/
/// neither of `ok`/`err`, is a clean `Err` — never a panic — so a reader
/// thread can log-and-continue instead of derailing the whole channel.
/// Partial/split lines are the reader's problem (line framing), not this
/// function's: it always receives one already-complete line.
pub fn parse_response_line(line: &str) -> Result<Response> {
    let raw: RawResponse =
        serde_json::from_str(line).with_context(|| format!("invalid response JSON: {line:?}"))?;
    match (raw.ok, raw.err) {
        (Some(ok), None) => Ok(Response::ok(raw.id, ok)),
        (None, Some(err)) => Ok(Response::err(raw.id, err)),
        (None, None) => bail!("response {} has neither ok nor err", raw.id),
        (Some(_), Some(_)) => bail!("response {} has both ok and err", raw.id),
    }
}

/// Host -> client, id-less (`event`/`params` only) — pushed asynchronously,
/// not a reply to any particular request. S1 defines the shape but no
/// host emits any yet (`watch/*` lands in S4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub event: String,
    #[serde(default)]
    pub params: Value,
}

/// First line the host writes on startup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub hello: String,
    pub proto: u32,
    pub version: String,
    pub pid: u32,
    #[serde(default)]
    pub caps: Vec<String>,
}

/// `proc/exec` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecParams {
    pub program: String,
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin_b64: Option<String>,
}

/// `proc/exec` result. A non-zero `exit_code` is still an `ok` result —
/// exit code is data, matching `CommandBuilder::run`'s existing contract;
/// only a spawn failure is a wire-level `err`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecResult {
    pub exit_code: i32,
    pub stdout_b64: String,
    pub stderr_b64: String,
}

/// `blob/get` params — `root` is the absolute in-distro repo root (the
/// same string [`crate::command::CommandBuilder`]'s `-C` argument carries),
/// `spec` is anything `git cat-file --batch` accepts on a line
/// (`<rev>:<path>`, `:0:<path>`, a raw oid, …) — see
/// `crates/core/src/git/batch.rs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlobGetParams {
    pub root: String,
    pub spec: String,
}

/// `blob/get` result. `bytes_b64` is empty when `!found`, mirroring
/// `BlobStore::request`'s `Ok(None)` contract for a missing object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlobGetResult {
    pub found: bool,
    pub bytes_b64: String,
}

/// `watch/subscribe` params (plan §2/§6). `root` is the absolute in-distro
/// path — the repo root for `kind: "worktree"`, or the same root
/// `resolve_local_git_dir` will be run against for `kind: "store"` (the
/// host resolves the real `.git`/`dv/reviews` location itself; the client
/// never has to know it). `kind` is `"store"` or `"worktree"`; an
/// unrecognized value is a `bad_request` error, not a silent no-op.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchSubscribeParams {
    pub root: String,
    pub kind: String,
}

/// `watch/subscribe` result — `watch_id` correlates every future
/// [`WatchEventParams`] notification (and is the handle
/// `watch/unsubscribe` takes back).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchSubscribeResult {
    pub watch_id: u64,
}

/// `watch/unsubscribe` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchUnsubscribeParams {
    pub watch_id: u64,
}

/// Params of a [`Notification`] whose `event` is [`WATCH_EVENT`] — pushed
/// by the host, unprompted, for as long as `watch_id`'s subscription is
/// alive. `paths` and `overflow` are carried for forward-compat/diagnostics
/// but ignored by the v1 client (plan §2: "callback is `Box<dyn Fn()>`") —
/// every event, regardless of contents, means "something changed, go
/// reload"; `overflow: true` (more than 1000 paths coalesced into one
/// event, or the host's own watcher hit an error — see
/// `crates/host/src/watch.rs`'s errors-fire-callback convention) is
/// informational only; it never suppresses the reload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchEventParams {
    pub watch_id: u64,
    pub kind: String,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub overflow: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips() {
        let request = Request::new(7, method::PROC_EXEC, serde_json::json!({"program": "git"}));
        let line = request.to_line();
        let parsed: Request = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed, request);
    }

    #[test]
    fn response_ok_round_trips_and_matches_wire_shape() {
        let response = Response::ok(7, serde_json::json!({"exit_code": 0}));
        let line = response.to_line();
        assert_eq!(line, r#"{"id":7,"ok":{"exit_code":0}}"#);
        let parsed = parse_response_line(&line).unwrap();
        assert_eq!(parsed, response);
    }

    #[test]
    fn response_err_round_trips_and_omits_absent_optional_fields() {
        let err = RpcError::new("exec_spawn_failed", "no such file");
        let response = Response::err(7, err);
        let line = response.to_line();
        let parsed = parse_response_line(&line).unwrap();
        assert_eq!(parsed, response);
        assert!(line.contains(r#""err":{"code":"exec_spawn_failed""#));
        assert!(
            !line.contains("exit_code"),
            "None fields must not appear on the wire: {line}"
        );
        assert!(
            !line.contains("stderr"),
            "None fields must not appear on the wire: {line}"
        );
    }

    #[test]
    fn response_err_carries_exec_fields_when_present() {
        let err = RpcError {
            code: "exec_failed".into(),
            message: "boom".into(),
            exit_code: Some(128),
            stderr: Some("fatal: bad revision".into()),
        };
        let line = Response::err(1, err.clone()).to_line();
        let parsed = parse_response_line(&line).unwrap();
        assert_eq!(parsed, Response::err(1, err));
    }

    /// Forward compat: a future host might add fields this client doesn't
    /// know about yet (a per-method timing field, say) — must still parse.
    #[test]
    fn response_forward_compat_ignores_unknown_fields() {
        let line = r#"{"id":9,"ok":{"exit_code":0},"took_ms":12,"server_note":"hi"}"#;
        let parsed = parse_response_line(line).unwrap();
        assert_eq!(parsed, Response::ok(9, serde_json::json!({"exit_code": 0})));
    }

    #[test]
    fn garbage_line_is_a_clean_error_not_a_panic() {
        assert!(parse_response_line("not json at all").is_err());
        assert!(parse_response_line("").is_err());
        assert!(
            parse_response_line(r#"{"id":1}"#).is_err(),
            "neither ok nor err"
        );
        assert!(
            parse_response_line(r#"{"id":1,"ok":{},"err":{"code":"x","message":"y"}}"#).is_err(),
            "both ok and err"
        );
    }

    #[test]
    fn notification_round_trips() {
        let notification = Notification {
            event: "watch/event".into(),
            params: serde_json::json!({"watch_id": 3}),
        };
        let line = serde_json::to_string(&notification).unwrap();
        let parsed: Notification = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed, notification);
    }

    #[test]
    fn hello_round_trips_and_matches_wire_shape() {
        let hello = Hello {
            hello: "dv-host".into(),
            proto: PROTO_VERSION,
            version: "0.1.0+abc1234".into(),
            pid: 4242,
            caps: vec!["exec".into()],
        };
        let line = serde_json::to_string(&hello).unwrap();
        let parsed: Hello = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed, hello);
        assert!(line.contains(r#""hello":"dv-host""#));
    }

    #[test]
    fn hello_missing_caps_defaults_to_empty() {
        let line = r#"{"hello":"dv-host","proto":1,"version":"0.1.0+dev","pid":1}"#;
        let hello: Hello = serde_json::from_str(line).unwrap();
        assert!(hello.caps.is_empty());
    }

    #[test]
    fn exec_params_and_result_round_trip() {
        let params = ExecParams {
            program: "git".into(),
            args: vec!["--version".into()],
            stdin_b64: Some("aGVsbG8=".into()),
        };
        let line = serde_json::to_string(&params).unwrap();
        let parsed: ExecParams = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed, params);

        let result = ExecResult {
            exit_code: 0,
            stdout_b64: "aGVsbG8=".into(),
            stderr_b64: String::new(),
        };
        let line = serde_json::to_string(&result).unwrap();
        let parsed: ExecResult = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed, result);
    }

    #[test]
    fn exec_params_stdin_b64_omitted_when_none() {
        let params = ExecParams {
            program: "git".into(),
            args: vec![],
            stdin_b64: None,
        };
        let line = serde_json::to_string(&params).unwrap();
        assert!(!line.contains("stdin_b64"));
    }

    #[test]
    fn blob_get_params_and_result_round_trip() {
        let params = BlobGetParams {
            root: "/home/kyle/proj".into(),
            spec: "HEAD:src/main.rs".into(),
        };
        let line = serde_json::to_string(&params).unwrap();
        let parsed: BlobGetParams = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed, params);

        let result = BlobGetResult {
            found: true,
            bytes_b64: "aGVsbG8=".into(),
        };
        let line = serde_json::to_string(&result).unwrap();
        let parsed: BlobGetResult = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed, result);
    }

    #[test]
    fn blob_get_result_missing_has_empty_bytes() {
        let result = BlobGetResult {
            found: false,
            bytes_b64: String::new(),
        };
        let line = serde_json::to_string(&result).unwrap();
        assert!(line.contains(r#""found":false"#));
        assert!(line.contains(r#""bytes_b64":"""#));
    }

    #[test]
    fn watch_subscribe_params_and_result_round_trip() {
        let params = WatchSubscribeParams {
            root: "/home/kyle/proj".into(),
            kind: "store".into(),
        };
        let line = serde_json::to_string(&params).unwrap();
        let parsed: WatchSubscribeParams = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed, params);

        let result = WatchSubscribeResult { watch_id: 42 };
        let line = serde_json::to_string(&result).unwrap();
        assert_eq!(line, r#"{"watch_id":42}"#);
        let parsed: WatchSubscribeResult = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed, result);
    }

    #[test]
    fn watch_unsubscribe_params_round_trip() {
        let params = WatchUnsubscribeParams { watch_id: 7 };
        let line = serde_json::to_string(&params).unwrap();
        assert_eq!(line, r#"{"watch_id":7}"#);
        let parsed: WatchUnsubscribeParams = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed, params);
    }

    #[test]
    fn watch_event_params_round_trip_as_a_notification() {
        let notification = Notification {
            event: WATCH_EVENT.to_string(),
            params: serde_json::to_value(WatchEventParams {
                watch_id: 3,
                kind: "worktree".into(),
                paths: vec!["src/main.rs".into()],
                overflow: false,
            })
            .unwrap(),
        };
        let line = serde_json::to_string(&notification).unwrap();
        let parsed: Notification = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed.event, WATCH_EVENT);
        let params: WatchEventParams = serde_json::from_value(parsed.params).unwrap();
        assert_eq!(params.watch_id, 3);
        assert_eq!(params.kind, "worktree");
        assert_eq!(params.paths, vec!["src/main.rs".to_string()]);
        assert!(!params.overflow);
    }

    #[test]
    fn watch_event_params_default_paths_and_overflow_when_absent() {
        // The store arm's events (and any minimal/older host) may omit
        // these — must still parse rather than error.
        let line = r#"{"watch_id":9,"kind":"store"}"#;
        let params: WatchEventParams = serde_json::from_str(line).unwrap();
        assert!(params.paths.is_empty());
        assert!(!params.overflow);
    }
}
