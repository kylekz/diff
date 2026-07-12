//! Watch manager for `dv-host` (docs/phase-5-implementation-plan.md §2/§6):
//! turns `notify` (real inotify inside the distro) into
//! `watch/subscribe`|`watch/unsubscribe` request handling plus outbound
//! `watch/event` notifications.
//!
//! Every subscription's OS watcher is kept alive in [`REGISTRY`] for as
//! long as the subscription lives — `notify::RecommendedWatcher` stops
//! watching the instant it's dropped, so `unsubscribe` removing it there
//! is what actually stops the watch. There is no separate per-connection
//! teardown: this binary serves exactly one client for its whole lifetime
//! (plan §4 — "one host per distro per dv process"), so "all watchers drop
//! on client disconnect" falls out for free from `main()` returning (and
//! so the process exiting) on stdin EOF.
//!
//! `main.rs` deliberately does NOT depend on dv-core for its OWN wire
//! shapes (see that module's doc), but this module DOES reuse
//! `dv_core::review::resolve_local_git_dir` for gitdir resolution — the
//! host runs local to the files it's serving, so that's the correct place
//! to share code rather than re-implement the `.git`-vs-gitlink-file
//! distinction a second time (plan §1: dv-core was always an allowed
//! dv-host dependency, and it has no gpui import to worry about pulling in
//! — see CLAUDE.md and this crate's Cargo.toml comment).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use notify::event::{AccessKind, AccessMode};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use serde_json::json;

/// Plan §2/§6: "200ms host-side coalesce" — collect paths for this long
/// after the first event in a burst, then emit one `watch/event`. Read
/// fresh via [`coalesce_window`] rather than used directly, so a test can
/// widen it (`DV_HOST_WATCH_COALESCE_MS`) — real sequential filesystem
/// writes (even 1000+ of them) routinely take well over 200ms wall clock
/// on a loaded CI runner or a slower filesystem, which would otherwise
/// make `MAX_COALESCED_PATHS` overflow un-reproducible in a test without
/// either an unrealistically tight write loop or real parallel write
/// pressure; same "make it configurable via env for a test hook" posture
/// as `remote::manager`'s `DV_HOST_COOLDOWN_MS`.
const COALESCE_WINDOW: Duration = Duration::from_millis(200);
/// Plan §2: "overflow flag when >1000 paths coalesce".
const MAX_COALESCED_PATHS: usize = 1000;

static NEXT_WATCH_ID: AtomicU64 = AtomicU64::new(1);

/// A subscribe-time failure, distinguished only enough for `main.rs`'s
/// dispatch to pick the right wire error code (`bad_request` for a
/// malformed/unrecognized request, `io` for an OS/filesystem-level
/// failure — plan §2's `err.code` set).
pub enum SubscribeError {
    BadRequest(String),
    Io(String),
}

impl std::fmt::Display for SubscribeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubscribeError::BadRequest(message) | SubscribeError::Io(message) => {
                write!(f, "{message}")
            }
        }
    }
}

struct Subscription {
    /// Held only for its `Drop` — dropping it is what actually stops the
    /// OS-level watch; nothing else in this struct is ever read.
    _watcher: RecommendedWatcher,
}

fn registry() -> &'static Mutex<HashMap<u64, Subscription>> {
    static REGISTRY: OnceLock<Mutex<HashMap<u64, Subscription>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// [`COALESCE_WINDOW`], or `DV_HOST_WATCH_COALESCE_MS` when set — read
/// fresh (never cached) so a test can flip it for a process that's already
/// running.
fn coalesce_window() -> Duration {
    coalesce_window_given(std::env::var("DV_HOST_WATCH_COALESCE_MS").ok().as_deref())
}

/// Pure core of [`coalesce_window`] — factored out so the env-var/default
/// matrix is unit-testable without touching real process-global env state,
/// same pattern as `remote::manager`'s `hosts_enabled_given`/`host_path_given`.
fn coalesce_window_given(env_val: Option<&str>) -> Duration {
    env_val
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(COALESCE_WINDOW)
}

