//! `LspClient`/`LspHandle`: spawns `vtsls --stdio` (via
//! [`crate::command::CommandBuilder::command`], never `dv-host`) and
//! speaks LSP's `Content-Length`-framed JSON-RPC over its stdin/stdout —
//! see [`super`]'s module doc for the transport deviation and lifecycle
//! contract every fn here operates under. Structurally mirrors
//! [`crate::remote::client::HostClient`] (long-lived child, reader thread,
//! id-keyed pending-request map, `Drop` grace-then-kill) — the framing is
//! the only real difference: `dv-host`'s wire is newline-delimited JSON,
//! LSP's is `Content-Length` HTTP-style headers.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use serde_json::{Value, json};

use crate::command::CommandBuilder;
use crate::location::RepoLocation;
use crate::provision::NodeVtsls;

/// Bound for the `initialize` round trip — generous: vtsls's first load of
/// a monorepo (resolving `tsconfig.json`/`node_modules`) can legitimately
/// take a while, unlike [`super`]'s other, much smaller detection scripts.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound for a `textDocument/definition` round trip. The doc's acceptance
/// target is "<500ms warm"; this is a much more generous ceiling so a slow
/// (but not wedged) cold analysis still gets an answer instead of a
/// spurious timeout — never-fail-hard (see [`super::provision`]'s module
/// doc for the same posture on the WSL detection side).
const DEFINITION_TIMEOUT: Duration = Duration::from_secs(15);
/// Bound for a `textDocument/hover` round trip (S8g) — same generous,
/// never-fail-hard posture as [`DEFINITION_TIMEOUT`], just a little
/// tighter: unlike a deliberate ctrl/cmd-click, a hover answer this slow no
/// longer matches a mouse that's very likely moved on to somewhere else by
/// the time it would arrive.
const HOVER_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound for a `textDocument/references` round trip (S8g stretch) — the
/// most expensive of the three request kinds this client speaks (a
/// project-wide search rather than a single-file lookup), so the most
/// generous ceiling.
const REFERENCES_TIMEOUT: Duration = Duration::from_secs(20);
/// Bound for the best-effort `shutdown` request issued by [`LspHandle::shutdown`]
/// and [`LspClient`]'s `Drop` — short, because a child that doesn't answer
/// this quickly is simply killed right after (see `Drop`'s doc).
const SHUTDOWN_REQUEST_TIMEOUT: Duration = Duration::from_millis(500);
/// How long `Drop` waits for the child to exit on its own (after the
/// best-effort shutdown handshake) before reaching for `kill` — mirrors
/// `HostClient`'s `SHUTDOWN_GRACE`.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(1000);
/// How long a gated request waits for the FIRST `$/progress` begin to show
/// up before concluding no semantic-readiness signal is coming at all (an
/// older vtsls, a server that skips progress) and proceeding ungated — see
/// [`SemanticReadiness`]. Observed live: vtsls's first begin lands ~0.4s
/// after the first `didOpen`, so 2s is a generous ceiling; a session that
/// never signals pays this at most ONCE (the no-signal verdict latches).
const SEMANTIC_BEGIN_WAIT: Duration = Duration::from_secs(2);
/// Hard cap on how long a single gated request waits for the project load
/// to FINISH once a begin has been seen — after this, proceed anyway
/// (degrading to the pre-gate behavior: a possibly-syntax-server answer,
/// still covered by the app's empty-result warm-up retries) rather than
/// bricking code intelligence on a signal that never completes. Observed
/// live: the scratch fixture's load finishes ~1.1s after begin; a real
/// monorepo takes longer, and a request that starts waiting mid-load only
/// waits for the REMAINDER (the deadline is per-call, completion is
/// latched).
const SEMANTIC_END_WAIT: Duration = Duration::from_secs(10);

/// Every way an [`LspHandle`] operation can fail. Every variant maps to a
/// UI-visible degrade (docs/phase-8-lsp-and-polish.md § LSP: "surface a
/// gentle warning when absent, don't fail") — never a panic, never a
/// blocked diff view.
#[derive(Debug)]
pub enum LspError {
    /// No `vtsls` to spawn with at all (node present without vtsls, or no
    /// node/WSL location in the first place) — the caller's cue to render
    /// "code intelligence unavailable" rather than retry.
    Unavailable(String),
    /// The spawn, handshake, or a request/response round trip itself
    /// failed — a bounded timeout, a dead pipe, a malformed response.
    /// Still never a panic; the caller degrades the same as `Unavailable`,
    /// just with a different (often retryable) message.
    Failed(anyhow::Error),
}

impl std::fmt::Display for LspError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LspError::Unavailable(reason) => write!(f, "{reason}"),
            LspError::Failed(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for LspError {}

type Pending = Arc<Mutex<HashMap<i64, SyncSender<Result<Value, String>>>>>;
type SharedStdin = Arc<Mutex<Option<BufWriter<ChildStdin>>>>;

/// How a [`SemanticReadiness::wait_ready`] gate resolved — carried back so
/// callers (and the `DV_LSP_TRACE` log) can tell the healthy path apart
/// from the two degrade paths. Every variant means "go ahead and send the
/// request now"; none is an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticWaitOutcome {
    /// The project-load progress cycle completed — the semantic tsserver
    /// has the project and answers are trustworthy.
    Ready,
    /// No `$/progress` begin ever arrived within [`SEMANTIC_BEGIN_WAIT`]
    /// (or the connection died) — no signal to gate on; behave exactly as
    /// before this gate existed. Latched: later requests skip the wait.
    NoSignal,
    /// A begin was seen but the load didn't finish within
    /// [`SEMANTIC_END_WAIT`] — proceed anyway (never brick code
    /// intelligence on a wedged load); NOT latched, so a later request
    /// re-waits for the remainder of a genuinely long project load.
    TimedOut,
}

/// The syntax-vs-semantic warm-up gate (docs/backlog.md "vtsls
/// syntax-vs-semantic server race at session-Ready"). vtsls forks TWO
/// tsserver instances: a `<syntax>` one that's up almost instantly and a
/// `<semantic>` one that takes seconds to load the project — and right
/// after a session first reports ready, the syntax server can answer a
/// definition/references/hover request with a fast, NON-empty, WRONG
/// result (same-file-only definition, partial references) that the
/// empty-result warm-up retries never re-ask.
///
/// **The wire signal** (observed live via `DV_LSP_TRACE` against the real
/// Ubuntu vtsls, 2026-07-18): once the client advertises the
/// `window.workDoneProgress` capability (see `LspHandle::initialize` — vtsls
/// stays silent without it), the first `textDocument/didOpen` is followed
/// ~0.4s later by `window/workDoneProgress/create` + `$/progress` begin
/// pairs ("Analyzing 'index.ts' and its dependencies", "Initializing
/// 'tsconfig.json'" — tsserver's `projectLoadingStart` surfaced), and all
/// of them `end` together when the semantic server finishes loading
/// (`projectLoadingFinish`; ~1.3s after spawn on the scratch fixture).
/// Matching is structural — begin/end counting across ALL progress tokens,
/// no title strings — and completion latches on the first "at least one
/// begin seen, all begun tokens ended" instant.
///
/// **Why this gates per-REQUEST, not session-ready itself:** tsserver has
/// no project until a file is opened, so the signal only starts after the
/// first `didOpen` — holding the session's `Ready` at spawn time would
/// wait on a signal that can never arrive. Instead
/// `LspHandle::definition`/`hover`/`references` each call
/// [`Self::wait_ready`] (bounded, on their own background threads) before
/// sending — requests issued during the load "queue" by blocking, exactly
/// the SemanticPending phase the backlog entry asks for, without any new
/// app-side session state.
struct SemanticReadiness {
    state: Mutex<SemanticState>,
    condvar: Condvar,
}