/// `watch/subscribe`: start watching `root` (an absolute in-distro path)
/// for `kind` (`"store"` or `"worktree"`), returning the assigned
/// `watch_id`. `stdout` is threaded through so the coalescer can push
/// `watch/event` notifications through the SAME mutex'd writer responses
/// use (plan §2: "Notifications write through the same mutex'd stdout as
/// responses").
pub fn subscribe(
    root: &str,
    kind: &str,
    stdout: Arc<Mutex<std::io::Stdout>>,
) -> Result<u64, SubscribeError> {
    let watch_id = NEXT_WATCH_ID.fetch_add(1, Ordering::SeqCst);
    let watcher = match kind {
        "store" => subscribe_store(root, watch_id, stdout)?,
        "worktree" => subscribe_worktree(root, watch_id, stdout)?,
        other => {
            return Err(SubscribeError::BadRequest(format!(
                "watch/subscribe: unknown kind {other:?} (expected \"store\" or \"worktree\")"
            )));
        }
    };
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(watch_id, Subscription { _watcher: watcher });
    Ok(watch_id)
}

/// `watch/unsubscribe`: drop `watch_id`'s OS watcher (idempotent — an
/// already-unknown id is simply a no-op, not an error, matching how
/// `dv_core::review::ReviewWatcher::Remote`'s `Drop` treats it as
/// best-effort).
pub fn unsubscribe(watch_id: u64) {
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&watch_id);
}

/// Whether an inotify event kind is worth forwarding to a coalescer at all
/// (review finding P1-2). `notify`'s inotify backend's default mask
/// includes `IN_ACCESS`/`IN_OPEN`, so a plain, read-only open of a file
/// fires an `Access` event too — and the refresh cycle THIS watch triggers
/// (`refresh_stale`'s `git hash-object`, `changed_files`, the review
/// store's own reload reading its JSON files back) itself reads worktree
/// files, which fires more `Access` events, which triggers another
/// refresh: a self-sustaining reload loop that never goes idle, with a
/// `git`/read subprocess alive continuously even on an otherwise-idle GUI.
///
/// Every REAL write still arrives as `Modify` (or `Create`/`Remove`)
/// regardless, so dropping `Access` loses nothing — except the one
/// sub-case that itself signals a write just completed:
/// `Access(Close(Write))` (`IN_CLOSE_WRITE`), which is kept so a save that
/// (on some editors/filesystems) surfaces only as open→write→close-write
/// isn't missed.
fn should_forward(kind: &EventKind) -> bool {
    match kind {
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        EventKind::Access(_) => false,
        _ => true,
    }
}

/// Run one `notify` callback invocation, catching a panic rather than
/// letting it unwind into `notify`'s own event-loop thread (review finding
/// P3-1). Each subscription's `RecommendedWatcher` owns its own dedicated
/// OS thread today (`notify`'s inotify backend spawns one per watcher
/// instance), so a panic here would otherwise silently kill delivery for
/// every *future* event on this same watch — no crash, no visible error,
/// just a subscription that quietly stops updating. Wrapping per callback
/// invocation (rather than, say, once outside the whole closure) also
/// means a single malformed/unexpected event can't take down the ones
/// after it in the same batch.
fn run_watch_callback(watch_id: u64, f: impl FnOnce()) {
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic payload".to_string());
        eprintln!("[dv-host watch] callback panicked (watch_id={watch_id}): {message}");
    }
}

/// Non-recursive watch on `<gitdir>/dv/reviews` — mirrors
/// `dv_core::review::watch`'s local arm exactly, including
/// create-the-directory-first and the errors-fire-callback convention (a
/// watcher error means the OS may have DROPPED events, so treating it as
/// "something changed, reload" is the safe direction — see that module's
/// doc comment).
fn subscribe_store(
    root: &str,
    watch_id: u64,
    stdout: Arc<Mutex<std::io::Stdout>>,
) -> Result<RecommendedWatcher, SubscribeError> {
    let gitdir = dv_core::review::resolve_local_git_dir(Path::new(root))
        .map_err(|err| SubscribeError::Io(format!("resolving git dir for {root:?}: {err:#}")))?;
    let reviews_dir = gitdir.join("dv").join("reviews");
    std::fs::create_dir_all(&reviews_dir)
        .map_err(|err| SubscribeError::Io(format!("creating {}: {err}", reviews_dir.display())))?;

    let coalescer = Coalescer::new(watch_id, "store", stdout);
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        // A panic inside the callback body must not silently kill this
        // watch's delivery thread forever (review finding P3-1) — see
        // `run_watch_callback`'s doc comment.
        run_watch_callback(watch_id, || match event {
            Ok(event) => {
                if !should_forward(&event.kind) {
                    // A read-only Access event (review finding P1-2) —
                    // see `should_forward`'s doc comment for why
                    // forwarding these creates a self-sustaining reload
                    // loop.
                    return;
                }
                let paths: Vec<String> = event
                    .paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect();
                if !paths.is_empty() {
                    coalescer.push_paths(paths);
                }
            }
            Err(err) => {
                eprintln!("[dv-host watch] store watch error (watch_id={watch_id}): {err}");
                coalescer.push_overflow();
            }
        });
    })
    .map_err(|err| SubscribeError::Io(format!("creating watcher: {err}")))?;
    watcher
        .watch(&reviews_dir, RecursiveMode::NonRecursive)
        .map_err(|err| SubscribeError::Io(format!("watching {}: {err}", reviews_dir.display())))?;
    Ok(watcher)
}

/// Recursive watch on the whole repo root, filtering out anything under
/// `.git` — both the naive `<root>/.git` (the common case, checkable with
/// no I/O) AND, if it resolves to something else, the REAL gitdir a linked
/// worktree's `.git` gitlink file points at (plan §6: "gitdir may be
/// inside root (.git/) or elsewhere for worktrees — filter both").
fn subscribe_worktree(
    root: &str,
    watch_id: u64,
    stdout: Arc<Mutex<std::io::Stdout>>,
) -> Result<RecommendedWatcher, SubscribeError> {
    let root_path = PathBuf::from(root);
    // Canonicalize the filter prefixes: notify's backends report CANONICAL
    // event paths on some platforms (macOS FSEvents resolves /var ->
    // /private/var), so a symlinked root would make every starts_with
    // filter silently miss and .git churn would leak through as worktree
    // events (caught by the macOS CI run of
    // watch_worktree_ignores_dot_git_but_sees_tracked_file_edits).
    let root_path = root_path.canonicalize().unwrap_or(root_path);
    let dot_git = root_path.join(".git");
    // Best-effort: if this repo is somehow unresolvable (shouldn't happen —
    // `root` is a repo dv-core's own GitRepo::open already validated
    // earlier in the session), fall back to just the naive `.git` filter
    // rather than failing the whole worktree subscribe over it.
    let real_gitdir = dv_core::review::resolve_local_git_dir(&root_path)
        .ok()
        .map(|g| g.canonicalize().unwrap_or(g));

    let coalescer = Coalescer::new(watch_id, "worktree", stdout);
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        // See `run_watch_callback`'s doc comment (review finding P3-1).
        run_watch_callback(watch_id, || match event {
            Ok(event) => {
                if !should_forward(&event.kind) {
                    // A read-only Access event (review finding P1-2) —
                    // see `should_forward`'s doc comment for why
                    // forwarding these creates a self-sustaining reload
                    // loop.
                    return;
                }
                let paths: Vec<String> = event
                    .paths
                    .iter()
                    .filter(|p| {
                        !p.starts_with(&dot_git)
                            && real_gitdir.as_ref().is_none_or(|g| !p.starts_with(g))
                    })
                    .map(|p| p.display().to_string())
                    .collect();
                if !paths.is_empty() {
                    coalescer.push_paths(paths);
                }
            }
            Err(err) => {
                eprintln!("[dv-host watch] worktree watch error (watch_id={watch_id}): {err}");
                coalescer.push_overflow();
            }
        });
    })
    .map_err(|err| SubscribeError::Io(format!("creating watcher: {err}")))?;
    watcher
        .watch(&root_path, RecursiveMode::Recursive)
        .map_err(|err| SubscribeError::Io(format!("watching {}: {err}", root_path.display())))?;
    Ok(watcher)
}

/// Per-subscription coalescing state: the first push after idle starts a
/// [`COALESCE_WINDOW`] timer thread; every push before that timer fires
/// just appends to the same pending batch (capped at
/// [`MAX_COALESCED_PATHS`], `overflow` set beyond it) instead of starting a
/// new one — so the window is anchored to the FIRST event of a burst, not
/// reset by each subsequent one (matches plan §6's "collect paths for
/// 200ms after first event", not a true debounce). See the `tests` module's
/// `coalesce` for a pure, offline equivalent of this same policy, kept for
/// unit testing without real sleeps.
struct Coalescer {
    watch_id: u64,
    kind: &'static str,
    stdout: Arc<Mutex<std::io::Stdout>>,
    buf: Mutex<CoalesceBuf>,
}