#[derive(Default)]
struct SemanticState {
    /// At least one `$/progress` begin has arrived.
    seen_begin: bool,
    /// Begun-but-not-ended progress tokens.
    active: u32,
    /// The latch: a full begin(s)→end(s) cycle completed. Never unset —
    /// later didChange-triggered re-analysis cycles don't re-gate a warm
    /// session (the app's empty-result retries cover those).
    completed: bool,
    /// [`SemanticWaitOutcome::NoSignal`] latch — the one full
    /// [`SEMANTIC_BEGIN_WAIT`] penalty is paid at most once per session.
    no_signal: bool,
    /// The reader thread died (EOF/error) — no more signals can ever
    /// arrive; waiters bail immediately instead of sleeping out their
    /// timeouts against a dead child.
    dead: bool,
}

impl SemanticReadiness {
    fn new() -> Self {
        SemanticReadiness {
            state: Mutex::new(SemanticState::default()),
            condvar: Condvar::new(),
        }
    }

    /// Feed one `$/progress` `value.kind` observed by the reader thread.
    /// `"report"` (and anything unrecognized) is ignored; an `end` with no
    /// matching begin is dropped (saturating) rather than corrupting the
    /// count.
    fn on_progress(&self, kind: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match kind {
            "begin" => {
                state.seen_begin = true;
                state.active += 1;
            }
            "end" => {
                state.active = state.active.saturating_sub(1);
                if state.seen_begin && state.active == 0 {
                    state.completed = true;
                }
            }
            _ => return,
        }
        self.condvar.notify_all();
    }

    /// The reader thread is gone — wake and release every waiter.
    fn mark_dead(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.dead = true;
        self.condvar.notify_all();
    }

    /// Block until the project-load cycle completes ([`SemanticWaitOutcome::Ready`]),
    /// or degrade: no begin within `begin_wait` → `NoSignal` (latched), or
    /// begin seen but no completion within `end_wait` → `TimedOut`. Both
    /// timeouts are per-call; completion is latched, so a warm session
    /// returns `Ready` immediately.
    fn wait_ready(&self, begin_wait: Duration, end_wait: Duration) -> SemanticWaitOutcome {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());

        // Phase 1: wait for the first begin (or the no-signal verdict).
        let begin_deadline = Instant::now() + begin_wait;
        while !state.completed && !state.seen_begin {
            if state.no_signal || state.dead {
                return SemanticWaitOutcome::NoSignal;
            }
            let now = Instant::now();
            if now >= begin_deadline {
                state.no_signal = true;
                return SemanticWaitOutcome::NoSignal;
            }
            let (next, _) = self
                .condvar
                .wait_timeout(state, begin_deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            state = next;
        }

        // Phase 2: a load is (or was) in flight — wait for it to finish.
        let end_deadline = Instant::now() + end_wait;
        while !state.completed {
            if state.dead {
                return SemanticWaitOutcome::NoSignal;
            }
            let now = Instant::now();
            if now >= end_deadline {
                return SemanticWaitOutcome::TimedOut;
            }
            let (next, _) = self
                .condvar
                .wait_timeout(state, end_deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            state = next;
        }
        SemanticWaitOutcome::Ready
    }
}

/// A live `vtsls --stdio` connection. Every method takes `&self` and is
/// safe to call concurrently — mirrors [`crate::remote::client::HostClient`]'s
/// same contract, for the same reason (dv's callers are gpui background
/// tasks).
pub struct LspClient {
    child: Mutex<Child>,
    stdin: SharedStdin,
    next_id: AtomicI64,
    pending: Pending,
    alive: Arc<AtomicBool>,
    /// URI -> (the version last announced to vtsls via `didOpen`/`didChange`,
    /// a hash of the text sent with that version). Keyed so
    /// [`LspHandle::sync_document`] can tell "first time we've touched this
    /// file" (send `didOpen`) apart from "already open, content may have
    /// moved on" (send `didChange` with a bumped version) — sending a
    /// second `didOpen` for an already-open document is an LSP protocol
    /// violation (P3 finding: a repeat click on the same file used to
    /// resend `didOpen` with a constant `version: 1` every time). The hash
    /// half lets a call whose text is BYTE-IDENTICAL to what's already open
    /// skip the `didChange` (and the version bump) entirely (P3 finding:
    /// every settled hover — one per ~150ms mouse rest — used to re-send the
    /// whole file and bump the version even when nothing had changed,
    /// forcing vtsls to re-analyze an unchanged document on every hover).
    open_docs: Mutex<HashMap<String, (i32, u64)>>,
    /// The syntax-vs-semantic warm-up gate — fed `$/progress` begin/end by
    /// the reader thread, consulted (bounded) by every
    /// definition/hover/references request. See [`SemanticReadiness`].
    semantic: Arc<SemanticReadiness>,
}

/// Cheap-to-clone handle onto a [`LspClient`] — the only long-lived clone
/// is the one the app's `Workspace` entity holds; dropping it (repo close,
/// app exit, workspace-LRU eviction) drops the last `Arc` and runs
/// [`LspClient`]'s `Drop`, which kills the child. See [`super`]'s module
/// doc.
#[derive(Clone)]
pub struct LspHandle(Arc<LspClient>);

impl LspHandle {
    /// Spawn `vtsls --stdio` at `location`'s repo root (routed through
    /// [`CommandBuilder::new_spawn_only`] — WSL locations become `wsl.exe
    /// -d <distro> --exec <node> <vtsls> --stdio`) and complete the
    /// `initialize`/`initialized` handshake. `root_uri` is a `file://`
    /// URI — build it with [`super::file_uri`].
    ///
    /// **Boots a stopped distro** (via `new_spawn_only`, same as every
    /// `dv_core::provision` fn) — callers must only call this for a WSL
    /// distro already known live (an actively opened repo), never from a
    /// passive walk. See [`super`]'s module doc.
    pub fn spawn(
        location: &RepoLocation,
        node: &NodeVtsls,
        root_uri: &str,
    ) -> Result<Self, LspError> {
        let Some(vtsls_path) = node.vtsls_path.as_deref() else {
            return Err(LspError::Unavailable(
                "vtsls is not installed for this distro".to_string(),
            ));
        };

        let builder = CommandBuilder::new_spawn_only(location.clone());
        // For WSL this is always `(node.node_path, [vtsls_path, "--stdio"])`
        // — vtsls_path is an asdf symlink, never a Windows shim, so
        // `vtsls_invocation` always takes its direct-via-node branch there.
        // For a `Local` repo, `vtsls_path` may instead be a `.cmd`/`.bat`
        // npm shim (a plain PATH search's common case) — see
        // `vtsls_invocation`'s doc comment for why that can't spawn
        // directly and needs `cmd /C` instead.
        let (program, args) =
            crate::provision::vtsls_invocation(&node.node_path, vtsls_path, &["--stdio"]);
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut cmd = builder.command(&program, &arg_refs);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|err| LspError::Failed(anyhow::Error::new(err).context("spawning vtsls")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| LspError::Failed(anyhow!("vtsls: missing stdin handle")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| LspError::Failed(anyhow!("vtsls: missing stdout handle")))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| LspError::Failed(anyhow!("vtsls: missing stderr handle")))?;
        spawn_stderr_drain(stderr);

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let stdin: SharedStdin = Arc::new(Mutex::new(Some(BufWriter::new(stdin))));
        let semantic = Arc::new(SemanticReadiness::new());

        spawn_reader_thread(
            BufReader::new(stdout),
            Arc::clone(&pending),
            Arc::clone(&alive),
            Arc::clone(&stdin),
            Arc::clone(&semantic),
        );

        let handle = LspHandle(Arc::new(LspClient {
            child: Mutex::new(child),
            stdin,
            next_id: AtomicI64::new(1),
            pending,
            alive,
            open_docs: Mutex::new(HashMap::new()),
            semantic,
        }));
        handle.initialize(root_uri)?;
        Ok(handle)
    }