#[derive(Default)]
struct CoalesceBuf {
    paths: Vec<String>,
    overflow: bool,
    timer_active: bool,
}

impl Coalescer {
    fn new(watch_id: u64, kind: &'static str, stdout: Arc<Mutex<std::io::Stdout>>) -> Arc<Self> {
        Arc::new(Self {
            watch_id,
            kind,
            stdout,
            buf: Mutex::new(CoalesceBuf::default()),
        })
    }

    fn push_overflow(self: &Arc<Self>) {
        let mut buf = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        buf.overflow = true;
        self.ensure_timer(&mut buf);
    }

    fn push_paths(self: &Arc<Self>, paths: Vec<String>) {
        let mut buf = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        for path in paths {
            if buf.paths.len() < MAX_COALESCED_PATHS {
                buf.paths.push(path);
            } else {
                buf.overflow = true;
            }
        }
        self.ensure_timer(&mut buf);
    }

    /// Starts the drain-and-emit timer if one isn't already running for
    /// the current batch. Must be called with `buf` already locked (the
    /// caller's guard) so setting `timer_active = true` and spawning the
    /// thread are atomic with respect to a concurrent push from another
    /// notify callback invocation.
    fn ensure_timer(self: &Arc<Self>, buf: &mut CoalesceBuf) {
        if buf.timer_active {
            return;
        }
        buf.timer_active = true;
        let this = Arc::clone(self);
        // One short-lived thread per coalesce batch (bounded by how often a
        // burst starts from idle, not by event volume within it) — fine at
        // today's scale, but a very chatty repo with many active
        // subscriptions could mean real thread churn; S5 may pool these
        // (review finding P3-2).
        thread::spawn(move || {
            thread::sleep(coalesce_window());
            let (paths, overflow) = {
                let mut buf = this.buf.lock().unwrap_or_else(|e| e.into_inner());
                buf.timer_active = false;
                (
                    std::mem::take(&mut buf.paths),
                    std::mem::take(&mut buf.overflow),
                )
            };
            emit_event(&this.stdout, this.watch_id, this.kind, paths, overflow);
        });
    }
}