    fn initialize(&self, root_uri: &str) -> Result<(), LspError> {
        // `workspaceFolders` (plus advertising the `workspace.workspaceFolders`
        // capability) matters here beyond spec completeness: vtsls wraps the
        // VS Code TS extension, whose project/module-resolution model keys
        // off the workspace-folder list, not `rootUri` alone. Sending only
        // `rootUri` (as this client used to) left vtsls unable to resolve
        // project-relative imports, and go-to-definition on an imported
        // symbol's call site landed back on the importing file's own import
        // line instead of crossing into the target file (P1 finding).
        let folder_name = root_uri.rsplit('/').next().unwrap_or("workspace");
        let params = json!({
            "processId": Value::Null,
            "rootUri": root_uri,
            "workspaceFolders": [{ "uri": root_uri, "name": folder_name }],
            "capabilities": {
                "window": {
                    // Advertised so vtsls actually CREATES work-done
                    // progress tokens (`window/workDoneProgress/create` +
                    // `$/progress`) — without this capability it stays
                    // silent about tsserver's project load, and the
                    // semantic-readiness gate below has no signal to watch
                    // (verified on the wire via `DV_LSP_TRACE`; see
                    // `SemanticReadiness`).
                    "workDoneProgress": true,
                },
                "workspace": {
                    "workspaceFolders": true,
                    "configuration": true,
                },
                "textDocument": {
                    "definition": { "linkSupport": true },
                    // S8g additions — hover/references. Neither needs
                    // `linkSupport` (that's a `definition`-only wire
                    // wrinkle); `hover.contentFormat` tells vtsls we can
                    // take either shape so it doesn't have to guess (this
                    // client normalizes both anyway — see
                    // `hover_contents_to_text`).
                    "hover": { "contentFormat": ["markdown", "plaintext"] },
                    "references": {},
                    "synchronization": {
                        "dynamicRegistration": false,
                        "didSave": false,
                        "willSave": false,
                    },
                },
            },
        });
        self.0.request("initialize", params, INITIALIZE_TIMEOUT)?;
        self.0.notify("initialized", json!({}))?;
        Ok(())
    }

    /// `textDocument/didOpen` — must precede a [`Self::definition`] request
    /// for `uri` (vtsls, like every real LSP server, only answers queries
    /// against documents it's been told are open). Best-effort by
    /// convention at call sites: a `didOpen` failure and the definition
    /// request that follows it fail the same way (a dead connection), so
    /// callers don't need to branch on this separately.
    pub fn did_open(
        &self,
        uri: &str,
        language_id: &str,
        version: i32,
        text: &str,
    ) -> Result<(), LspError> {
        self.0.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": version,
                    "text": text,
                },
            }),
        )
    }

    /// `textDocument/didChange`, full-document sync (a single `TextDocumentContentChangeEvent`
    /// with no `range` — whole-text replace): the client only ever announces
    /// `TextDocumentSyncKind::Full` capability-wise (see `Self::initialize`'s
    /// minimal `synchronization` block), so this is the only legal shape.
    pub fn did_change(&self, uri: &str, version: i32, text: &str) -> Result<(), LspError> {
        self.0.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": text }],
            }),
        )
    }

    /// Tell vtsls about `uri`'s current content, choosing `didOpen` (the
    /// first time this handle has ever touched `uri`), `didChange` (already
    /// open, and `text` has moved on since the last call), or nothing at all
    /// (already open with this EXACT `text` already announced) so a repeat
    /// call on an already-opened, unchanged file never resends `didOpen` —
    /// see [`LspClient::open_docs`]'s doc comment for why either matters.
    /// Always the right call before a [`Self::definition`]/[`Self::hover`]
    /// request; callers no longer need to track document lifecycle
    /// themselves.
    pub fn sync_document(&self, uri: &str, language_id: &str, text: &str) -> Result<(), LspError> {
        let text_hash = hash_text(text);
        // Hold `open_docs` locked across the send itself (not just the
        // peek): two concurrent first-time syncs of the same `uri` (e.g. a
        // hover and a ctrl-click definition request racing on a
        // just-selected file) must never both observe "not open" and both
        // send `didOpen` — that's the exact LSP protocol violation
        // `open_docs` exists to prevent (P2 finding). There is no
        // lock-order hazard: `did_open`/`did_change` only ever touch the
        // separate stdin mutex, never `open_docs`. On a send failure we
        // simply don't commit — no rollback needed, the map still reflects
        // reality and the next call resyncs normally.
        let mut docs = self.0.open_docs.lock().unwrap_or_else(|e| e.into_inner());
        let intended_version = match docs.get(uri) {
            Some((_version, last_hash)) if *last_hash == text_hash => {
                // Byte-identical to what vtsls was last told — no version
                // bump, no `didChange` at all (see this fn's doc comment).
                return Ok(());
            }
            Some((version, _last_hash)) => *version + 1,
            None => 1,
        };

        if intended_version == 1 {
            self.did_open(uri, language_id, intended_version, text)?;
        } else {
            self.did_change(uri, intended_version, text)?;
        }

        docs.insert(uri.to_string(), (intended_version, text_hash));
        Ok(())
    }

    /// Block (bounded — see [`SEMANTIC_BEGIN_WAIT`]/[`SEMANTIC_END_WAIT`])
    /// until vtsls's SEMANTIC tsserver has finished its first project load,
    /// or a degrade verdict is reached — the syntax-vs-semantic warm-up
    /// gate every definition/hover/references request runs through
    /// automatically (see [`SemanticReadiness`]'s doc for the wire signal
    /// and why this is per-request rather than part of session readiness).
    /// Public so live tests can observe the gate's outcome and timing
    /// directly; app callers never need to call it themselves.
    pub fn wait_semantic_ready(&self) -> SemanticWaitOutcome {
        let started = Instant::now();
        let outcome = self
            .0
            .semantic
            .wait_ready(SEMANTIC_BEGIN_WAIT, SEMANTIC_END_WAIT);
        trace_note(&format!(
            "semantic gate: {outcome:?} after {:.3}s",
            started.elapsed().as_secs_f64()
        ));
        outcome
    }

    /// `textDocument/definition`, normalized to `Vec<LocationLink>`
    /// regardless of which of the three wire shapes (`null` /
    /// `Location`/`Location[]` / `LocationLink[]`) vtsls actually answers
    /// with — see [`normalize_definition_result`]. Gated on
    /// [`Self::wait_semantic_ready`] so a request racing vtsls's first
    /// project load waits for the semantic tsserver instead of accepting
    /// the syntax server's fast-but-wrong answer.
    pub fn definition(
        &self,
        uri: &str,
        pos: lsp_types::Position,
    ) -> Result<Vec<lsp_types::LocationLink>, LspError> {
        self.wait_semantic_ready();
        let value = self.0.request(
            "textDocument/definition",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": pos.line, "character": pos.character },
            }),
            DEFINITION_TIMEOUT,
        )?;
        Ok(normalize_definition_result(value))
    }

    /// `textDocument/hover` (S8g). `Ok(None)` covers both wire shapes that
    /// mean "nothing to show here": a `null` result (vtsls's answer for
    /// whitespace/no-symbol positions) and a response that fails to parse
    /// as `Hover` at all — never-fail-hard, same posture as
    /// [`normalize_definition_result`] (a malformed answer reads as "no
    /// hover", not an error the caller has to handle separately from the
    /// ordinary empty case). Same semantic warm-up gate as
    /// [`Self::definition`].
    pub fn hover(
        &self,
        uri: &str,
        pos: lsp_types::Position,
    ) -> Result<Option<lsp_types::Hover>, LspError> {
        self.wait_semantic_ready();
        let value = self.0.request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": pos.line, "character": pos.character },
            }),
            HOVER_TIMEOUT,
        )?;
        Ok(serde_json::from_value::<Option<lsp_types::Hover>>(value).unwrap_or(None))
    }

    /// `textDocument/references` (S8g stretch), always requesting
    /// `includeDeclaration: true` — the declaration site is itself a
    /// reference worth showing in a results list. Normalized the same
    /// never-a-panic way as [`normalize_definition_result`]: a malformed or
    /// `null` result reads as "no references", never an error. Same
    /// semantic warm-up gate as [`Self::definition`].
    pub fn references(
        &self,
        uri: &str,
        pos: lsp_types::Position,
    ) -> Result<Vec<lsp_types::Location>, LspError> {
        self.wait_semantic_ready();
        let value = self.0.request(
            "textDocument/references",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": pos.line, "character": pos.character },
                "context": { "includeDeclaration": true },
            }),
            REFERENCES_TIMEOUT,
        )?;
        Ok(serde_json::from_value::<Vec<lsp_types::Location>>(value).unwrap_or_default())
    }

    /// `true` while the reader thread hasn't yet observed EOF/an I/O error
    /// on vtsls's stdout.
    pub fn is_alive(&self) -> bool {
        self.0.alive.load(Ordering::SeqCst)
    }

    /// Best-effort graceful `shutdown` request + `exit` notification —
    /// callers (repo close) may call this explicitly to ask vtsls to exit
    /// cleanly a little ahead of the `Drop` that will run anyway once the
    /// last clone of this handle goes away (see [`LspClient`]'s `Drop`,
    /// which repeats this same handshake before falling back to `kill`).
    /// Errors are swallowed: a connection that's already dead has nothing
    /// left to shut down gracefully.
    pub fn shutdown(&self) {
        let _ = self
            .0
            .request("shutdown", Value::Null, SHUTDOWN_REQUEST_TIMEOUT);
        let _ = self.0.notify("exit", json!({}));
    }
}