fn emit_event(
    stdout: &Arc<Mutex<std::io::Stdout>>,
    watch_id: u64,
    kind: &str,
    paths: Vec<String>,
    overflow: bool,
) {
    let notification = json!({
        "event": "watch/event",
        "params": {
            "watch_id": watch_id,
            "kind": kind,
            "paths": paths,
            "overflow": overflow,
        },
    });
    crate::write_line(stdout, &notification);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// One coalesced batch, as [`coalesce`] groups a pre-collected event
    /// stream — the pure, offline equivalent of what [`Coalescer`] computes
    /// online via a real timer thread (see that type's doc for why they're
    /// two separate implementations of the same rule rather than shared
    /// code: an offline batch-grouping function and an online
    /// first-event-anchored timer are structurally different shapes for
    /// the identical policy). Test-only: nothing in the live server calls
    /// this, it exists purely so the windowing/cap/overflow rules are
    /// unit-testable without a real 200ms sleep.
    #[derive(Debug, PartialEq)]
    struct CoalesceBatch {
        paths: Vec<String>,
        overflow: bool,
    }

    /// Group a chronological `(timestamp, path)` event stream into
    /// batches: each batch starts at its first unconsumed event's
    /// timestamp and absorbs every subsequent event within `window` of
    /// THAT start (not a rolling window — an event just past the deadline
    /// starts the NEXT batch rather than extending this one). Paths beyond
    /// `cap` within a single batch are dropped and that batch is flagged
    /// `overflow`.
    fn coalesce(events: &[(Instant, String)], window: Duration, cap: usize) -> Vec<CoalesceBatch> {
        let mut batches = Vec::new();
        let mut i = 0;
        while i < events.len() {
            let start = events[i].0;
            let mut paths = Vec::new();
            let mut overflow = false;
            let mut j = i;
            while j < events.len() && events[j].0.duration_since(start) <= window {
                if paths.len() < cap {
                    paths.push(events[j].1.clone());
                } else {
                    overflow = true;
                }
                j += 1;
            }
            batches.push(CoalesceBatch { paths, overflow });
            i = j;
        }
        batches
    }

    fn events_at(offsets_ms: &[u64]) -> Vec<(Instant, String)> {
        let t0 = Instant::now();
        offsets_ms
            .iter()
            .enumerate()
            .map(|(i, &ms)| (t0 + Duration::from_millis(ms), format!("path-{i}")))
            .collect()
    }

    #[test]
    fn coalesce_groups_events_within_the_window_into_one_batch() {
        let events = events_at(&[0, 50, 150, 199]);
        let batches = coalesce(&events, Duration::from_millis(200), 1000);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].paths.len(), 4);
        assert!(!batches[0].overflow);
    }

    #[test]
    fn coalesce_starts_a_new_batch_once_the_window_elapses() {
        // The window is anchored to the FIRST event of a batch — an event
        // 250ms after that first one is past the 200ms deadline (even
        // though it's within 200ms of the SECOND event) and must start its
        // own new batch, not extend the first.
        let events = events_at(&[0, 100, 250]);
        let batches = coalesce(&events, Duration::from_millis(200), 1000);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].paths.len(), 2); // t=0, t=100
        assert_eq!(batches[1].paths.len(), 1); // t=250
    }

    #[test]
    fn coalesce_flags_overflow_beyond_cap_but_keeps_the_capped_paths() {
        let events = events_at(&[0, 1, 2, 3, 4]);
        let batches = coalesce(&events, Duration::from_millis(200), 3);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].paths.len(), 3);
        assert!(batches[0].overflow);
    }

    #[test]
    fn coalesce_empty_input_is_no_batches() {
        let batches = coalesce(&[], Duration::from_millis(200), 1000);
        assert!(batches.is_empty());
    }

    #[test]
    fn coalesce_boundary_event_exactly_at_window_joins_the_same_batch() {
        let events = events_at(&[0, 200]);
        let batches = coalesce(&events, Duration::from_millis(200), 1000);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].paths.len(), 2);
    }

    #[test]
    fn subscribe_rejects_unknown_kind() {
        let stdout = Arc::new(Mutex::new(std::io::stdout()));
        let err = subscribe("/tmp/does-not-matter", "bogus", stdout)
            .expect_err("unknown kind must be rejected");
        match err {
            SubscribeError::BadRequest(message) => {
                assert!(message.contains("bogus"), "{message}");
            }
            SubscribeError::Io(message) => panic!("expected BadRequest, got Io({message})"),
        }
    }

    #[test]
    fn unsubscribe_of_unknown_id_is_a_harmless_no_op() {
        // Must not panic — matches the wire contract (idempotent, never a
        // wire error) documented on `unsubscribe`.
        unsubscribe(999_999_999);
    }

    // --- coalesce_window_given: env override/default matrix --------------

    #[test]
    fn coalesce_window_given_defaults_when_unset_or_unparseable() {
        assert_eq!(coalesce_window_given(None), COALESCE_WINDOW);
        assert_eq!(coalesce_window_given(Some("not-a-number")), COALESCE_WINDOW);
    }

    #[test]
    fn coalesce_window_given_honors_a_valid_override() {
        assert_eq!(
            coalesce_window_given(Some("3000")),
            Duration::from_millis(3000)
        );
    }

    // --- should_forward: inotify Access-event filter (review finding P1-2) -

    #[test]
    fn should_forward_drops_plain_access_open() {
        assert!(!should_forward(&EventKind::Access(AccessKind::Open(
            AccessMode::Read
        ))));
        assert!(!should_forward(&EventKind::Access(AccessKind::Open(
            AccessMode::Any
        ))));
    }

    #[test]
    fn should_forward_drops_access_close_read() {
        assert!(!should_forward(&EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));
    }

    #[test]
    fn should_forward_keeps_access_close_write() {
        // IN_CLOSE_WRITE — a genuine write completion, the one Access
        // sub-case that must survive the filter.
        assert!(should_forward(&EventKind::Access(AccessKind::Close(
            AccessMode::Write
        ))));
    }

    #[test]
    fn should_forward_keeps_modify_create_remove() {
        use notify::event::{CreateKind, ModifyKind, RemoveKind};
        assert!(should_forward(&EventKind::Modify(ModifyKind::Any)));
        assert!(should_forward(&EventKind::Create(CreateKind::Any)));
        assert!(should_forward(&EventKind::Remove(RemoveKind::Any)));
    }
}