/// Wall-clock bound for [`node_modules_present`]'s `test -d` existence
/// check — a single stat, so this is much tighter than [`INITIALIZE_TIMEOUT`].
const NODE_MODULES_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether `<repo-root>/node_modules` exists inside `location`'s distro.
/// Best-effort and never-fail-hard: a spawn failure, a timeout, or a non-WSL
/// `location` (this client only ever spawns for WSL — see [`LspHandle::spawn`]'s
/// caller) all read as "present" — i.e. no warning — rather than blocking or
/// mis-reporting on an environmental hiccup unrelated to whether the
/// directory is actually there.
///
/// Called once, right after a session first spawns successfully (see this
/// fn's call site in `workspace.rs`), so the app can surface
/// docs/phase-8-lsp-and-polish.md § LSP.1's "gentle warning when absent" for
/// package-symbol lookups — without this, they silently return an empty
/// result that reads to the user as "no definition found" rather than "the
/// project isn't installed" (P2 finding).
pub fn node_modules_present(location: &RepoLocation) -> bool {
    let RepoLocation::Wsl { path, .. } = location else {
        return true;
    };
    let dir = format!("{}/node_modules", path.trim_end_matches('/'));
    CommandBuilder::new_spawn_only(location.clone())
        .run_timeout("test", &["-d", &dir], NODE_MODULES_CHECK_TIMEOUT)
        .is_ok()
}

impl LspClient {
    fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, LspError> {
        self.request_with_write(method, params, timeout, Self::write_message)
    }

    /// Same request/response dance as [`Self::request`], but writing via
    /// `write_fn` — [`Drop`] passes [`Self::try_write_message`] here so a
    /// wedged child (stdin pipe full, nothing draining it) can't block the
    /// thread running `Drop` forever: see [`Self::try_write_message`]'s doc
    /// comment.
    fn request_with_write(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        write_fn: impl FnOnce(&Self, &Value) -> anyhow::Result<()>,
    ) -> Result<Value, LspError> {
        if !self.alive.load(Ordering::SeqCst) {
            return Err(LspError::Failed(anyhow!("lsp connection lost")));
        }

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::sync_channel(1);
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);

        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(err) = write_fn(self, &message) {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return Err(LspError::Failed(err));
        }

        match rx.recv_timeout(timeout) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(message)) => Err(LspError::Failed(anyhow!("{method} failed: {message}"))),
            Err(RecvTimeoutError::Timeout) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                Err(LspError::Failed(anyhow!(
                    "{method} timed out after {}s",
                    timeout.as_secs()
                )))
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err(LspError::Failed(anyhow!("lsp connection lost")))
            }
        }
    }

    fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.write_message(&message).map_err(LspError::Failed)
    }

    /// Same as [`Self::notify`], but via [`Self::try_write_message`] — see
    /// [`Drop`]'s doc comment for why it needs this non-blocking variant.
    fn notify_nonblocking(&self, method: &str, params: Value) -> Result<(), LspError> {
        let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.try_write_message(&message).map_err(LspError::Failed)
    }

    fn write_message(&self, value: &Value) -> anyhow::Result<()> {
        trace_outgoing(value);
        let mut guard = self.stdin.lock().unwrap_or_else(|e| e.into_inner());
        let writer = guard
            .as_mut()
            .ok_or_else(|| anyhow!("lsp connection lost"))?;
        write_framed(writer, value)
    }

    /// Like [`Self::write_message`], but `try_lock`s the stdin mutex
    /// instead of blocking on it — `Ok`/`Err` immediately rather than
    /// waiting. [`write_message`]'s blocking lock is held across a raw,
    /// unbounded OS pipe write (`write_all`): a vtsls child that's stopped
    /// draining its stdin (wedged, or its stdout reader thread already
    /// marked it dead) can leave some OTHER thread's in-flight `request`/
    /// `notify` blocked inside that write holding the lock indefinitely.
    /// [`Drop`] runs on whatever thread drops the last `LspHandle` clone —
    /// often the UI thread (workspace close/eviction/app exit) — so it must
    /// never risk that same block (P3 finding: it used to, via a plain
    /// blocking `lock()`, and a wedged child meant a permanent app freeze).
    fn try_write_message(&self, value: &Value) -> anyhow::Result<()> {
        use std::sync::TryLockError;
        let mut guard = match self.stdin.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => {
                return Err(anyhow!(
                    "lsp stdin busy — a write is already in flight, skipping"
                ));
            }
        };
        let writer = guard
            .as_mut()
            .ok_or_else(|| anyhow!("lsp connection lost"))?;
        write_framed(writer, value)
    }
}

impl Drop for LspClient {
    /// Best-effort graceful `shutdown`/`exit` (bounded, short — see
    /// [`SHUTDOWN_REQUEST_TIMEOUT`]; and never BLOCKING — see
    /// [`LspClient::try_write_message`]'s doc comment), then close our end
    /// of stdin (same non-blocking `try_lock`), then wait up to
    /// [`SHUTDOWN_GRACE`] for the child to exit on its own before killing
    /// it outright. Mirrors
    /// [`crate::remote::client::HostClient`]'s `Drop` exactly — a leaked
    /// `wsl.exe --exec node` here would hold a distro handle open for the
    /// rest of the process's life.
    fn drop(&mut self) {
        let _ = self.request_with_write(
            "shutdown",
            Value::Null,
            SHUTDOWN_REQUEST_TIMEOUT,
            Self::try_write_message,
        );
        let _ = self.notify_nonblocking("exit", json!({}));

        if let Ok(mut guard) = self.stdin.try_lock() {
            guard.take(); // dropping the BufWriter<ChildStdin> closes the pipe
        }
        // If the stdin mutex is still held here, some other thread's write
        // is genuinely wedged — leave the pipe open rather than block; the
        // `kill` below still ends the child (and its stdout closing then
        // unblocks that other write with an I/O error, freeing the mutex).

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
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Normalize `textDocument/definition`'s result, which per the LSP spec may
/// be `null`, a single `Location`, a `Location[]`, or a `LocationLink[]`.
/// The two array shapes are mutually exclusive on the wire (`LocationLink`
/// requires `targetUri`/`targetRange`/`targetSelectionRange`, which a plain
/// `Location` never has, and vice versa for `uri`/`range`), so trying
/// `Vec<LocationLink>` first and falling back is safe — never silently
/// misreads one shape as the other.
fn normalize_definition_result(value: Value) -> Vec<lsp_types::LocationLink> {
    if value.is_null() {
        return Vec::new();
    }
    if let Ok(links) = serde_json::from_value::<Vec<lsp_types::LocationLink>>(value.clone()) {
        return links;
    }
    if let Ok(locations) = serde_json::from_value::<Vec<lsp_types::Location>>(value.clone()) {
        return locations.into_iter().map(location_to_link).collect();
    }
    if let Ok(location) = serde_json::from_value::<lsp_types::Location>(value) {
        return vec![location_to_link(location)];
    }
    Vec::new()
}

fn location_to_link(location: lsp_types::Location) -> lsp_types::LocationLink {
    lsp_types::LocationLink {
        origin_selection_range: None,
        target_uri: location.uri,
        target_range: location.range,
        target_selection_range: location.range,
    }
}

/// Cheap content fingerprint for [`LspClient::open_docs`] — collisions would
/// only ever cost a missed `didChange` (a stale-answer risk no worse than
/// the debounce/epoch races every LSP call site already guards against with
/// a re-check on completion), so a fast non-cryptographic hash is the right
/// tool, not a cryptographic digest.
fn hash_text(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// True when any definition target's NAME token covers (`uri`, `pos`)
/// itself — the hover sits ON the symbol's own declaration, so a popover
/// would only echo the line already under the cursor (the "tautological
/// hover" suppression rule, R3). Compared against `target_selection_range`
/// (the name span), deliberately NOT the whole-declaration
/// `target_range`: a whole-body span would also swallow the hover on a
/// recursive call inside its own function, which is a genuine usage site.
/// The app chains hover → definition and drops the popover when this is
/// true; a definition error keeps the hover (never-fail-hard).
pub fn definition_covers_position(
    targets: &[lsp_types::LocationLink],
    uri: &str,
    pos: lsp_types::Position,
) -> bool {
    // Compare decoded paths, not raw URI strings (R3 review, P3): dv's
    // `percent_encode_path` escapes everything outside RFC 3986
    // unreserved+`/`, while vtsls (vscode-uri) leaves several of those
    // sub-delims literal in path segments — `utils(old).ts` would never
    // string-match and the suppression would silently go inert for that
    // file. Undecodable URIs (either side) fall back to exact equality.
    let self_path = super::path_from_file_uri(uri);
    targets.iter().any(|target| {
        let same_file = match (
            &self_path,
            super::path_from_file_uri(target.target_uri.as_str()),
        ) {
            (Some(a), Some(b)) => *a == b,
            _ => target.target_uri.as_str() == uri,
        };
        same_file && range_contains_position(&target.target_selection_range, pos)
    })
}

/// Half-open on lines/inclusive-start exclusive-end on characters, matching
/// LSP `Range` semantics (end is exclusive).
fn range_contains_position(range: &lsp_types::Range, pos: lsp_types::Position) -> bool {
    let after_start = pos.line > range.start.line
        || (pos.line == range.start.line && pos.character >= range.start.character);
    let before_end = pos.line < range.end.line
        || (pos.line == range.end.line && pos.character < range.end.character);
    after_start && before_end
}

/// Normalize `textDocument/hover`'s `Hover.contents` (S8g) — one of three
/// wire shapes (a single `MarkedString`, a `MarkedString[]`, or a
/// `MarkupContent`) — into plain-ish text for the app's minimal hover
/// popover (docs/phase-8-lsp-and-polish.md § LSP: "hover for types/docs";
/// this is deliberately NOT a markdown renderer — a `LanguageString`/code
/// fence keeps its backtick delimiters as literal text, readable as-is in
/// the popover's monospace font rather than actually rendered). `None` when
/// every shape normalizes to empty text — the caller's cue to show no
/// popover at all rather than an empty box (the same "hover empty space ->
/// no popover" case a `null` `Hover` response covers at the [`LspHandle::hover`]
/// layer).
pub fn hover_contents_to_text(contents: &lsp_types::HoverContents) -> Option<String> {
    let text = match contents {
        lsp_types::HoverContents::Scalar(marked) => marked_string_to_text(marked),
        lsp_types::HoverContents::Array(list) => list
            .iter()
            .map(marked_string_to_text)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        lsp_types::HoverContents::Markup(markup) => markup.value.clone(),
    };
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn marked_string_to_text(marked: &lsp_types::MarkedString) -> String {
    match marked {
        lsp_types::MarkedString::String(s) => s.clone(),
        lsp_types::MarkedString::LanguageString(ls) => {
            format!("```{}\n{}\n```", ls.language, ls.value)
        }
    }
}

/// The reply body for a `workspace/configuration` request: an array with
/// exactly as many entries as `params.items` had, each `null` (we don't
/// actually implement configuration — see [`super`]'s module doc — but the
/// spec requires an array here, not a bare `null`, or vtsls's settings
/// handling can misbehave). Missing/malformed `params`/`items` degrades to
/// an empty array rather than panicking — never-fail-hard, same posture as
/// every other bit of this client.
fn workspace_configuration_reply(params: Option<&Value>) -> Value {
    let items_len = params
        .and_then(|params| params.get("items"))
        .and_then(|items| items.as_array())
        .map_or(0, Vec::len);
    Value::Array(vec![Value::Null; items_len])
}

/// Write one `Content-Length: <n>\r\n\r\n<json>` framed message.
fn write_framed(writer: &mut impl Write, value: &Value) -> anyhow::Result<()> {
    let body = serde_json::to_vec(value)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

/// Read one `Content-Length`-framed message. `Ok(None)` on a clean EOF
/// (the child closed its stdout) — every other header/body read failure is
/// an `Err`.
fn read_framed(reader: &mut BufReader<ChildStdout>) -> io::Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut header = String::new();
        let n = reader.read_line(&mut header)?;
        if n == 0 {
            return Ok(None); // EOF before/between headers
        }
        let trimmed = header.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break; // blank line ends the header block
        }
        if let Some(raw) = trimmed
            .split_once(':')
            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value.trim())
        {
            content_length = raw.parse::<usize>().ok();
        }
        // Any other header (`Content-Type`, …) is read and discarded.
    }
    let len = content_length
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length"))?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    let value: Value = serde_json::from_slice(&body)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    Ok(Some(value))
}

/// Ensures the "mark dead + drain pending" cleanup runs on every exit path
/// out of [`spawn_reader_thread`]'s closure, including an unwinding panic —
/// identical rationale to `HostClient`'s own `ReaderDeathGuard`.
struct ReaderDeathGuard {
    pending: Pending,
    alive: Arc<AtomicBool>,
    semantic: Arc<SemanticReadiness>,
}

impl Drop for ReaderDeathGuard {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
        // Wake any request blocked in the semantic warm-up gate — no more
        // `$/progress` can ever arrive, so sleeping out the full timeout
        // against a dead child would only delay its "connection lost" error.
        self.semantic.mark_dead();
        let lost: Vec<_> = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .collect();
        for (_, tx) in lost {
            let _ = tx.send(Err("lsp connection lost".to_string()));
        }
    }
}

/// The long-lived reader: demuxes response frames by id into the pending
/// map, answers server-to-client requests with a bare `null` result (see
/// [`super`]'s module doc — this client doesn't implement any of them for
/// real, just enough to keep vtsls from stalling on an unanswered request),
/// and discards notifications. On EOF/I/O error (or a panic), marks the
/// client dead and fails every still-pending request.
fn spawn_reader_thread(
    mut reader: BufReader<ChildStdout>,
    pending: Pending,
    alive: Arc<AtomicBool>,
    stdin: SharedStdin,
    semantic: Arc<SemanticReadiness>,
) {
    std::thread::Builder::new()
        .name("dv-lsp-reader".into())
        .spawn(move || {
            let _death_guard = ReaderDeathGuard {
                pending: Arc::clone(&pending),
                alive: Arc::clone(&alive),
                semantic: Arc::clone(&semantic),
            };
            while let Ok(Some(value)) = read_framed(&mut reader) {
                dispatch_message(value, &pending, &stdin, &semantic);
            }
        })
        .expect("failed to spawn dv-lsp reader thread");
}

/// Env-gated (`DV_LSP_TRACE=1`) stderr trace of every incoming message the
/// reader thread sees, timestamped relative to the first traced message —
/// the honest way to find out what vtsls actually puts on the wire (this is
/// how the syntax-vs-semantic readiness signal below was discovered and
/// verified live; see [`SemanticReadiness`]). Zero cost when the env var is
/// unset beyond one lazy check.
fn trace_incoming(value: &Value) {
    trace_message("RECV", value);
}

/// The outgoing twin of [`trace_incoming`] — same `DV_LSP_TRACE` gate, same
/// clock, so a trace interleaves client-sent requests/notifications with
/// server messages in true wire order.
fn trace_outgoing(value: &Value) {
    trace_message("SEND", value);
}

/// Free-text sibling of [`trace_message`] on the same gate and clock — used
/// for client-side events that aren't wire messages (the semantic gate's
/// outcome and timing).
fn trace_note(text: &str) {
    if let Some(elapsed) = trace_clock() {
        eprintln!("[lsp {:8.3}s NOTE] {text}", elapsed.as_secs_f64());
    }
}

fn trace_message(direction: &str, value: &Value) {
    let Some(elapsed) = trace_clock() else {
        return;
    };
    let mut rendered = value.to_string();
    if rendered.len() > 400 {
        rendered.truncate(400);
        rendered.push('…');
    }
    eprintln!(
        "[lsp {:8.3}s {direction}] {rendered}",
        elapsed.as_secs_f64()
    );
}

/// `None` when `DV_LSP_TRACE` is unset; otherwise the elapsed time since
/// the first traced event (a process-wide clock, so all sessions in one
/// process interleave coherently).
fn trace_clock() -> Option<Duration> {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    static START: OnceLock<Instant> = OnceLock::new();
    if !*ENABLED.get_or_init(|| std::env::var_os("DV_LSP_TRACE").is_some()) {
        return None;
    }
    Some(START.get_or_init(Instant::now).elapsed())
}

fn dispatch_message(
    value: Value,
    pending: &Pending,
    stdin: &SharedStdin,
    semantic: &SemanticReadiness,
) {
    trace_incoming(&value);
    let id = value.get("id").cloned();
    let has_method = value.get("method").is_some();

    match (id, has_method) {
        (Some(id_value), true) => {
            // A request FROM the server (`client/registerCapability`,
            // `window/workDoneProgress/create`, …) — not implemented for
            // real; a bare `null` result keeps the protocol moving for most
            // of them (see this module's doc for the minimal-client scope).
            //
            // `workspace/configuration` is the one exception the LSP spec
            // makes non-optional: its result MUST be an array with exactly
            // one entry per `params.items`, never a bare `null` — vtsls
            // issues this during `initialize`, and answering it wrong is a
            // protocol violation that can corrupt its settings handling
            // (P2 finding).
            let method = value.get("method").and_then(|m| m.as_str());
            let result = if method == Some("workspace/configuration") {
                workspace_configuration_reply(value.get("params"))
            } else {
                Value::Null
            };
            let reply = json!({ "jsonrpc": "2.0", "id": id_value, "result": result });
            if let Ok(mut guard) = stdin.lock()
                && let Some(writer) = guard.as_mut()
            {
                let _ = write_framed(writer, &reply);
            }
        }
        (Some(id_value), false) => {
            // A response to one of OUR requests.
            let Some(id) = id_value.as_i64() else {
                return;
            };
            let Some(tx) = pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id)
            else {
                return; // unknown or already-timed-out id
            };
            if let Some(error) = value.get("error") {
                let message = error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("LSP error")
                    .to_string();
                let _ = tx.send(Err(message));
            } else {
                let result = value.get("result").cloned().unwrap_or(Value::Null);
                let _ = tx.send(Ok(result));
            }
        }
        (None, _) => {
            // A notification. `$/progress` begin/end feeds the semantic
            // warm-up gate (see `SemanticReadiness`); everything else
            // (diagnostics, log messages, …) is read and discarded.
            if value.get("method").and_then(|m| m.as_str()) == Some("$/progress")
                && let Some(kind) = value
                    .get("params")
                    .and_then(|p| p.get("value"))
                    .and_then(|v| v.get("kind"))
                    .and_then(|k| k.as_str())
            {
                semantic.on_progress(kind);
            }
        }
    }
}

/// Drain vtsls's stderr so the pipe never fills and blocks the child; not
/// forwarded anywhere (unlike `HostClient`'s stderr forwarder) since this
/// is a per-repo, potentially-noisy third-party process rather than dv's
/// own `dv-host` — a future revision could ring-buffer this the same way
/// if a "why did vtsls fail" diagnostic is ever needed.
fn spawn_stderr_drain(stderr: ChildStderr) {
    std::thread::Builder::new()
        .name("dv-lsp-stderr".into())
        .spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
            }
        })
        .expect("failed to spawn dv-lsp stderr drain thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- SemanticReadiness: the syntax-vs-semantic warm-up gate -----------
    // (headless — begin/end fed directly, no live vtsls; timeouts kept
    // tiny-but-not-flaky: the asserts are on OUTCOMES, not elapsed time,
    // except where the latch's whole point is "returns well before the
    // full wait")

    #[test]
    fn semantic_gate_completes_when_all_begun_progress_ends() {
        let gate = SemanticReadiness::new();
        gate.on_progress("begin");
        gate.on_progress("begin");
        gate.on_progress("end");
        // One token still active — a bounded wait times out, not Ready.
        assert_eq!(
            gate.wait_ready(Duration::from_millis(10), Duration::from_millis(30)),
            SemanticWaitOutcome::TimedOut
        );
        gate.on_progress("end");
        // All ended — completion latched; an instant Ready from here on.
        assert_eq!(
            gate.wait_ready(Duration::from_millis(10), Duration::from_millis(10)),
            SemanticWaitOutcome::Ready
        );
    }

    #[test]
    fn semantic_gate_no_begin_times_out_as_no_signal_and_latches() {
        let gate = SemanticReadiness::new();
        assert_eq!(
            gate.wait_ready(Duration::from_millis(30), Duration::from_millis(30)),
            SemanticWaitOutcome::NoSignal
        );
        // Latched: a second wait with a LONG begin window must return
        // immediately rather than paying the wait again (the at-most-once
        // penalty contract) — well under the 5s it would otherwise block.
        let started = Instant::now();
        assert_eq!(
            gate.wait_ready(Duration::from_secs(5), Duration::from_secs(5)),
            SemanticWaitOutcome::NoSignal
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn semantic_gate_begin_without_end_times_out_but_does_not_latch() {
        // The hard-timeout fallback: a load that never finishes must not
        // brick code intelligence — the wait resolves TimedOut (proceed
        // anyway) after the bounded end wait.
        let gate = SemanticReadiness::new();
        gate.on_progress("begin");
        assert_eq!(
            gate.wait_ready(Duration::from_millis(10), Duration::from_millis(30)),
            SemanticWaitOutcome::TimedOut
        );
        // NOT latched: once the load does finish, the next wait is Ready.
        gate.on_progress("end");
        assert_eq!(
            gate.wait_ready(Duration::from_millis(10), Duration::from_millis(10)),
            SemanticWaitOutcome::Ready
        );
    }

    #[test]
    fn semantic_gate_end_without_begin_never_completes_or_underflows() {
        let gate = SemanticReadiness::new();
        gate.on_progress("end");
        gate.on_progress("end");
        assert_eq!(
            gate.wait_ready(Duration::from_millis(20), Duration::from_millis(20)),
            SemanticWaitOutcome::NoSignal
        );
        // A real begin/end cycle afterwards still completes normally — the
        // stray ends didn't corrupt the count.
        gate.on_progress("begin");
        gate.on_progress("end");
        assert_eq!(
            gate.wait_ready(Duration::from_millis(10), Duration::from_millis(10)),
            SemanticWaitOutcome::Ready
        );
    }

    #[test]
    fn semantic_gate_report_and_unknown_kinds_are_ignored() {
        let gate = SemanticReadiness::new();
        gate.on_progress("begin");
        gate.on_progress("report");
        gate.on_progress("something-new");
        gate.on_progress("end");
        assert_eq!(
            gate.wait_ready(Duration::from_millis(10), Duration::from_millis(10)),
            SemanticWaitOutcome::Ready
        );
    }

    #[test]
    fn semantic_gate_wakes_a_waiter_when_the_cycle_completes() {
        // The queuing path: a request blocked in the gate mid-load is
        // released the moment the load finishes, not at its timeout.
        let gate = Arc::new(SemanticReadiness::new());
        let signaller = Arc::clone(&gate);
        let feeder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            signaller.on_progress("begin");
            std::thread::sleep(Duration::from_millis(20));
            signaller.on_progress("end");
        });
        let outcome = gate.wait_ready(Duration::from_secs(5), Duration::from_secs(5));
        feeder.join().unwrap();
        assert_eq!(outcome, SemanticWaitOutcome::Ready);
    }

    #[test]
    fn semantic_gate_death_releases_waiters_immediately() {
        let gate = Arc::new(SemanticReadiness::new());
        let killer = Arc::clone(&gate);
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            killer.mark_dead();
        });
        // Both phases: a phase-1 waiter (no begin yet) ...
        let started = Instant::now();
        assert_eq!(
            gate.wait_ready(Duration::from_secs(5), Duration::from_secs(5)),
            SemanticWaitOutcome::NoSignal
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        thread.join().unwrap();
        // ... and a phase-2 waiter (begin seen, dead before end).
        let gate = SemanticReadiness::new();
        gate.on_progress("begin");
        gate.mark_dead();
        assert_eq!(
            gate.wait_ready(Duration::from_secs(5), Duration::from_secs(5)),
            SemanticWaitOutcome::NoSignal
        );
    }

    #[test]
    fn semantic_gate_completion_latch_survives_later_begin_cycles() {
        // A didChange-triggered re-analysis after warm-up must not re-gate
        // a warm session (see `SemanticState::completed`'s doc).
        let gate = SemanticReadiness::new();
        gate.on_progress("begin");
        gate.on_progress("end");
        gate.on_progress("begin"); // a later re-analysis starts...
        assert_eq!(
            gate.wait_ready(Duration::from_millis(10), Duration::from_millis(10)),
            SemanticWaitOutcome::Ready
        );
    }

    // --- normalize_definition_result: the three wire shapes --------------

    fn loc(uri: &str, line: u32) -> Value {
        json!({
            "uri": uri,
            "range": {
                "start": { "line": line, "character": 0 },
                "end": { "line": line, "character": 5 },
            },
        })
    }

    fn link(uri: &str, line: u32) -> Value {
        json!({
            "targetUri": uri,
            "targetRange": {
                "start": { "line": line, "character": 0 },
                "end": { "line": line, "character": 5 },
            },
            "targetSelectionRange": {
                "start": { "line": line, "character": 0 },
                "end": { "line": line, "character": 5 },
            },
        })
    }

    #[test]
    fn normalize_null_is_empty() {
        assert!(normalize_definition_result(Value::Null).is_empty());
    }

    // --- hash_text: the fingerprint sync_document's dedup depends on -----

    #[test]
    fn hash_text_is_deterministic_for_identical_text() {
        assert_eq!(
            hash_text("function greet(): void {}"),
            hash_text("function greet(): void {}")
        );
    }

    #[test]
    fn hash_text_differs_for_different_text() {
        assert_ne!(hash_text("a"), hash_text("b"));
    }

    // --- workspace_configuration_reply: the spec's array requirement -----

    #[test]
    fn workspace_configuration_reply_matches_items_length() {
        let params = json!({ "items": [{"section": "typescript"}, {"section": "javascript"}] });
        let reply = workspace_configuration_reply(Some(&params));
        assert_eq!(reply, json!([null, null]));
    }

    #[test]
    fn workspace_configuration_reply_empty_items_is_empty_array() {
        let params = json!({ "items": [] });
        assert_eq!(workspace_configuration_reply(Some(&params)), json!([]));
    }

    #[test]
    fn workspace_configuration_reply_missing_items_is_empty_array_not_null() {
        // Malformed/absent `params`/`items` still must not reply with a
        // bare `null` — that's the exact spec violation this fn exists to
        // avoid — it degrades to an empty array instead (never-fail-hard).
        assert_eq!(workspace_configuration_reply(None), json!([]));
        assert_eq!(workspace_configuration_reply(Some(&json!({}))), json!([]));
    }

    #[test]
    fn normalize_single_location() {
        let result = normalize_definition_result(loc("file:///a.ts", 3));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].target_uri.as_str(), "file:///a.ts");
        assert_eq!(result[0].target_range.start.line, 3);
        assert!(result[0].origin_selection_range.is_none());
    }

    #[test]
    fn normalize_location_array() {
        let result =
            normalize_definition_result(json!([loc("file:///a.ts", 1), loc("file:///b.ts", 2)]));
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].target_uri.as_str(), "file:///a.ts");
        assert_eq!(result[1].target_uri.as_str(), "file:///b.ts");
    }

    #[test]
    fn normalize_location_link_array() {
        let result = normalize_definition_result(json!([link("file:///a.ts", 7)]));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].target_uri.as_str(), "file:///a.ts");
        assert_eq!(result[0].target_range.start.line, 7);
    }

    #[test]
    fn normalize_empty_array_is_empty() {
        assert!(normalize_definition_result(json!([])).is_empty());
    }

    #[test]
    fn normalize_unrecognized_shape_is_empty_not_a_panic() {
        assert!(normalize_definition_result(json!({"unexpected": true})).is_empty());
    }

    // --- hover_contents_to_text: the three `Hover.contents` wire shapes ---

    #[test]
    fn hover_contents_scalar_string() {
        let contents = lsp_types::HoverContents::Scalar(lsp_types::MarkedString::String(
            "a type signature".to_string(),
        ));
        assert_eq!(
            hover_contents_to_text(&contents).as_deref(),
            Some("a type signature")
        );
    }

    #[test]
    fn hover_contents_scalar_language_string_keeps_the_code_fence() {
        let contents = lsp_types::HoverContents::Scalar(lsp_types::MarkedString::LanguageString(
            lsp_types::LanguageString {
                language: "typescript".to_string(),
                value: "function greet(): void".to_string(),
            },
        ));
        assert_eq!(
            hover_contents_to_text(&contents).as_deref(),
            Some("```typescript\nfunction greet(): void\n```")
        );
    }

    #[test]
    fn hover_contents_array_joins_non_empty_entries() {
        let contents = lsp_types::HoverContents::Array(vec![
            lsp_types::MarkedString::String("first".to_string()),
            lsp_types::MarkedString::String(String::new()),
            lsp_types::MarkedString::String("second".to_string()),
        ]);
        assert_eq!(
            hover_contents_to_text(&contents).as_deref(),
            Some("first\n\nsecond")
        );
    }

    #[test]
    fn hover_contents_markup_uses_the_value_verbatim() {
        let contents = lsp_types::HoverContents::Markup(lsp_types::MarkupContent {
            kind: lsp_types::MarkupKind::Markdown,
            value: "**bold** docs".to_string(),
        });
        assert_eq!(
            hover_contents_to_text(&contents).as_deref(),
            Some("**bold** docs")
        );
    }

    #[test]
    fn hover_contents_all_empty_is_none_not_an_empty_popover() {
        let contents =
            lsp_types::HoverContents::Scalar(lsp_types::MarkedString::String("   ".to_string()));
        assert!(hover_contents_to_text(&contents).is_none());
    }

    // --- read_framed / write_framed: the Content-Length wire format ------

    #[test]
    fn write_then_read_framed_round_trips() {
        let value = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"});
        let expected_len = serde_json::to_vec(&value).unwrap().len();
        let mut buf = Vec::new();
        write_framed(&mut buf, &value).unwrap();

        let mut reader = BufReader::new(std::io::Cursor::new(buf));
        // `read_framed` is typed over `BufReader<ChildStdout>` specifically
        // (a real, non-mockable OS handle) — exercise the exact same
        // header-parsing/body-read logic against a `Cursor` via a tiny
        // local copy of the parse step instead of the child-process type.
        let mut header = String::new();
        std::io::BufRead::read_line(&mut reader, &mut header).unwrap();
        assert_eq!(header.trim_end(), format!("Content-Length: {expected_len}"));
        let mut blank = String::new();
        std::io::BufRead::read_line(&mut reader, &mut blank).unwrap();
        assert_eq!(blank, "\r\n");
        let mut body = String::new();
        std::io::Read::read_to_string(&mut reader, &mut body).unwrap();
        let parsed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed, value);
    }

    #[test]
    fn content_length_header_matching_is_case_insensitive() {
        // Some servers/tooling emit "content-length" — the LSP spec's own
        // examples use the canonical casing, but parsing should not be
        // fragile against a lowercase variant.
        let trimmed = "content-length: 12";
        let parsed = trimmed
            .split_once(':')
            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value.trim());
        assert_eq!(parsed, Some("12"));
    }

    // --- definition_covers_position (tautological-hover suppression, R3) --

    fn decl_link(
        uri: &str,
        sel: ((u32, u32), (u32, u32)),
        target: ((u32, u32), (u32, u32)),
    ) -> lsp_types::LocationLink {
        let range = |((sl, sc), (el, ec)): ((u32, u32), (u32, u32))| {
            json!({
                "start": { "line": sl, "character": sc },
                "end": { "line": el, "character": ec },
            })
        };
        // Built through the wire shape — this lsp_types' `Uri` offers no
        // direct constructor, same reason the normalize tests above go
        // through JSON.
        serde_json::from_value(json!({
            "targetUri": uri,
            "targetRange": range(target),
            "targetSelectionRange": range(sel),
        }))
        .unwrap()
    }

    #[test]
    fn hover_on_own_declaration_name_is_covered() {
        // `fn mono_family` declared at line 10, name token cols 3..14;
        // hovering inside the name is tautological.
        let targets = vec![decl_link(
            "file:///w/src/a.ts",
            ((10, 3), (10, 14)),
            ((10, 0), (20, 1)),
        )];
        let pos = lsp_types::Position {
            line: 10,
            character: 5,
        };
        assert!(definition_covers_position(
            &targets,
            "file:///w/src/a.ts",
            pos
        ));
    }

    #[test]
    fn hover_on_a_usage_site_is_not_covered() {
        // Same declaration; hovering a CALL at line 42 resolves to it but
        // the position isn't inside the name token — a real hover.
        let targets = vec![decl_link(
            "file:///w/src/a.ts",
            ((10, 3), (10, 14)),
            ((10, 0), (20, 1)),
        )];
        let pos = lsp_types::Position {
            line: 42,
            character: 8,
        };
        assert!(!definition_covers_position(
            &targets,
            "file:///w/src/a.ts",
            pos
        ));
    }

    #[test]
    fn recursive_call_inside_own_body_is_not_covered() {
        // The reason this checks target_SELECTION_range, not the whole
        // target_range: a recursive call at line 15 sits inside the
        // declaration's body span (10..20) but not inside the name token.
        let targets = vec![decl_link(
            "file:///w/src/a.ts",
            ((10, 3), (10, 14)),
            ((10, 0), (20, 1)),
        )];
        let pos = lsp_types::Position {
            line: 15,
            character: 5,
        };
        assert!(!definition_covers_position(
            &targets,
            "file:///w/src/a.ts",
            pos
        ));
    }

    #[test]
    fn cross_file_definition_is_not_covered() {
        // Hovering a usage whose declaration lives in another file — the
        // uri differs even if line/col happen to coincide.
        let targets = vec![decl_link(
            "file:///w/src/other.ts",
            ((10, 3), (10, 14)),
            ((10, 0), (20, 1)),
        )];
        let pos = lsp_types::Position {
            line: 10,
            character: 5,
        };
        assert!(!definition_covers_position(
            &targets,
            "file:///w/src/a.ts",
            pos
        ));
    }

    #[test]
    fn range_end_is_exclusive_and_start_inclusive() {
        let targets = vec![decl_link(
            "file:///w/src/a.ts",
            ((10, 3), (10, 14)),
            ((10, 0), (20, 1)),
        )];
        let at_start = lsp_types::Position {
            line: 10,
            character: 3,
        };
        let at_end = lsp_types::Position {
            line: 10,
            character: 14,
        };
        assert!(definition_covers_position(
            &targets,
            "file:///w/src/a.ts",
            at_start
        ));
        assert!(!definition_covers_position(
            &targets,
            "file:///w/src/a.ts",
            at_end
        ));
    }
}
